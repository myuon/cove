//! A Rust application that embeds the `examples/rules` package as a typed,
//! bounded, inspectable decision engine.
//!
//! The Cove half of this example is a pull-request review policy: six rules,
//! a `dyn Rule` catalog, and one `decide` that turns a `PullRequest` into a
//! `Decision`. `examples/rules/README.md` describes it. This half is what an
//! embedder writes, and it exists because a rule engine has a shape nothing
//! else in this repository has: it is compiled once and invoked many times,
//! against inputs that arrive one at a time from the application around it.
//!
//! Everything here uses the public embedding API and nothing else:
//! [`cove_sema::Compiler`] with the embedder's own [`ModuleSchema`],
//! [`cove_lir::lower_entry`], [`cove_runtime::Lvm`] or
//! [`cove_runtime::interp::Interpreter`], [`cove_runtime::HostRegistry`],
//! [`cove_runtime::Grants`], [`cove_runtime::Budget`], and
//! [`cove_runtime::TraceSink`]. Nothing reaches into an internal module, and
//! nothing duplicates a checker or runtime table.
//!
//! # What the embedding is shaped by
//!
//! Two facts about the API decide the shape of everything below, and both are
//! worth stating plainly because they are not obvious from the outside.
//!
//! **An exported function is called with values, and an entry is called with
//! process arguments.** `Lvm::invoke` and `Interpreter::invoke` take a
//! `Vec<Value>`, so [`Session::evaluate`] hands `rules.embedded.evaluate` a
//! `rules.policy.PullRequest` the Rust side built and reads a
//! `rules.policy.Decision` out of what came back. Nothing crosses the Host
//! API boundary on that path at all. `run_entry` is still there and still
//! takes a `Vec<Rc<str>>`, because a *command* has strings to hand over —
//! [`Session::decide`] uses it, and what it costs against `evaluate` is the
//! measurement in `examples/rules/README.md`.
//!
//! This is the half of the example that changed. It was written when
//! `run_entry` was the only way in: the request identifier went in as the one
//! process argument and the pull request came back out through a Host API
//! call into this crate's own module, because there was no other channel.
//! Issue #150 was that gap. The `reviews` module below is not a casualty of
//! closing it — it is what the two paths are measured against each other
//! with, and it is still what a host module *is* for, which is reaching
//! something outside the process rather than carrying an argument into it.
//!
//! **A host module the toolchain does not ship is invisible to `cove
//! check`.** `reviews` is this crate's, so `cove check` in `examples/`
//! reports one warning about `examples/rules/embedded/embedded.cove`, whose
//! help text says to hand the schema to the compiler. That is what
//! [`RulePackage::load`] does, and [`REVIEWS`] is the single value both the
//! checker and the boundary read, so the two cannot drift. That no `cove`
//! command can be handed one is issue #151.
//!
//! # The two things that are paid once
//!
//! Parsing and resolving and checking the package, and lowering one entry to
//! `cove-lir`'s executable IR. The two are [`RulePackage`] and [`Lowering`],
//! in that order, and `cove-rules-measure` reports what each of them costs
//! against what one invocation costs. There used to be a third: the
//! predecessor backend read a lowered program's struct shapes, enum shapes
//! and constants at construction time and built a table of each, so
//! [`RulePackage::serve`] had a cost of its own worth reporting beside the
//! other two. `cove-lir` computes every layout once, while lowering, and
//! [`cove_runtime::Lvm::new`] reads none of that back out of the program — it
//! allocates the heap region and a table sized to the program's string count,
//! neither of which grows with how much the program declares. What
//! `RulePackage::serve` costs is still reported, because a reader should not
//! have to take "now cheap" on faith, but it is no longer a pass over the
//! program the way the other two are.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_diag::{render, SourceMap};
use cove_runtime::interp::Interpreter;
use cove_runtime::value::MapKey;
use cove_runtime::{
    Budget, Effect, FieldSchema, Grants, HostApi, HostRegistry, HostType, Limits, Lvm,
    ModuleSchema, OperationSchema, RecordedValue, Runtime, RuntimeError, TraceEvent, TraceSink,
    TypeSchema, Value,
};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program;
use cove_sema::{Compiler, Config, HostSchemas};

// ---------------------------------------------------------------------------
// The module this host registers

