//! The machine-readable Host API schema.
//!
//! ADR 0001 states what this module has to carry:
//!
//! > A machine-readable Host API schema is shared by the compiler, runtime,
//! > and CLI. Each operation describes its argument, result, and error types;
//! > capability; serialization and resource ownership; cancellation and
//! > recordability; and whether it is a read, reversible write, or
//! > irreversible write.
//!
//! and the Language Card adds the sentence that makes the schema load-bearing
//! for tasks: "Host resources declare task-safety in their Host API schema."
//!
//! A schema entry is Rust data, not a parsed declaration, because a host is
//! written in Rust: [`crate::host::HostApi::schema`] returns a `'static`
//! table that a module declares alongside its implementation, so the two
//! cannot drift apart at run time.
//!
//! Only the parts of ADR 0001's list that something reads are modelled here.
//! Serialization is left out: every value that crosses the boundary is an
//! ordinary [`crate::value::Value`], and a field nothing consults is a claim
//! nothing checks.
//!
//! Resource ownership is no longer among the omissions. A host may declare
//! types of its own — [`TypeSchema`] for the ones that are plain data, and
//! [`ResourceSchema`] for the ones the host keeps on the far side of the
//! boundary — and ADR 0013 makes the second of those the whole of a resource
//! handle's contract: which operations it answers, what capability each of
//! them needs, and whether the handle may cross a task boundary.

use std::fmt;

use crate::value::Value;

/// A type in a Host API signature, written in Cove's source vocabulary.
///
/// This is a small enum rather than [`cove_syntax::ast::Type`] because an
/// `ast::Type` is a *parsed* type: every node carries a span into a source
/// file and its path segments are identifiers with spans of their own, all of
/// which would have to be invented for an operation that has no source to
/// point at. The rendering vocabulary is the same one — [`fmt::Display`]
/// produces the form the type would be written in Cove — so a signature
/// printed from a schema entry reads exactly like a signature written by
/// hand.
///
/// The variants cover exactly the types the shipped hosts use. Add one when a
/// host needs it; an unused variant is a type nobody can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostType {
    /// `Unit`, the value an operation returns when it returns nothing.
    Unit,
    /// `Bool`.
    Bool,
    /// `Int`, a signed 64-bit integer.
    Int,
    /// `String`.
    String,
    /// `Duration`, a signed count of nanoseconds.
    Duration,
    /// `Error`, the builtin error struct.
    Error,
    /// `Array<T>`, the fixed-length immutable sequence.
    Array(&'static HostType),
    /// `Option<T>`.
    Option(&'static HostType),
    /// `Result<T, E>`. Expected failure is part of an operation's result
    /// type, exactly as it is in Cove source, rather than a second channel
    /// beside it.
    Result(&'static HostType, &'static HostType),
    /// A type the host declares, written qualified: `http.Response`.
    ///
    /// The name is the one Cove source writes, module included, because that
    /// is what a signature in a diagnostic has to read as. Whether it names a
    /// [`TypeSchema`] or a [`ResourceSchema`] is the host's business; a
    /// signature says only which type it is.
    Named(&'static str),
    /// Any value at all.
    ///
    /// This is not a missing type: it is the type of an operation whose
    /// meaning does not depend on which value it was given. `http.json`
    /// renders whatever it is handed, and a callback a host stores and calls
    /// later is a value the host never looks inside.
    Any,
}

impl fmt::Display for HostType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostType::Unit => f.write_str("Unit"),
            HostType::Bool => f.write_str("Bool"),
            HostType::Int => f.write_str("Int"),
            HostType::String => f.write_str("String"),
            HostType::Duration => f.write_str("Duration"),
            HostType::Error => f.write_str("Error"),
            HostType::Array(inner) => write!(f, "Array<{inner}>"),
            HostType::Option(inner) => write!(f, "Option<{inner}>"),
            HostType::Result(ok, error) => write!(f, "Result<{ok}, {error}>"),
            HostType::Named(name) => f.write_str(name),
            HostType::Any => f.write_str("Any"),
        }
    }
}

