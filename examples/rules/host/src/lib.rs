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
//! [`cove_ir::lower::lower_entry`], [`cove_runtime::Vm`] or
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
//! **An entry takes process arguments and nothing else.** `run_entry` on
//! either backend takes a `Vec<Rc<str>>`, so a host cannot call an exported
//! Cove function with a Rust-built value. The pull request therefore cannot
//! be passed in: the host passes a *request identifier* as the one argument,
//! and the Cove program fetches the pull request back out through a Host API
//! call. That is a real constraint rather than a stylistic choice, and it is
//! also convenient here, because the Host API call is exactly the boundary
//! whose cost this example set out to measure. Issue #150 is the gap.
//!
//! **A host module the toolchain does not ship is invisible to `cove
//! check`.** `reviews` is this crate's, so `cove check` in `examples/`
//! reports one warning about `examples/rules/embedded/embedded.cove`, whose
//! help text says to hand the schema to the compiler. That is what
//! [`RulePackage::load`] does, and [`REVIEWS`] is the single value both the
//! checker and the boundary read, so the two cannot drift. That no `cove`
//! command can be handed one is issue #151.
//!
//! # The three things that are paid once
//!
//! Parsing and resolving and checking the package; lowering one entry to the
//! executable IR; and building the VM's shape and constant tables. All three
//! are [`RulePackage`] and [`Lowering`] and [`RulePackage::serve`], in that
//! order, and `cove-rules-measure` reports what each of them costs against
//! what one invocation costs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_diag::{render, SourceMap};
use cove_ir::lower::Lowered;
use cove_runtime::interp::Interpreter;
use cove_runtime::value::StructValue;
use cove_runtime::{
    Budget, Effect, FieldSchema, Grants, HostApi, HostRegistry, HostType, Limits, ModuleSchema,
    OperationSchema, RecordedValue, Runtime, RuntimeError, TraceEvent, TraceSink, TypeSchema,
    Value, Vm,
};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program;
use cove_sema::{Compiler, Config};

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
/// `PullRequest` carries ten fields and `labels` is an `Array<String>` rather
/// than a set, because [`HostType`] has no `Set`: the Cove side converts where
/// it asks a membership question. That is a limitation of the schema
/// vocabulary -- issue #153 -- and is recorded in `examples/rules/README.md`
/// rather than worked around silently.
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
                ty: HostType::Array(&HostType::String),
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
    /// This pull request as the struct value `reviews.PullRequest` names.
    ///
    /// Ten fields, and every one of them is an allocation: an `Rc<str>` for
    /// each name and each string, an `Rc<[Value]>` for each of the two
    /// arrays, a `Vec` for the field list, and an `Rc<StructValue>` around
    /// the whole. The measurement counts them rather than estimating them,
    /// because the point of counting is to find out whether the estimate was
    /// right.
    pub fn to_cove(&self) -> Value {
        Value::Struct(Rc::new(StructValue {
            type_name: "reviews.PullRequest".into(),
            fields: vec![
                ("id".into(), Value::Str(self.id.as_str().into())),
                ("title".into(), Value::Str(self.title.as_str().into())),
                ("author".into(), Value::Str(self.author.as_str().into())),
                (
                    "targetBranch".into(),
                    Value::Str(self.target_branch.as_str().into()),
                ),
                ("changedLines".into(), Value::Int(self.changed_lines)),
                ("filesTouched".into(), strings(&self.files_touched)),
                ("labels".into(), strings(&self.labels)),
                ("approvals".into(), Value::Int(self.approvals)),
                ("isDraft".into(), Value::Bool(self.is_draft)),
                ("hasTests".into(), Value::Bool(self.has_tests)),
            ],
            opaque: false,
        }))
    }
}

/// `texts` as the Cove `Array<String>` a schema's `Array(String)` admits.
fn strings(texts: &[String]) -> Value {
    Value::Array(
        texts
            .iter()
            .map(|t| Value::Str(t.as_str().into()))
            .collect(),
    )
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
    /// conversion. It is deliberately written the obvious way — match, read
    /// the field by name, clone the string — rather than the fast way, since
    /// what an embedder writes is what an embedder's cost is.
    pub fn from_cove(value: &Value) -> Result<Decision, String> {
        let decision = ok_payload(value)?;
        let Value::Struct(fields) = decision else {
            return Err(format!("expected a `Decision` struct, found {decision}"));
        };
        if &*fields.type_name != "rules.policy.Decision" {
            return Err(format!(
                "expected `rules.policy.Decision`, found `{}`",
                fields.type_name
            ));
        }
        Ok(Decision {
            policy: policy_of(field(fields, "policy")?)?,
            findings: findings_of(field(fields, "findings")?)?,
        })
    }
}

