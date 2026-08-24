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
//! Serialization and resource ownership are left out: every value that
//! crosses the boundary today is an ordinary [`crate::value::Value`], no
//! shipped operation hands back a live resource handle, and a field nothing
//! consults is a claim nothing checks.

use std::fmt;

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
    /// `String`.
    String,
    /// `Duration`, a signed count of nanoseconds.
    Duration,
    /// `Error`, the builtin error struct.
    Error,
    /// `Option<T>`.
    Option(&'static HostType),
    /// `Result<T, E>`. Expected failure is part of an operation's result
    /// type, exactly as it is in Cove source, rather than a second channel
    /// beside it.
    Result(&'static HostType, &'static HostType),
}

impl fmt::Display for HostType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostType::Unit => f.write_str("Unit"),
            HostType::String => f.write_str("String"),
            HostType::Duration => f.write_str("Duration"),
            HostType::Error => f.write_str("Error"),
            HostType::Option(inner) => write!(f, "Option<{inner}>"),
            HostType::Result(ok, error) => write!(f, "Result<{ok}, {error}>"),
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
/// checks `name` and arity before it dispatches, `params` and `result` render
/// the signature a diagnostic shows, `capability` is the gate, and
/// `result_is_task_safe` answers the Language Card's rule for values leaving
/// a host call for a task. `effect`, `cancellable`, and `recordable` are the
/// three ADR 0001 facts whose consumers are named but not yet built —
/// `cove replay` for `recordable`, ADR 0003 phase 2 cancellation for
/// `cancellable`, and `cove impact` for `effect`.
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
    /// without calling the host again, which is what `cove replay` needs. An
    /// operation that returns a live resource handle rather than a plain
    /// value is not recordable.
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(HostType::Duration.to_string(), "Duration");
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