impl HostType {
    /// Whether `value` is one this type admits, and where it stops being one
    /// when it is not.
    ///
    /// The check follows this type's own recursion rather than looking only
    /// at the outermost constructor, because a shallow check would admit an
    /// `Array<Int>` where an `Array<String>` was declared and the schema says
    /// more than "an array". [`HostType::Any`] admits everything, which is
    /// not a hole: it is the type of an operation whose meaning does not
    /// depend on which value it was given — the work `clock.timeout` bounds,
    /// the body `clock.every` repeats — so there is nothing there to check.
    ///
    /// [`HostType::Named`] is checked by name. Every value carries the
    /// qualified type it was built with and a [`crate::host::ResourceHandle`]
    /// carries the module and kind it was issued for, so the comparison is
    /// one the value itself can answer, with no registry to consult and no
    /// second lookup on a path every host call takes. What that leaves
    /// unchecked is a [`TypeSchema`]'s *fields*: a value calling itself an
    /// `http.Response` is taken at its word about what is inside it. See ADR
    /// 0013's amendment for why that line is drawn there.
    pub fn admits(&self, value: &Value) -> Result<(), Mismatch> {
        match (self, value) {
            (HostType::Any, _)
            | (HostType::Unit, Value::Unit)
            | (HostType::Bool, Value::Bool(_))
            | (HostType::Int, Value::Int(_))
            | (HostType::String, Value::Str(_))
            | (HostType::Duration, Value::Duration(_)) => Ok(()),
            // The builtin error struct, which is what `Err` carries
            // everywhere a host declares one.
            (HostType::Error, Value::Struct(fields)) if &*fields.type_name == "Error" => Ok(()),
            (HostType::Array(item), Value::Array(items)) => {
                for (index, element) in items.iter().enumerate() {
                    item.admits(element)
                        .map_err(|mismatch| mismatch.inside(&format!("[{index}]")))?;
                }
                Ok(())
            }
            (HostType::Option(some), Value::Enum(case)) if &*case.type_name == "Option" => {
                match (&*case.case, case.payload.as_slice()) {
                    ("Some", [inner]) => some.admits(inner).map_err(|m| m.inside("Some(_)")),
                    ("None", []) => Ok(()),
                    _ => Err(self.mismatched(value)),
                }
            }
            (HostType::Result(ok, error), Value::Enum(case)) if &*case.type_name == "Result" => {
                match (&*case.case, case.payload.as_slice()) {
                    ("Ok", [inner]) => ok.admits(inner).map_err(|m| m.inside("Ok(_)")),
                    ("Err", [inner]) => error.admits(inner).map_err(|m| m.inside("Err(_)")),
                    _ => Err(self.mismatched(value)),
                }
            }
            // A handle names its module and its kind, which together are the
            // qualified name a signature writes.
            (HostType::Named(name), Value::Resource(handle))
                if handle.qualified_type() == *name =>
            {
                Ok(())
            }
            (HostType::Named(name), Value::Struct(fields)) if &*fields.type_name == *name => Ok(()),
            (HostType::Named(name), Value::Enum(case)) if &*case.type_name == *name => Ok(()),
            _ => Err(self.mismatched(value)),
        }
    }

    /// This type and a value that is none of it, as a mismatch at the point
    /// the two part company.
    fn mismatched(&self, value: &Value) -> Mismatch {
        Mismatch {
            path: String::new(),
            expected: *self,
            found: value.type_name(),
        }
    }
}

/// Where a value stopped agreeing with the type declared for it, and what was
/// found there instead.
///
/// The disagreement is reported where it happens rather than at the top of
/// the value: an operation declaring `Result<Array<String>, Error>` that
/// answers `Ok([3])` disagrees at one element, and saying which one is the
/// difference between a diagnostic the host's author can act on and one that
/// says only that the two do not match. Nothing here is built until a value
/// fails, so the path costs a run that never fails nothing at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// How the offending part is reached from the whole value, such as
    /// `Ok(_)[0]`. Empty when the value itself, rather than something nested
    /// inside it, is the disagreement.
    pub path: String,
    /// The type declared at that point.
    pub expected: HostType,
    /// What was found there, named the way a diagnostic names a value.
    pub found: String,
}