/// What an `Ok` carries, or a message saying what arrived instead.
fn ok_payload(value: &Value) -> Result<&Value, String> {
    let Value::Enum(result) = value else {
        return Err(format!("expected a `Result`, found {value}"));
    };
    match (&*result.type_name, &*result.case) {
        ("Result", "Ok") => Ok(&result.payload[0]),
        ("Result", "Err") => Err(format!("the rules answered `Err`: {}", result.payload[0])),
        (name, case) => Err(format!("expected a `Result`, found `{name}.{case}`")),
    }
}

/// One field of a struct value, or a message naming the field that was
/// missing and the type that should have carried it.
fn field<'v>(value: &'v StructValue, name: &str) -> Result<&'v Value, String> {
    value
        .get(name)
        .ok_or_else(|| format!("`{}` carries no field `{name}`", value.type_name))
}

/// A `rules.policy.ReviewPolicy` value as the Rust enum.
fn policy_of(value: &Value) -> Result<ReviewPolicy, String> {
    let Value::Enum(policy) = value else {
        return Err(format!("expected a `ReviewPolicy`, found {value}"));
    };
    match &*policy.case {
        "Normal" => Ok(ReviewPolicy::Normal),
        "Require" => {
            let Value::Struct(requirement) = &policy.payload[0] else {
                return Err("a `Require` carries a `Requirement`".to_string());
            };
            Ok(ReviewPolicy::Require {
                reviewers: int(field(requirement, "reviewers")?)?,
                reason: text(field(requirement, "reason")?)?,
            })
        }
        "Block" => Ok(ReviewPolicy::Block {
            reason: text(&policy.payload[0])?,
        }),
        case => Err(format!("`ReviewPolicy` has no case `{case}`")),
    }
}

/// An `Array<Finding>` as the Rust vector.
fn findings_of(value: &Value) -> Result<Vec<Finding>, String> {
    let Value::Array(items) = value else {
        return Err(format!("expected an `Array<Finding>`, found {value}"));
    };
    items
        .iter()
        .map(|item| {
            let Value::Struct(finding) = item else {
                return Err(format!("expected a `Finding`, found {item}"));
            };
            let Value::Enum(severity) = field(finding, "severity")? else {
                return Err("a `Finding` carries a `Severity`".to_string());
            };
            Ok(Finding {
                rule: text(field(finding, "rule")?)?,
                severity: severity.case.to_string(),
                reason: text(field(finding, "reason")?)?,
                reviewers: int(field(finding, "reviewers")?)?,
            })
        })
        .collect()
}

/// A `String` value as a Rust `String`.
fn text(value: &Value) -> Result<String, String> {
    match value {
        Value::Str(text) => Ok(text.to_string()),
        other => Err(format!("expected a `String`, found {other}")),
    }
}