/// What `reviews` declares about itself.
///
/// One value, read twice: [`Reviews::module_schema`] answers with it, so the
/// boundary holds every call to it, and [`RulePackage::load`] hands the same
/// value to [`Compiler::with_host_schema`], so the checker holds every call
/// site to it. Nothing about the module is written down a second time, which
/// is the whole reason the two ends cannot disagree.
///
/// `PullRequest` carries ten fields and each is declared as the shape it is,
/// `labels: Set<String>` included. It was an `Array<String>` until issue #153,
/// not because a pull request's labels are a sequence but because [`HostType`]
/// had no `Set` to say otherwise, and the Cove side carried a loop that turned
/// one into the other wherever a rule asked a membership question. A schema
/// that cannot say what a field is makes the program say it instead.
///
/// Nothing here is checked by writing it down twice: the boundary holds a
/// value to this table, and
/// `the_schema_declares_only_types_a_value_could_have` holds the table itself
/// to `ModuleSchema::validate`, which is the one thing a `HostType` can now
/// say and no value satisfy.
pub const REVIEWS: ModuleSchema = ModuleSchema {
    name: "reviews",
    capability: "reviews",
    operations: &[
        OperationSchema {
            name: "pull",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::Named("reviews.PullRequest"), &HostType::Error),
            capability: "reviews",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "record",
            params: &[
                HostType::String,
                HostType::String,
                HostType::Int,
                HostType::String,
            ],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "reviews",
            effect: Effect::ReversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[TypeSchema {
        name: "PullRequest",
        cases: &[],
        fields: &[
            FieldSchema {
                name: "id",
                ty: HostType::String,
            },
            FieldSchema {
                name: "title",
                ty: HostType::String,
            },
            FieldSchema {
                name: "author",
                ty: HostType::String,
            },
            FieldSchema {
                name: "targetBranch",
                ty: HostType::String,
            },
            FieldSchema {
                name: "changedLines",
                ty: HostType::Int,
            },
            FieldSchema {
                name: "filesTouched",
                ty: HostType::Array(&HostType::String),
            },
            FieldSchema {
                name: "labels",
                ty: HostType::Set(&HostType::String),
            },
            FieldSchema {
                name: "approvals",
                ty: HostType::Int,
            },
            FieldSchema {
                name: "isDraft",
                ty: HostType::Bool,
            },
            FieldSchema {
                name: "hasTests",
                ty: HostType::Bool,
            },
        ],
    }],
    resources: &[],
};

/// The next version of [`REVIEWS`], with one operation and one field added
/// and nothing taken away.
///
/// An additive change: `blame` is an operation no rule package calls yet, and
/// `openedAt` is a field none reads. The compatibility test holds this to the
/// rule an additive change has to obey, which is that a package written
/// against the older schema goes on checking and running unchanged.
pub const REVIEWS_NEXT: ModuleSchema = ModuleSchema {
    name: "reviews",
    capability: "reviews",
    operations: &[
        REVIEWS.operations[0],
        REVIEWS.operations[1],
        OperationSchema {
            name: "blame",
            params: &[HostType::String, HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "reviews",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[TypeSchema {
        name: "PullRequest",
        cases: &[],
        fields: PULL_REQUEST_WITH_OPENED_AT,
    }],
    resources: &[],
};

/// [`REVIEWS_NEXT`]'s field list: every field [`REVIEWS`] declares, and one
/// more.
const PULL_REQUEST_WITH_OPENED_AT: &[FieldSchema] = &[
    REVIEWS.types[0].fields[0],
    REVIEWS.types[0].fields[1],
    REVIEWS.types[0].fields[2],
    REVIEWS.types[0].fields[3],
    REVIEWS.types[0].fields[4],
    REVIEWS.types[0].fields[5],
    REVIEWS.types[0].fields[6],
    REVIEWS.types[0].fields[7],
    REVIEWS.types[0].fields[8],
    REVIEWS.types[0].fields[9],
    FieldSchema {
        name: "openedAt",
        ty: HostType::Int,
    },
];

/// [`REVIEWS`] with `changedLines` renamed, which is the breaking change.
///
/// A field a caller reads is part of the interface whether or not anybody
/// wrote that down, so renaming one is a break. The compatibility test holds
/// this to the rule a breaking change has to obey, which is that the checker
/// says so, at the line that reads the field, before anything runs.
pub const REVIEWS_RENAMED: ModuleSchema = ModuleSchema {
    name: "reviews",
    capability: "reviews",
    operations: REVIEWS.operations,
    types: &[TypeSchema {
        name: "PullRequest",
        cases: &[],
        fields: PULL_REQUEST_RENAMED,
    }],
    resources: &[],
};

/// [`REVIEWS_RENAMED`]'s field list, in which `changedLines` is gone.
const PULL_REQUEST_RENAMED: &[FieldSchema] = &[
    REVIEWS.types[0].fields[0],
    REVIEWS.types[0].fields[1],
    REVIEWS.types[0].fields[2],
    REVIEWS.types[0].fields[3],
    FieldSchema {
        name: "changedLineCount",
        ty: HostType::Int,
    },
    REVIEWS.types[0].fields[5],
    REVIEWS.types[0].fields[6],
    REVIEWS.types[0].fields[7],
    REVIEWS.types[0].fields[8],
    REVIEWS.types[0].fields[9],
];

// ---------------------------------------------------------------------------
// The values that cross

/// A pull request, as the application around this embedding holds one.
///
/// A plain Rust struct with no Cove in it. [`PullRequest::to_cove`] is the
/// one place it becomes a value the boundary can carry, and that conversion
/// is what `cove-rules-measure` counts the allocations of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub id: String,
    pub title: String,
    pub author: String,
    pub target_branch: String,
    pub changed_lines: i64,
    pub files_touched: Vec<String>,
    pub labels: Vec<String>,
    pub approvals: i64,
    pub is_draft: bool,
    pub has_tests: bool,
}

impl PullRequest {
    /// This pull request as the struct value `reviews.PullRequest` names,
    /// which is what the Host API boundary carries.
    pub fn to_cove(&self) -> Value {
        Value::structure("reviews.PullRequest", self.fields())
    }

    /// The same ten fields as the struct value `rules.policy.PullRequest`
    /// names, which is what [`Session::evaluate`] hands the rules directly.
    ///
    /// The two are different types and always were: one is a
    /// `cove_schema::TypeSchema` this crate wrote in Rust and the other is a
    /// struct the rule package declared, and a Host API schema has no way to
    /// name the second. What is new is that a host may build the second —
    /// which is what makes `rules.embedded.pullRequest`'s field-by-field
    /// rebuild unnecessary on this path, and is where a chunk of what the
    /// boundary cost went.
    pub fn to_policy(&self) -> Value {
        Value::structure("rules.policy.PullRequest", self.fields())
    }

    /// The ten fields, in the order both declarations list them.
    ///
    /// Every one of them is an allocation: an `Rc<str>` for each name and
    /// each string, a shared slice for each of the two arrays, a vector for
    /// the field list, and one more for the struct. The measurement counts
    /// them rather than estimating them, because the point of counting is to
    /// find out whether the estimate was right.
    ///
    /// The *order* matters to exactly one of the two consumers. The boundary
    /// reads a `reviews.PullRequest`'s fields by name and does not care; the
    /// lowering reads a `rules.policy.PullRequest`'s by index, so a value
    /// whose fields are not the declaration's in the declaration's order is
    /// refused by `Lvm::invoke` before anything runs. That the two
    /// declarations list the same ten in the same order is a convenience
    /// rather than a rule, and it is what lets one list serve both.
    fn fields(&self) -> Vec<(&'static str, Value)> {
        vec![
            ("id", Value::string(self.id.as_str())),
            ("title", Value::string(self.title.as_str())),
            ("author", Value::string(self.author.as_str())),
            ("targetBranch", Value::string(self.target_branch.as_str())),
            ("changedLines", Value::int(self.changed_lines)),
            ("filesTouched", strings(&self.files_touched)),
            ("labels", label_set(&self.labels)),
            ("approvals", Value::int(self.approvals)),
            ("isDraft", Value::bool(self.is_draft)),
            ("hasTests", Value::bool(self.has_tests)),
        ]
    }
}

/// `texts` as the Cove `Array<String>` both declarations admit.
fn strings(texts: &[String]) -> Value {
    Value::array(texts.iter().map(|t| Value::string(t.as_str())))
}

/// `labels` as the Cove `Set<String>` both declarations admit.
///
/// A `Set`'s elements are `MapKey`s rather than `Value`s, which is Cove's
/// own restriction on what may be a key showing through: a host writes the key
/// it means. Nothing is walked or de-duplicated here that the set does not do
/// itself, which is the whole difference from what this used to be -- an
/// `Array<String>` the Cove side turned into a `Set` on every membership
/// question, because a Host API schema had no `Set` to declare one with.
fn label_set(labels: &[String]) -> Value {
    Value::set(labels.iter().map(|label| MapKey::Str(label.clone())))
}

/// What a review policy demands, as the application receives it.
///
/// The mirror of `rules.policy.ReviewPolicy`, and the type an embedder's
/// caller actually acts on: nothing downstream of [`Decision::from_cove`]
/// holds a [`Value`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewPolicy {
    /// Nothing beyond what the repository asks of every change.
    Normal,
    /// Reviewers, and the reason they are being asked for.
    Require { reviewers: i64, reason: String },
    /// The change may not land, and why.
    Block { reason: String },
}

/// One thing a rule noticed, as the application receives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub rule: String,
    pub severity: String,
    pub reason: String,
    pub reviewers: i64,
}

/// A policy and the findings that argued for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub policy: ReviewPolicy,
    pub findings: Vec<Finding>,
}