impl Mismatch {
    /// Re-anchors this mismatch one level out, where the part that disagrees
    /// is reached by `step`.
    fn inside(mut self, step: &str) -> Mismatch {
        self.path.insert_str(0, step);
        self
    }

    /// The disagreement, phrased for a diagnostic about the operation
    /// `shown` names.
    pub fn describe(&self, shown: &str) -> String {
        if self.path.is_empty() {
            format!(
                "`{shown}` answered `{}`, but its schema declares `{}`",
                self.found, self.expected
            )
        } else {
            format!(
                "`{shown}` answered `{}` at `{}` of its result, but its schema declares `{}` there",
                self.found, self.path, self.expected
            )
        }
    }
}

/// Whether an operation observes the world or changes it, and whether the
/// change can be taken back.
///
/// ADR 0001 asks each operation to say which of the three it is. The
/// distinction is about the world outside the run, not about the host's own
/// bookkeeping: waiting is a [`Effect::Read`] because nothing outside the run
/// is different afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Observes state without changing it.
    Read,
    /// Changes state in a way the same host can put back.
    ReversibleWrite,
    /// Changes state nothing can put back, such as bytes already on a
    /// terminal or a message already sent.
    IrreversibleWrite,
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Effect::Read => f.write_str("read"),
            Effect::ReversibleWrite => f.write_str("reversible write"),
            Effect::IrreversibleWrite => f.write_str("irreversible write"),
        }
    }
}

/// One operation of one host module.
///
/// Every field is read by something: [`crate::host::HostRegistry::call`]
/// checks `name` and arity before it dispatches and holds the host to
/// `result` after it answers, `params` and `result` render the signature a
/// diagnostic shows, `capability` is the gate, and
/// `result_is_task_safe` answers the Language Card's rule for values leaving
/// a host call for a task. `effect`, `cancellable`, and `recordable` are the
/// three ADR 0001 facts whose consumers are named but not yet built —
/// `cove replay` for `recordable`, `cove impact` for `effect`, and for
/// `cancellable`, a host that could abandon a call in flight: a cancelled
/// task stops at its next safepoint, which is after the call it is already
/// inside returns.
#[derive(Clone, Copy, Debug)]
pub struct OperationSchema {
    /// The name Cove source calls, such as `println`.
    pub name: &'static str,
    /// Parameter types in declaration order.
    pub params: &'static [HostType],
    /// Whether the last parameter is variadic.
    ///
    /// Cove writes a variadic parameter `items: T...` and makes it an
    /// immutable `Array<T>` inside the callee, so it accepts zero or more
    /// arguments: a variadic operation's minimum arity is one less than
    /// `params.len()`.
    pub variadic: bool,
    /// The type the operation produces.
    ///
    /// This is a promise the boundary holds the host to rather than a label
    /// on it: [`crate::host::HostRegistry`] checks what the host answered
    /// against this before handing it on, so an operation cannot declare one
    /// type and produce another. [`HostType::admits`] says how far the check
    /// goes.
    pub result: HostType,
    /// The capability a host must grant before this operation may be called.
    pub capability: &'static str,
    /// Whether the operation reads, writes reversibly, or writes
    /// irreversibly.
    pub effect: Effect,
    /// Whether abandoning a call that is already in flight is meaningful and
    /// safe. A wait can be abandoned because nothing has happened yet; a
    /// write that has already reached the outside world cannot.
    pub cancellable: bool,
    /// Whether the call's result can be recorded and handed back later
    /// without calling the host again, which is what `cove replay` needs.
    ///
    /// An operation that opens a resource is recordable, because ADR 0013
    /// makes a handle a name rather than a live thing: what the trace records
    /// is the identity the host issued, and a replay hands the same identity
    /// back and answers the calls made on it from the trace too.
    pub recordable: bool,
    /// Whether the value this operation produces may cross a task boundary.
    ///
    /// The Language Card puts this decision here rather than in the value:
    /// "Host resources declare task-safety in their Host API schema."
    pub result_is_task_safe: bool,
}