/// An `Int` value as an `i64`.
fn int(value: &Value) -> Result<i64, String> {
    match value {
        Value::Int(number) => Ok(*number),
        other => Err(format!("expected an `Int`, found {other}")),
    }
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
                let [Value::Str(request)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                match self.fault {
                    Fault::WrongResultType => return Ok(Value::Int(7)),
                    Fault::Broken => {
                        return Err(RuntimeError::new(
                            "the review queue is unreachable".to_string(),
                        ))
                    }
                    Fault::None => {}
                }
                Ok(match self.open.lock().unwrap().get(&**request) {
                    Some(pr) => Value::ok(pr.to_cove()),
                    None => Value::err(Value::error(format!("no request named `{request}`"))),
                })
            }
            "record" => {
                let [Value::Str(request), Value::Str(policy), Value::Int(reviewers), Value::Str(trail)] =
                    args.as_slice()
                else {
                    unreachable!("checked by HostRegistry::call")
                };
                self.recorded.lock().unwrap().push(Recorded {
                    request: request.to_string(),
                    policy: policy.to_string(),
                    reviewers: *reviewers,
                    trail: trail.to_string(),
                });
                Ok(Value::ok(Value::Unit))
            }
            "blame" => Ok(Value::ok(Value::Str("nobody".into()))),
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

    /// Lowers one entry to the executable IR, and validates the result.
    ///
    /// Per entry rather than per package, because that is what
    /// `cove_ir::lower::lower_entry` does: it lowers what the entry can reach
    /// and nothing else. An embedder that invokes two entries lowers twice,
    /// once each, and holds both for the life of the process.
    pub fn lower(&self, module: &str, entry: &str) -> Result<Lowering, String> {
        let started = Instant::now();
        let ir: Lowered = cove_ir::lower::lower_entry(&self.program, module, entry)
            .map_err(|why| format!("the VM cannot run `{module}.{entry}`: {why}"))?;
        let lower = started.elapsed();

        let started = Instant::now();
        cove_ir::lower::validate(&ir.program)
            .map_err(|why| format!("the lowering of `{module}.{entry}` is not valid: {why}"))?;
        Ok(Lowering {
            functions: ir.program.functions.len(),
            ir: Arc::new(ir.program),
            lower,
            validate: started.elapsed(),
        })
    }

    /// Builds one backend over `hosts` and hands it to `body`.
    ///
    /// The one VM or interpreter `body` is given serves every invocation
    /// `body` makes, which is what compile-once/invoke-many means on this
    /// API. Building it is not free — a `Vm` reads the program's struct
    /// shapes, enum shapes, and constants as it is constructed — and
    /// `cove-rules-measure` reports that cost separately from an invocation's.
    ///
    /// The borrow is why this takes a closure rather than answering with a
    /// session. A `Vm` borrows the `Runtime` and the lowered program, both of
    /// which live for exactly as long as this call, and nothing Cove-shaped
    /// may leave it in any case: a `Value` is `Rc`-based and is not `Send`.
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
            Some(lowering) => Backend::Vm(Vm::new(&runtime, runtime.hosts(), &lowering.ir)),
            None => Backend::Ast(Interpreter::new(&runtime)),
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
    ir: Arc<cove_ir::Program>,
    /// How many functions the entry reached.
    pub functions: usize,
    /// Lowering itself.
    pub lower: Duration,
    /// Validating what it produced.
    pub validate: Duration,
}

/// Which backend a session runs on.
enum Backend<'a> {
    Ast(Interpreter<'a>),
    Vm(Vm<'a>),
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

    /// Invokes `module.entry` with `args`, and answers what it produced.
    ///
    /// The one seam. `run_entry` is what both backends are reached through
    /// and it takes the process arguments an entry may declare, so this is
    /// the whole of what an embedder can say to a Cove program.
    pub fn invoke(
        &mut self,
        module: &str,
        entry: &str,
        args: &[&str],
    ) -> Result<Value, RuntimeError> {
        let args: Vec<Rc<str>> = args.iter().map(|arg| (*arg).into()).collect();
        match &mut self.backend {
            Backend::Ast(interpreter) => interpreter.run_entry(module, entry, args),
            Backend::Vm(vm) => vm.run_entry(module, entry, args),
        }
    }

    /// Invokes `module.entry` and decodes what it answered as a [`Decision`].
    pub fn decide(&mut self, module: &str, entry: &str, request: &str) -> Result<Decision, String> {
        let value = self
            .invoke(module, entry, &[request])
            .map_err(|error| error.message)?;
        Decision::from_cove(&value)
    }

    /// How many VM instructions every invocation on this session has executed
    /// between them, or `None` on the interpreter, which counts none.
    pub fn instructions(&self) -> Option<u64> {
        match &self.backend {
            Backend::Ast(_) => None,
            Backend::Vm(vm) => Some(vm.instructions()),
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
/// The budget is installed once, on the registry, and that has a consequence
/// this example ran into and records rather than routes around. A `Budget` is
/// cumulative over the registry, `set_budget` needs `&mut HostRegistry`, and
/// a `Vm` holds the registry by shared reference for as long as it exists. So
/// a limit imposed here is a limit over *every* invocation the session makes,
/// and an embedder that wants a fuel limit per invocation has to build a new
/// registry, a new `Runtime` and a new backend for each one -- which is
/// exactly what compiling once and invoking many times is for not doing.
/// Issue #152 is the gap, and `examples/rules/README.md` says what it costs.
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