impl Decision {
    /// Reads a decision out of what an invocation answered.
    ///
    /// The entry is declared `Result<Decision, Error>`, so what arrives is a
    /// `Result` enum whose `Ok` payload is a `rules.policy.Decision` struct.
    /// Every step names what it expected, because a decoder that answers
    /// `None` tells its caller that something was wrong and not what.
    ///
    /// This walk is the outbound half of what the measurement attributes to
    /// conversion. It is deliberately written the obvious way — ask for the
    /// case, read the field by name, clone the string — rather than the fast
    /// way, since what an embedder writes is what an embedder's cost is.
    ///
    /// It asks through the readers on [`Value`] and matches on none of its
    /// variants, which is what issue #186 added them for: the shapes below
    /// are the ones a `rules.policy` declaration states, and nothing here
    /// says how the runtime holds one.
    pub fn from_cove(value: &Value) -> Result<Decision, String> {
        Decision::of(ok_payload(value)?)
    }

    /// Reads a decision out of a bare `rules.policy.Decision`.
    ///
    /// `rules.embedded.evaluate` is declared `-> Decision` rather than `->
    /// Result<Decision, Error>`, because nothing it does can fail: it takes
    /// the pull request as an argument instead of fetching it, and fetching
    /// it was the only fallible step. So the answer arrives unwrapped, and
    /// [`Decision::from_cove`] is this with the `Result` peeled off first.
    pub fn of(decision: &Value) -> Result<Decision, String> {
        let Some(type_name) = decision.declared_type() else {
            return Err(format!("expected a `Decision` struct, found {decision}"));
        };
        if type_name != "rules.policy.Decision" {
            return Err(format!(
                "expected `rules.policy.Decision`, found `{type_name}`"
            ));
        }
        Ok(Decision {
            policy: policy_of(field(decision, "policy")?)?,
            findings: findings_of(field(decision, "findings")?)?,
        })
    }
}

/// What an `Ok` carries, or a message saying what arrived instead.
///
/// `Result` is a builtin, so the readers for it are the four that predate
/// issue #186: a host asks "is this an `Ok`?" and gets the payload as the
/// answer, without stating the case names itself.
fn ok_payload(value: &Value) -> Result<&Value, String> {
    if let Some([payload]) = value.ok_payload() {
        return Ok(payload);
    }
    if let Some([error]) = value.err_payload() {
        return Err(format!("the rules answered `Err`: {error}"));
    }
    Err(format!("expected a `Result`, found {value}"))
}

/// One field of a struct value, or a message naming the field that was
/// missing and the type that should have carried it.
///
/// One message for both halves of what [`Value::field`] answers `None` to,
/// because the type name is what distinguishes them: a value that is not a
/// struct at all reports `Int` where a struct missing the field reports
/// `rules.policy.Decision`.
fn field<'v>(value: &'v Value, name: &str) -> Result<&'v Value, String> {
    value
        .field(name)
        .ok_or_else(|| format!("`{}` carries no field `{name}`", value.type_name()))
}

/// A `rules.policy.ReviewPolicy` value as the Rust enum.
fn policy_of(value: &Value) -> Result<ReviewPolicy, String> {
    let (Some(case), Some(payload)) = (value.case(), value.payload()) else {
        return Err(format!("expected a `ReviewPolicy`, found {value}"));
    };
    match case {
        "Normal" => Ok(ReviewPolicy::Normal),
        "Require" => {
            let requirement = &payload[0];
            Ok(ReviewPolicy::Require {
                reviewers: int(field(requirement, "reviewers")?)?,
                reason: text(field(requirement, "reason")?)?,
            })
        }
        "Block" => Ok(ReviewPolicy::Block {
            reason: text(&payload[0])?,
        }),
        case => Err(format!("`ReviewPolicy` has no case `{case}`")),
    }
}