impl OperationSchema {
    /// The fewest arguments the operation accepts.
    pub fn min_arity(&self) -> usize {
        if self.variadic {
            self.params.len().saturating_sub(1)
        } else {
            self.params.len()
        }
    }

    /// Whether a call with `arity` arguments has the right number of them.
    pub fn accepts(&self, arity: usize) -> bool {
        if self.variadic {
            arity >= self.min_arity()
        } else {
            arity == self.params.len()
        }
    }

    /// The signature, in the form it would be written in Cove source, without
    /// the module qualifier: `println(String...) -> Result<Unit, Error>`.
    pub fn signature(&self) -> String {
        let mut params = self
            .params
            .iter()
            .map(HostType::to_string)
            .collect::<Vec<_>>();
        if self.variadic {
            if let Some(last) = params.last_mut() {
                last.push_str("...");
            }
        }
        format!("{}({}) -> {}", self.name, params.join(", "), self.result)
    }

    /// How many arguments this operation takes, phrased for a diagnostic.
    pub fn expected_arity(&self) -> String {
        let least = self.min_arity();
        let noun = if least == 1 { "argument" } else { "arguments" };
        if self.variadic {
            format!("at least {least} {noun}")
        } else {
            format!("{least} {noun}")
        }
    }
}

/// One field of a host type.
#[derive(Clone, Copy, Debug)]
pub struct FieldSchema {
    /// The label Cove source writes in the initializer, such as `method`.
    pub name: &'static str,
    /// The field's type.
    pub ty: HostType,
}

/// The shape of one type a host declares.
///
/// A host type is ordinary data: `http.Method` is an enum whose cases carry
/// nothing, and `http.Route` is a struct initialized with labels, exactly as a
/// Cove struct is. Neither needs a representation of its own — the runtime
/// builds a [`crate::value::Value::Enum`] or a
/// [`crate::value::Value::Struct`] whose type name is qualified by the module
/// — so what the schema adds is only the vocabulary: which names exist and
/// what they are made of.
///
/// A type whose values the host keeps rather than hands over is a
/// [`ResourceSchema`] instead.
#[derive(Clone, Copy, Debug)]
pub struct TypeSchema {
    /// The name Cove source writes after the module, such as `Route`.
    pub name: &'static str,
    /// The cases, for an enum. Empty for a struct.
    pub cases: &'static [&'static str],
    /// The fields, for a struct. Empty for an enum.
    pub fields: &'static [FieldSchema],
}

impl TypeSchema {
    /// Whether this is an enum, which is what having cases means.
    pub fn is_enum(&self) -> bool {
        !self.cases.is_empty()
    }

    /// The initializer, in the form it would be written in Cove source:
    /// `Route(method: http.Method, path: String, handler: Any)`.
    pub fn initializer(&self) -> String {
        let fields = self
            .fields
            .iter()
            .map(|field| format!("{}: {}", field.name, field.ty))
            .collect::<Vec<_>>();
        format!("{}({})", self.name, fields.join(", "))
    }
}

/// One kind of host resource: a value the host owns and Cove only names.
///
/// ADR 0013 states the contract this carries. A handle is an identity, not
/// state: the host keeps whatever a `database.Connection` really is, and Cove
/// holds the name of it. So a resource declares three things and nothing
/// else — what it is called, which operations it answers, and whether the
/// name may cross a task boundary.
///
/// `task_safe` is the Language Card's sentence applied to a host's own types:
/// "Host resources declare task-safety in their Host API schema." A resource
/// whose state the host keeps behind a lock says `true`, and its name then
/// crosses like any other immutable value; one whose state belongs to the
/// task that opened it says `false`, and the name is refused at the boundary
/// with the same diagnostic a vector gets.
#[derive(Clone, Copy, Debug)]
pub struct ResourceSchema {
    /// The name Cove source writes after the module, such as `Connection`.
    pub name: &'static str,
    /// Whether a handle to this resource may cross a task boundary.
    pub task_safe: bool,
    /// The operations a handle answers, called as methods on it.
    pub operations: &'static [OperationSchema],
}