/// An `Array<Finding>` as the Rust vector.
fn findings_of(value: &Value) -> Result<Vec<Finding>, String> {
    let Some(items) = value.items() else {
        return Err(format!("expected an `Array<Finding>`, found {value}"));
    };
    items
        .iter()
        .map(|finding| {
            // A struct is the one shape with fields, so `fields()` answering
            // is the question "is this a `Finding`?" asked without naming a
            // variant.
            if finding.fields().is_none() {
                return Err(format!("expected a `Finding`, found {finding}"));
            }
            let severity = field(finding, "severity")?;
            let Some(case) = severity.case() else {
                return Err("a `Finding` carries a `Severity`".to_string());
            };
            Ok(Finding {
                rule: text(field(finding, "rule")?)?,
                severity: case.to_string(),
                reason: text(field(finding, "reason")?)?,
                reviewers: int(field(finding, "reviewers")?)?,
            })
        })
        .collect()
}

/// A `String` value as a Rust `String`.
fn text(value: &Value) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("expected a `String`, found {value}"))
}

/// An `Int` value as an `i64`.
fn int(value: &Value) -> Result<i64, String> {
    value
        .as_int()
        .ok_or_else(|| format!("expected an `Int`, found {value}"))
}

// ---------------------------------------------------------------------------
// The host

/// What the host wrote down about one decision, under the request identifier
/// that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    pub request: String,
    pub policy: String,
    pub reviewers: i64,
    pub trail: String,
}

/// A way for a test to make the host misbehave, so that the boundary can be
/// seen holding it to its own schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Fault {
    /// The host answers what it declared.
    #[default]
    None,
    /// `pull` answers an `Int`, which its schema does not admit.
    WrongResultType,
    /// `pull` fails with a runtime error rather than with a Cove `Err`.
    Broken,
}

/// The embedder's own host module: the pull requests the application holds,
/// and the decisions it has been told about.
pub struct Reviews {
    /// The pull requests, by the request identifier that names each.
    ///
    /// Behind a `Mutex` because [`HostApi::call`] takes `&self` and a real
    /// application's queue is written to while the engine is reading it.
    open: Mutex<BTreeMap<String, PullRequest>>,
    /// Every decision the rules reported back, in order.
    recorded: Arc<Mutex<Vec<Recorded>>>,
    /// The schema this module answers with, which is [`REVIEWS`] unless a
    /// compatibility test asked for another.
    schema: ModuleSchema,
    /// How this host is asked to misbehave, if at all.
    fault: Fault,
}

impl Reviews {
    /// A host serving `open`, keyed by request identifier.
    pub fn new(open: BTreeMap<String, PullRequest>) -> Reviews {
        Reviews {
            open: Mutex::new(open),
            recorded: Arc::new(Mutex::new(Vec::new())),
            schema: REVIEWS,
            fault: Fault::None,
        }
    }

    /// The same host, declaring `schema` instead of [`REVIEWS`].
    pub fn with_schema(mut self, schema: ModuleSchema) -> Reviews {
        self.schema = schema;
        self
    }

    /// The same host, made to misbehave in the one named way.
    pub fn with_fault(mut self, fault: Fault) -> Reviews {
        self.fault = fault;
        self
    }

    /// The log every decision is recorded in, shared with the host so a
    /// caller can read it after the run.
    pub fn log(&self) -> Arc<Mutex<Vec<Recorded>>> {
        Arc::clone(&self.recorded)
    }

    /// `pr` as the `reviews.PullRequest` *this host's own* schema declares,
    /// which is [`PullRequest::to_cove`]'s ten fields plus whatever a newer
    /// schema added and `PullRequest` has no place to hold.
    ///
    /// `Lvm` materialises a Host API result into the full physical layout its
    /// schema declares the moment the call returns, rather than reading a
    /// field lazily the way the tree-walking interpreter's tagged value does
    /// — `docs/LINEAR_VM.md` is why every value has one fixed shape. So a
    /// host that declares [`REVIEWS_NEXT`] and answers only the ten fields
    /// [`REVIEWS`] always had is answering a value its own schema does not
    /// admit, whether or not the rule package reads the eleventh: the words
    /// for `openedAt` have to come from somewhere before the struct is a
    /// struct at all. This crate has no opening time to report, so it
    /// answers zero, which is a fixed answer good enough for a decision
    /// nothing in `examples/rules/embedded/embedded.cove` reads.
    fn answer(&self, pr: &PullRequest) -> Value {
        let mut fields = pr.fields();
        let declares_opened_at = self
            .schema
            .declared_type("PullRequest")
            .is_some_and(|declared| declared.fields.iter().any(|field| field.name == "openedAt"));
        if declares_opened_at {
            fields.push(("openedAt", Value::int(0)));
        }
        Value::structure("reviews.PullRequest", fields)
    }
}

impl HostApi for Reviews {
    fn module_schema(&self) -> ModuleSchema {
        self.schema
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // The boundary checked the operation, the arity, and every argument
        // type against the schema above before dispatching, so nothing here
        // restates any of it.
        match op {
            "pull" => {
                let [request] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                let Some(request) = request.as_str() else {
                    unreachable!("checked by HostRegistry::call")
                };
                match self.fault {
                    Fault::WrongResultType => return Ok(Value::int(7)),
                    Fault::Broken => {
                        return Err(RuntimeError::new(
                            "the review queue is unreachable".to_string(),
                        ))
                    }
                    Fault::None => {}
                }
                Ok(match self.open.lock().unwrap().get(request) {
                    Some(pr) => Value::ok(self.answer(pr)),
                    None => Value::err(Value::error(format!("no request named `{request}`"))),
                })
            }
            "record" => {
                let [request, policy, reviewers, trail] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                let (Some(request), Some(policy), Some(reviewers), Some(trail)) = (
                    request.as_str(),
                    policy.as_str(),
                    reviewers.as_int(),
                    trail.as_str(),
                ) else {
                    unreachable!("checked by HostRegistry::call")
                };
                self.recorded.lock().unwrap().push(Recorded {
                    request: request.to_string(),
                    policy: policy.to_string(),
                    reviewers,
                    trail: trail.to_string(),
                });
                Ok(Value::ok(Value::unit()))
            }
            "blame" => Ok(Value::ok(Value::string("nobody"))),
            other => unreachable!("`reviews` declares no operation `{other}`"),
        }
    }
}

// ---------------------------------------------------------------------------
// Compiling once

/// What loading and checking the rule package cost.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadCost {
    /// Reading every `.cove` file off disk.
    pub read: Duration,
    /// Parsing them.
    pub parse: Duration,
    /// Resolving and type-checking, which is where the schema is read.
    pub check: Duration,
    /// How many files were loaded.
    pub files: usize,
    /// How many modules they made.
    pub modules: usize,
}

/// A rule package, parsed and checked once.
///
/// This is the artefact an embedder holds for the life of the process. It
/// carries no host, no budget, and no backend: those belong to a run, and a
/// package outlives every run made from it.
pub struct RulePackage {
    sources: Arc<SourceMap>,
    program: Arc<Program>,
    /// The Host API schema this package was checked against, held so
    /// [`RulePackage::lower`] can hand `cove_lir::lower_entry` the same set
    /// [`Compiler::with_host_schema`] checked it against. The two must agree:
    /// a lowering that read a different set could build a `reviews.pull` call
    /// against a signature the checker never confirmed.
    schema: ModuleSchema,
    cost: LoadCost,
}

impl RulePackage {
    /// Loads, parses, and checks the rule package rooted at `root`, holding
    /// every call into `reviews` to `schema`.
    ///
    /// `root` is `examples/rules`. Every directory under it holding `.cove`
    /// files becomes one module, named by its path — `rules`, `rules.policy`,
    /// `rules.catalog` and so on — which is the rule
    /// `cove_sema::package::load` follows on disk. It is done here rather
    /// than by that function for one reason: an embedder composes a package
    /// out of the rules its user wrote, and it is entitled to decide what is
    /// in it.
    ///
    /// The schema goes to [`Compiler::with_host_schema`], so a call into
    /// `reviews` is checked at its call site, against the same table the
    /// boundary will hold it to.
    pub fn load(root: &Path, schema: ModuleSchema) -> Result<RulePackage, String> {
        let mut cost = LoadCost::default();

        let started = Instant::now();
        let mut files: Vec<(String, PathBuf, String)> = Vec::new();
        collect(root, root, &mut files)?;
        files.sort();
        cost.read = started.elapsed();
        cost.files = files.len();

        let started = Instant::now();
        let mut sources = SourceMap::new();
        let mut modules: BTreeMap<String, Module> = BTreeMap::new();
        for (name, path, text) in files {
            let file = sources.add(path.clone(), &text);
            let ast = cove_syntax::parse_file(&sources, file)
                .map_err(|items| report(&sources, &items))?;
            modules
                .entry(name.clone())
                .or_insert_with(|| Module {
                    name: name.clone(),
                    dir: path.parent().unwrap_or(root).to_path_buf(),
                    units: Vec::new(),
                })
                .units
                .push(Unit { file, path, ast });
        }
        cost.parse = started.elapsed();
        cost.modules = modules.len();

        let package = Package {
            root: root.to_path_buf(),
            config: Config::default(),
            modules,
        };
        let started = Instant::now();
        let program = Compiler::new()
            .with_host_schema(schema)
            .compile(&package)
            .map_err(|items| report(&sources, &items))?;
        cost.check = started.elapsed();

        Ok(RulePackage {
            sources: Arc::new(sources),
            program: Arc::new(program),
            schema,
            cost,
        })
    }

    /// What loading this package cost.
    pub fn cost(&self) -> LoadCost {
        self.cost
    }

    /// Whatever the checker accepted but doubted, rendered.
    ///
    /// Empty for a package checked against a schema that describes every host
    /// module it names, which is what an embedder should expect to see.
    pub fn notices(&self) -> Vec<String> {
        self.program
            .notices
            .iter()
            .map(|item| render(&self.sources, item))
            .collect()
    }

    /// Lowers one entry to `cove-lir`'s executable IR.
    ///
    /// Per entry rather than per package, because that is what
    /// [`cove_lir::lower_entry`] does: it lowers what the entry can reach and
    /// nothing else. An embedder that invokes two entries lowers twice, once
    /// each, and holds both for the life of the process.
    ///
    /// There is no separate validation step to time. The predecessor lowered
    /// to a form a second pass then checked; `cove_lir::lower_entry` verifies
    /// as it goes and answers a lowering that is already known good or a
    /// `Vec<Diagnostic>` naming what was wrong, so [`Lowering`] has one
    /// duration where it used to have two.
    ///
    /// The schema handed to [`cove_lir::lower_entry`] is [`RulePackage::load`]'s
    /// own — the one [`Compiler::with_host_schema`] checked this package
    /// against — because a `reviews.pull` call has to lower against the same
    /// signature the checker confirmed it against, or the two could disagree
    /// about what the boundary looks like.
    pub fn lower(&self, module: &str, entry: &str) -> Result<Lowering, String> {
        let started = Instant::now();
        let schemas = HostSchemas::new().with(self.schema);
        let program = cove_lir::lower_entry(&self.program, &self.sources, &schemas, module, entry)
            .map_err(|items| {
                format!(
                    "`{module}.{entry}` does not lower: {}",
                    report(&self.sources, &items)
                )
            })?;
        let lower = started.elapsed();

        Ok(Lowering {
            functions: program.functions.len(),
            ir: Arc::new(program),
            lower,
        })
    }