impl ResourceSchema {
    /// The operation `name`, if this resource has one.
    pub fn operation(&self, name: &str) -> Option<&OperationSchema> {
        self.operations.iter().find(|entry| entry.name == name)
    }

    /// Every operation name, for a diagnostic that has to list them.
    pub fn operation_names(&self) -> Vec<String> {
        self.operations
            .iter()
            .map(|entry| format!("`{}`", entry.name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ResourceHandle;
    use crate::value::StructValue;

    const READ_A_STRING: OperationSchema = OperationSchema {
        name: "read",
        params: &[HostType::String],
        variadic: false,
        result: HostType::Result(&HostType::String, &HostType::Error),
        capability: "documents",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    };

    const PRINT_MANY: OperationSchema = OperationSchema {
        name: "println",
        params: &[HostType::String],
        variadic: true,
        result: HostType::Result(&HostType::Unit, &HostType::Error),
        capability: "console",
        effect: Effect::IrreversibleWrite,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    };

    #[test]
    fn types_render_in_cove_source_form() {
        assert_eq!(HostType::Unit.to_string(), "Unit");
        assert_eq!(HostType::Bool.to_string(), "Bool");
        assert_eq!(HostType::Int.to_string(), "Int");
        assert_eq!(HostType::Duration.to_string(), "Duration");
        assert_eq!(
            HostType::Array(&HostType::String).to_string(),
            "Array<String>"
        );
        assert_eq!(
            HostType::Option(&HostType::String).to_string(),
            "Option<String>"
        );
        assert_eq!(
            HostType::Result(&HostType::Unit, &HostType::Error).to_string(),
            "Result<Unit, Error>"
        );
    }

    #[test]
    fn a_fixed_operation_accepts_exactly_its_parameters() {
        assert!(!READ_A_STRING.accepts(0));
        assert!(READ_A_STRING.accepts(1));
        assert!(!READ_A_STRING.accepts(2));
        assert_eq!(READ_A_STRING.min_arity(), 1);
        assert_eq!(READ_A_STRING.expected_arity(), "1 argument");
    }

    #[test]
    fn a_variadic_operation_accepts_zero_or_more() {
        assert!(PRINT_MANY.accepts(0));
        assert!(PRINT_MANY.accepts(1));
        assert!(PRINT_MANY.accepts(7));
        assert_eq!(PRINT_MANY.min_arity(), 0);
        assert_eq!(PRINT_MANY.expected_arity(), "at least 0 arguments");
    }

    // ------------------------------------------- what a declared type admits
    //
    // ADR 0001 asks each operation to describe its argument, result, and
    // error types, and ADR 0013's amendment makes the result one the boundary
    // holds a host to. These pin the vocabulary of that check; `host.rs` pins
    // what the boundary does with it.

    /// The one kind of resource a host in these tests can open.
    static CONNECTION: ResourceSchema = ResourceSchema {
        name: "Connection",
        task_safe: true,
        operations: &[],
    };

    /// A struct value named the way a host builds one: qualified by module.
    fn host_struct(type_name: &str, fields: Vec<(&str, Value)>) -> Value {
        Value::Struct(Box::new(StructValue {
            type_name: type_name.into(),
            fields: fields
                .into_iter()
                .map(|(name, value)| (name.into(), value))
                .collect(),
        }))
    }

    #[test]
    fn a_declared_type_admits_the_value_it_names() {
        assert!(HostType::Unit.admits(&Value::Unit).is_ok());
        assert!(HostType::Bool.admits(&Value::Bool(true)).is_ok());
        assert!(HostType::Int.admits(&Value::Int(3)).is_ok());
        assert!(HostType::String.admits(&Value::Str("text".into())).is_ok());
        assert!(HostType::Duration.admits(&Value::Duration(500)).is_ok());
        assert!(HostType::Error.admits(&Value::error("gone")).is_ok());
    }

    #[test]
    fn a_value_of_another_type_is_a_mismatch_where_the_two_part_company() {
        let mismatch = HostType::String
            .admits(&Value::Int(3))
            .expect_err("an `Int` is not a `String`");

        assert_eq!(mismatch.path, "");
        assert_eq!(mismatch.expected, HostType::String);
        assert_eq!(mismatch.found, "Int");
        assert_eq!(
            mismatch.describe("wayward.read"),
            "`wayward.read` answered `Int`, but its schema declares `String`"
        );
    }

    #[test]
    fn a_declared_type_is_followed_all_the_way_down() {
        let declared = HostType::Result(&HostType::Array(&HostType::String), &HostType::Error);

        assert!(declared
            .admits(&Value::ok(Value::Array(
                vec![Value::Str("one".into())].into()
            )))
            .is_ok());
        assert!(
            declared.admits(&Value::err(Value::error("gone"))).is_ok(),
            "the declared error type is the one inside `Err`"
        );

        let mismatch = declared
            .admits(&Value::ok(Value::Array(
                vec![Value::Str("one".into()), Value::Int(2)].into(),
            )))
            .expect_err("an `Int` among the declared strings is not admitted");
        assert_eq!(mismatch.path, "Ok(_)[1]");
        assert_eq!(mismatch.expected, HostType::String);
        assert_eq!(
            mismatch.describe("wayward.list"),
            "`wayward.list` answered `Int` at `Ok(_)[1]` of its result, but its schema declares `String` there"
        );
    }

    #[test]
    fn an_option_is_admitted_by_either_case() {
        let declared = HostType::Option(&HostType::String);

        assert!(declared.admits(&Value::none()).is_ok());
        assert!(declared
            .admits(&Value::some(Value::Str("set".into())))
            .is_ok());
        assert_eq!(
            declared
                .admits(&Value::some(Value::Int(3)))
                .expect_err("`Some(3)` is not an `Option<String>`")
                .path,
            "Some(_)"
        );
    }

    #[test]
    fn any_admits_whatever_it_is_given() {
        assert!(HostType::Any.admits(&Value::Int(3)).is_ok());
        assert!(HostType::Any.admits(&Value::Unit).is_ok());
        assert!(HostType::Any
            .admits(&host_struct("demo.Point", Vec::new()))
            .is_ok());
        assert!(HostType::Array(&HostType::Any)
            .admits(&Value::Array(vec![Value::Int(1), Value::Unit].into()))
            .is_ok());
    }

    #[test]
    fn a_named_type_is_checked_by_the_name_the_value_carries() {
        let declared = HostType::Named("http.Response");
        let response = host_struct(
            "http.Response",
            vec![
                ("status", Value::Int(200)),
                ("body", Value::Str("ok".into())),
            ],
        );
        assert!(declared.admits(&response).is_ok());

        // The name is what is checked; the fields behind it are not. A
        // `Response` with nothing inside it is still a `Response` here.
        assert!(declared
            .admits(&host_struct("http.Response", Vec::new()))
            .is_ok());

        assert_eq!(
            declared
                .admits(&host_struct("demo.Point", Vec::new()))
                .expect_err("another struct is not an `http.Response`")
                .found,
            "demo.Point"
        );
    }

    #[test]
    fn a_named_resource_is_checked_by_the_kind_the_handle_was_issued_for() {
        let declared = HostType::Named("database.Connection");
        let handle = Value::Resource(ResourceHandle::new("database", &CONNECTION, 7));
        assert!(declared.admits(&handle).is_ok());

        let elsewhere = Value::Resource(ResourceHandle::new("http", &CONNECTION, 7));
        assert_eq!(
            declared
                .admits(&elsewhere)
                .expect_err("the same kind of another module is another type")
                .found,
            "http.Connection"
        );
    }

    #[test]
    fn signatures_read_like_source() {
        assert_eq!(
            READ_A_STRING.signature(),
            "read(String) -> Result<String, Error>"
        );
        assert_eq!(
            PRINT_MANY.signature(),
            "println(String...) -> Result<Unit, Error>"
        );
    }
}