    /// Builds one backend over `hosts` and hands it to `body`.
    ///
    /// The one `Lvm` or interpreter `body` is given serves every invocation
    /// `body` makes, which is what compile-once/invoke-many means on this
    /// API. `cove-rules-measure` reports what building it costs separately
    /// from an invocation's, though for `Lvm` that cost is no longer a pass
    /// over the program: `cove_lir::lower_entry` computed every layout while
    /// [`RulePackage::lower`] ran, so [`cove_runtime::Lvm::new`] allocates the
    /// heap region and a table sized to the program's string count and reads
    /// nothing else back out of the lowered program. The predecessor backend
    /// read the program's struct shapes, enum shapes and constants at this
    /// point and built a table of each, which is what made building *it* a
    /// cost worth reporting in the first place.
    ///
    /// The borrow is why this takes a closure rather than answering with a
    /// session. An `Lvm` borrows the `Runtime` and the lowered program, both
    /// of which live for exactly as long as this call, and nothing
    /// Cove-shaped may leave it in any case: a `Value` is `Rc`-based and is
    /// not `Send`.
    pub fn serve<T>(
        &self,
        hosts: Arc<HostRegistry>,
        lowering: Option<&Lowering>,
        body: impl FnOnce(&mut Session<'_>) -> T,
    ) -> T {
        let runtime = Runtime::new(
            Arc::clone(&self.program),
            Arc::clone(&self.sources),
            Arc::clone(&hosts),
        );
        let started = Instant::now();
        let backend = match lowering {
            Some(lowering) => {
                Backend::Lvm(Box::new(Lvm::new(&runtime, runtime.hosts(), &lowering.ir)))
            }
            None => Backend::Ast(Box::new(Interpreter::new(&runtime))),
        };
        let mut session = Session {
            build: started.elapsed(),
            backend,
        };
        body(&mut session)
    }
}

/// Every `.cove` file under `dir`, with the module name its directory gives
/// it and the text it holds.
fn collect(
    root: &Path,
    dir: &Path,
    into: &mut Vec<(String, PathBuf, String)>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read `{}`: {e}", dir.display()))?;
    let mut subdirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read `{}`: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.extension().and_then(|e| e.to_str()) == Some("cove") {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
            into.push((module_name(root, dir), path, text));
        }
    }
    subdirs.sort();
    for subdir in subdirs {
        collect(root, &subdir, into)?;
    }
    Ok(())
}

/// The module a directory holds: `rules` for the root, and the path below it
/// joined with dots for anything deeper.
fn module_name(root: &Path, dir: &Path) -> String {
    let mut parts = vec!["rules".to_string()];
    if let Ok(rest) = dir.strip_prefix(root) {
        parts.extend(
            rest.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned()),
        );
    }
    parts.join(".")
}

/// Diagnostics, rendered the way `cove check` renders them.
fn report(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
    items.iter().map(|item| render(sources, item)).collect()
}

/// What lowering one entry cost, and what it produced.
pub struct Lowering {
    ir: Arc<cove_lir::Program>,
    /// How many functions the entry reached.
    pub functions: usize,
    /// Lowering itself, verification included.
    ///
    /// The predecessor backend timed lowering and validating separately,
    /// because they were two passes over two different representations.
    /// `cove_lir::lower_entry` verifies as it lowers rather than after, so
    /// there is one duration rather than two.
    pub lower: Duration,
}

/// Which backend a session runs on.
///
/// Both are boxed, which is not a statement about either: an `Interpreter`
/// and an `Lvm` are each several hundred bytes of stacks and tables, so an
/// enum holding either inline is as wide as the wider one wherever it is
/// passed. A session is built once per run and invoked many times, so the
/// indirection costs nothing that anything here measures. Nothing else about
/// the shape follows from it — the arms below deref through the box and read
/// the way they read.
enum Backend<'a> {
    Ast(Box<Interpreter<'a>>),
    Lvm(Box<Lvm<'a>>),
}

/// One backend, ready to be invoked as many times as an embedder likes.
pub struct Session<'a> {
    backend: Backend<'a>,
    /// What building the backend cost.
    build: Duration,
}

impl Session<'_> {
    /// What building this session's backend cost.
    pub fn build_time(&self) -> Duration {
        self.build
    }

    /// Calls `module.entry` with the values `args`, and answers what it
    /// produced.
    ///
    /// The seam an application uses. `invoke` holds `args` to the signature
    /// the checker resolved for `module.entry` — the parameter count, and
    /// each value against its declared type, followed into a declared
    /// struct's fields — and refuses before the first instruction if they do
    /// not match. Both backends answer it the same way; nothing here knows
    /// which one is underneath.
    pub fn invoke(
        &mut self,
        module: &str,
        entry: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        match &mut self.backend {
            Backend::Ast(interpreter) => interpreter.invoke(module, entry, args),
            Backend::Lvm(lvm) => lvm.invoke(module, entry, args),
        }
    }

    /// Runs `module.entry` as a command would, handing it `args` as the
    /// process arguments an entry may declare.
    ///
    /// The other seam, and the only one there used to be. It is what
    /// [`Session::decide`] uses to reach `rules.embedded.decideRequest`,
    /// which is the control the Host API boundary is measured with.
    pub fn run(&mut self, module: &str, entry: &str, args: &[&str]) -> Result<Value, RuntimeError> {
        let args: Vec<Rc<str>> = args.iter().map(|arg| (*arg).into()).collect();
        match &mut self.backend {
            Backend::Ast(interpreter) => interpreter.run_entry(module, entry, args),
            Backend::Lvm(lvm) => lvm.run_entry(module, entry, args),
        }
    }

    /// Decides `pr` by invoking `module.entry` with it, and reads the
    /// [`Decision`] back.
    ///
    /// This is what an application calls once per request. Nothing crosses
    /// the Host API boundary: the pull request goes in as an argument and the
    /// decision comes back as a result, so a run of it makes no host call at
    /// all and needs no capability.
    pub fn evaluate(
        &mut self,
        module: &str,
        entry: &str,
        pr: &PullRequest,
    ) -> Result<Decision, String> {
        let value = self
            .invoke(module, entry, vec![pr.to_policy()])
            .map_err(|error| error.message)?;
        Decision::of(&value)
    }

    /// Decides `pr` the same way, bounded by `limits` for this request and no
    /// other.
    ///
    /// This is what an application that runs somebody else's rules calls. A
    /// rule package is not the application's code: it can loop, and an
    /// application would rather be told which request went wrong than stop
    /// serving. `invoke_within` installs a `Budget` built from `limits` as the
    /// invocation is entered, so the fuel, the deadline and the host-call
    /// limit are this request's -- and the next request gets its own, on the
    /// same `Lvm`, with none of the 168 allocations that rebuilding a backend
    /// per request costs.
    ///
    /// The deadline runs from here rather than from wherever the `Limits` were
    /// written, which is what makes a per-request deadline mean the request.
    ///
    /// A failure comes back as the message it came back as; nothing about a
    /// stopped invocation damages the session, exactly as for a host that
    /// failed.
    pub fn evaluate_within(
        &mut self,
        limits: Limits,
        module: &str,
        entry: &str,
        pr: &PullRequest,
    ) -> Result<Decision, String> {
        let value = match &mut self.backend {
            Backend::Ast(interpreter) => {
                interpreter.invoke_within(Budget::new(limits), module, entry, vec![pr.to_policy()])
            }
            Backend::Lvm(lvm) => {
                lvm.invoke_within(Budget::new(limits), module, entry, vec![pr.to_policy()])
            }
        }
        .map_err(|error| error.message)?;
        Decision::of(&value)
    }

    /// The same decision the other way: `module.entry` is run with the
    /// request identifier as its one process argument, fetches the pull
    /// request through `reviews.pull`, and reports the answer through
    /// `reviews.record`.
    pub fn decide(&mut self, module: &str, entry: &str, request: &str) -> Result<Decision, String> {
        let value = self
            .run(module, entry, &[request])
            .map_err(|error| error.message)?;
        Decision::from_cove(&value)
    }

    /// The boundary route, bounded by `limits` for this request and no other.
    ///
    /// The counterpart of [`Session::evaluate_within`] for the way in that
    /// makes host calls, and the reason it is here as well as that one: ADR
    /// 0024 says `max_host_calls` is the control that bounds *effects*
    /// exactly, where fuel bounds work only to within a straight line. An
    /// application that wants to cap what one request may do to the outside
    /// world sets it, and until issue #152 it could only set it for the life
    /// of the session.
    pub fn decide_within(
        &mut self,
        limits: Limits,
        module: &str,
        entry: &str,
        request: &str,
    ) -> Result<Decision, String> {
        let args: Vec<Rc<str>> = vec![request.into()];
        let value = match &mut self.backend {
            Backend::Ast(interpreter) => {
                interpreter.run_entry_within(Budget::new(limits), module, entry, args)
            }
            Backend::Lvm(lvm) => lvm.run_entry_within(Budget::new(limits), module, entry, args),
        }
        .map_err(|error| error.message)?;
        Decision::from_cove(&value)
    }

    /// How many instructions every invocation on this session has executed
    /// between them, or `None` on the interpreter, which counts none.
    pub fn instructions(&self) -> Option<u64> {
        match &self.backend {
            Backend::Ast(_) => None,
            Backend::Lvm(lvm) => Some(lvm.instructions()),
        }
    }
}

// ---------------------------------------------------------------------------
// Watching the boundary

/// A trace sink that keeps the host calls a run made, in order.
///
/// What it is for is the deliverable that says every invocation and trace is
/// linked to an application request identifier. Both `reviews` operations take
/// that identifier as their first argument, so every `HostCall` event carries
/// it, and [`Calls::for_request`] is the query that shows it.
#[derive(Default)]
pub struct Calls(Mutex<Vec<RecordedCall>>);

/// One recorded host call: the module, the operation, and the first argument
/// when it was a string, which for `reviews` is the request identifier.
pub type RecordedCall = (String, String, Option<String>);

impl Calls {
    /// Every call recorded, as `module`, `op`, and the first argument when it
    /// was a string.
    pub fn all(&self) -> Vec<RecordedCall> {
        self.0.lock().unwrap().clone()
    }

    /// The operations recorded under `request`, in order.
    pub fn for_request(&self, request: &str) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|(_, _, first)| first.as_deref() == Some(request))
            .map(|(module, op, _)| format!("{module}.{op}"))
            .collect()
    }
}

impl TraceSink for Calls {
    fn record(&self, event: TraceEvent) {
        if let TraceEvent::HostCall {
            module, op, args, ..
        } = event
        {
            let first = match args.first() {
                Some(RecordedValue::Carried(cove_runtime::Transfer::Str(text))) => {
                    Some(text.clone())
                }
                _ => None,
            };
            self.0.lock().unwrap().push((module, op, first));
        }
    }
}

// ---------------------------------------------------------------------------
// Setting one up

/// Everything an embedding holds beside the compiled package: the registry a
/// run calls through, the log the host writes decisions to, and the trace it
/// records host calls in.
pub struct Embedding {
    /// The registry, ready to be handed to [`RulePackage::serve`].
    pub hosts: Arc<HostRegistry>,
    /// What the rules have reported back, in order.
    pub log: Arc<Mutex<Vec<Recorded>>>,
    /// Every host call the run made, with the request that made it.
    pub calls: Arc<Calls>,
}

/// Registers `reviews`, grants `grants`, imposes `limits`, and watches the
/// boundary.
///
/// Registering a module does not grant it: a capability missing from `grants`
/// makes every call into the module a refusal, which is one of the cases the
/// tests beside this file pin.
///
/// `limits` is what bounds the *session*: it is installed on the registry
/// before anything runs, and every invocation the session makes spends out of
/// the one budget. That is what an application wants for the limits that are
/// about the process rather than about a request.
///
/// It is no longer the only choice, which it was when this example was
/// written. A limit that belongs to one request is what a rule engine actually
/// wants -- a rule package is somebody else's code, and an application running
/// one wants to be told when a rule loops rather than to stop serving -- and
/// [`Session::evaluate_within`] is that: the same compiled package, the same
/// `Lvm`, and a `Budget` per invocation. Issue #152 was the gap, and
/// `examples/rules/README.md` says what it used to cost.
///
/// Pass [`Limits::default`] here for a session that is bounded per request and
/// not otherwise, which is what the cases beside this file do.
pub fn embedding(reviews: Reviews, grants: &[&str], limits: Limits) -> Embedding {
    embedding_traced(reviews, grants, limits, true)
}

/// The same, with the trace sink left off.
///
/// A sink that is going to read an event needs the event to be complete, so
/// `HostRegistry` describes every argument and every result a call carried
/// whenever one is installed -- a deep copy of each, per call. A registry with
/// no sink installed keeps the `NullSink` it was built with, which answers
/// `is_recording()` with `false`, and the description is skipped.
///
/// That difference is worth a whole embedding of its own because it is what
/// tracing costs, and `cove-rules-measure` prints the two rows beside each
/// other. It is not a small number when the value being described is a
/// ten-field struct carrying two arrays.
pub fn embedding_without_trace(reviews: Reviews, grants: &[&str], limits: Limits) -> Embedding {
    embedding_traced(reviews, grants, limits, false)
}

/// Both of the above.
fn embedding_traced(reviews: Reviews, grants: &[&str], limits: Limits, trace: bool) -> Embedding {
    let log = reviews.log();
    let calls = Arc::new(Calls::default());
    let mut hosts = HostRegistry::new(Grants::new(grants.to_vec()));
    hosts.register(Box::new(reviews));
    hosts.set_budget(Budget::new(limits));
    if trace {
        hosts.set_trace(Arc::clone(&calls) as Arc<dyn TraceSink>);
    }
    Embedding {
        hosts: Arc::new(hosts),
        log,
        calls,
    }
}

/// The six pull requests this example is demonstrated on, by request
/// identifier.
///
/// The same six `rules.fixtures` declares in Cove, written again in Rust
/// because in a real embedding they arrive from the application. That they
/// are written twice is a hazard, so a test asserts the two agree: the
/// decision reached over the host's copy of a pull request and the decision
/// reached over the package's own have to be the same decision.
pub fn samples() -> BTreeMap<String, PullRequest> {
    let mut open = BTreeMap::new();
    for (request, pr) in [
        ("req-1", clean()),
        ("req-2", large()),
        ("req-3", guarded()),
        ("req-4", waived()),
        ("req-5", draft()),
        ("req-6", labelled()),
    ] {
        open.insert(request.to_string(), pr);
    }
    open
}

/// A small change with tests, on an ordinary branch.
fn clean() -> PullRequest {
    PullRequest {
        id: "pr-1001".to_string(),
        title: "Correct a typo in the changelog".to_string(),
        author: "ada".to_string(),
        target_branch: "main".to_string(),
        changed_lines: 4,
        files_touched: vec!["CHANGELOG.md".to_string()],
        labels: vec!["docs".to_string()],
        approvals: 0,
        is_draft: false,
        has_tests: true,
    }
}

/// A change over the size threshold, with tests.
fn large() -> PullRequest {
    PullRequest {
        id: "pr-1002".to_string(),
        title: "Rewrite the scheduler".to_string(),
        author: "grace".to_string(),
        target_branch: "main".to_string(),
        changed_lines: 2400,
        files_touched: vec!["src/scheduler.rs".to_string(), "src/queue.rs".to_string()],
        labels: Vec::new(),
        approvals: 1,
        is_draft: false,
        has_tests: true,
    }
}

/// A change that reaches a guarded directory without a waiver.
fn guarded() -> PullRequest {
    PullRequest {
        id: "pr-1003".to_string(),
        title: "Rotate the signing key".to_string(),
        author: "linus".to_string(),
        target_branch: "main".to_string(),
        changed_lines: 30,
        files_touched: vec!["auth/keys.yaml".to_string()],
        labels: Vec::new(),
        approvals: 0,
        is_draft: false,
        has_tests: false,
    }
}

/// The same guarded directory, with the waiver label.
fn waived() -> PullRequest {
    PullRequest {
        id: "pr-1004".to_string(),
        title: "Rotate the signing key, reviewed".to_string(),
        author: "linus".to_string(),
        target_branch: "main".to_string(),
        changed_lines: 30,
        files_touched: vec!["auth/keys.yaml".to_string()],
        labels: vec!["security-reviewed".to_string()],
        approvals: 2,
        is_draft: false,
        has_tests: false,
    }
}

/// A draft nobody has asked for review on yet.
fn draft() -> PullRequest {
    PullRequest {
        id: "pr-1005".to_string(),
        title: "Sketch a cache".to_string(),
        author: "ada".to_string(),
        target_branch: "main".to_string(),
        changed_lines: 12,
        files_touched: vec!["src/cache.rs".to_string()],
        labels: Vec::new(),
        approvals: 0,
        is_draft: true,
        has_tests: false,
    }
}

/// A change aimed at the protected branch, carrying the heaviest label.
fn labelled() -> PullRequest {
    PullRequest {
        id: "pr-1006".to_string(),
        title: "Drop the v1 wire format".to_string(),
        author: "grace".to_string(),
        target_branch: "release".to_string(),
        changed_lines: 300,
        files_touched: vec!["src/wire.rs".to_string(), "docs/wire.md".to_string()],
        labels: vec!["breaking-change".to_string(), "migration".to_string()],
        approvals: 0,
        is_draft: false,
        has_tests: true,
    }
}

/// Where the rule package lives, relative to this crate.
///
/// A path rather than `include_str!`, because the sources an embedder
/// compiles are its user's and arrive when the process starts. A binary that
/// carried them inside itself would be `cove build`, which is a different
/// thing with a different ADR.
pub fn package_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the host crate sits inside the rule package")
        .to_path_buf()
}
