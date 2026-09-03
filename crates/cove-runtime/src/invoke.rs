//! Holding a host's arguments to the declaration it is calling.
//!
//! [`Interpreter::invoke`](crate::interp::Interpreter::invoke) and
//! [`Vm::invoke`](crate::Vm::invoke) let a host call an exported function
//! with values it built, which
//! [`run_entry`](crate::interp::Interpreter::run_entry) cannot: that one takes
//! the process arguments an entry may declare, so what a host could say to a
//! Cove program was a list of strings and nothing else (issue #150).
//!
//! A host call into a Cove *host module* is checked against that module's
//! [`ModuleSchema`](cove_schema::ModuleSchema) by
//! [`HostRegistry::call`](crate::host::HostRegistry::call). An invocation has
//! no schema, because the thing being called is not a host module — it is a
//! declaration of the checked package. What it has instead is better: the
//! checker resolved that declaration's parameter and return types while it
//! walked the program, and published them on
//! [`Facts`](cove_sema::facts::Facts). So this module holds an invocation to
//! the same declaration the program was checked against, and a host that gets
//! it wrong is told so in the checker's own words, at the declaration's span,
//! before the first instruction runs.
//!
//! # Why the check is not optional
//!
//! It would be tempting to let the backend discover a wrong argument on its
//! own. It can, now: `cove_ir::lower` spends the checker's answer when it
//! chooses each parameter's `Repr`, and the boundary that turns a `Value`
//! into words for the call already refuses one of the wrong shape before the
//! first instruction runs. But it refuses from inside its own reading of one
//! word at a time, so what an unchecked caller would see is the function's
//! declaration span and a message naming the word kind that was wanted —
//! nothing about which parameter, and nothing in the checker's own words. The
//! check here is what a caller sees instead.
//!
//! # What is checked, and what is not
//!
//! The shape a host may call at all — no type parameters, no `var`, no
//! variadic — is read off the declaration, so it is refused identically on
//! both backends and before either is entered. Arity is the declared parameter
//! count, exactly: a parameter with a default is still a parameter here,
//! because a default is an expression the *checker* evaluates at a call site
//! it walked, and a host is not one.
//!
//! Types are checked against [`Signature`], following the declared type's own
//! recursion, exactly as [`crate::schema::Admits`] follows a `HostType`'s: a
//! shallow check would admit an `Array<Int>` where an `Array<String>` was
//! declared. A type this package *declares* is followed further than the Host
//! API boundary follows one — into a struct's fields and an enum case's
//! payload — and [`admits`] says why that is not inconsistency but the same
//! rule applied to a different fact about how each is read.
//!
//! [`Ty::Unknown`] admits everything. That is not a hole either: it is the
//! checker saying it settled nothing about this position — a host module no
//! schema describes, most often — and there is nothing to hold a value to.
//!
//! What is *not* checked is a capability. An invocation grants nothing: what
//! the called function may reach is what the [`crate::HostRegistry`] it runs
//! against was granted, and a call into a host module is refused there, on the
//! instruction that makes it, exactly as it is for a run `cove run` started.

use std::fmt::Write as _;
use std::sync::Arc;

use cove_diag::Span;
use cove_schema::builtins::{
    ERROR, ERR_CASE, MAP_ENTRY, NONE_CASE, OK_CASE, OPTION, RESULT, SOME_CASE,
};
use cove_sema::facts::Signature;
use cove_sema::resolve::Program;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{FnDecl, GenericParam};

use crate::error::RuntimeError;
use crate::value::{EnumValue, Repr, StructValue, Value};

/// The rule an invocation keeps, quoted on every refusal so a host author
/// reads the same sentence whichever way they broke it.
const AN_INVOCATION_KEEPS_THE_DECLARATION: &str =
    "A host invoking an exported function supplies exactly the parameters it declares, and a value of each declared type.";

/// Holds `args` to what the checker resolved about `module.name`.
///
/// Answers `Ok(())` when the call may proceed and a [`RuntimeError`] naming
/// what is wrong when it may not. Nothing here runs anything, so a refusal
/// costs a run no side effect at all — which is the same promise ADR 0019
/// makes about a lowering that fails.
pub(crate) fn check(
    program: &Program,
    module: &str,
    name: &str,
    args: &[Value],
) -> Result<(), RuntimeError> {
    let Some(entry) = program.lookup_fn(module, name) else {
        return Err(RuntimeError::new(format!(
            "this package does not declare `{module}.{name}`"
        )));
    };
    let decl = &entry.decl;
    let shown = format!("{module}.{name}");

    callable_shape(decl, &shown)?;

    if args.len() != decl.params.len() {
        return Err(RuntimeError::new(format!(
            "`{shown}` takes {}, but {} {} given",
            parameters(decl.params.len()),
            args.len(),
            if args.len() == 1 { "was" } else { "were" }
        ))
        .at(decl.span)
        .with_rule(AN_INVOCATION_KEEPS_THE_DECLARATION)
        .with_help(defaulted_help(decl)));
    }

    // The declaration's parameter *types* are the checker's answer rather than
    // the source's, so they come from the facts table and not from
    // `decl.params[i].ty`: `-> module.Thing` written in one module and read in
    // another is a name only the checker resolved. `cove_ir::lower` reads the
    // same table through the same key.
    let Some(signature) = program.facts.signature(decl.span.file, decl.span) else {
        return Err(RuntimeError::new(format!(
            "`{shown}` cannot be invoked: this program was resolved but not checked, so nothing recorded what its parameters are"
        ))
        .at(decl.span)
        .with_rule(AN_INVOCATION_KEEPS_THE_DECLARATION)
        .with_help(
            "build the program with `cove_sema::Compiler::new().compile(..)` rather than with `cove_sema::resolve::resolve(..)`",
        ));
    };

    for (position, (ty, value)) in signature.params.iter().zip(args).enumerate() {
        if let Err(mismatch) = admits(program, ty, value, module) {
            return Err(RuntimeError::new(mismatch.describe(&shown, position + 1))
                .at(param_span(decl, position))
                .with_rule(AN_INVOCATION_KEEPS_THE_DECLARATION)
                .with_help(format!("`{shown}` declares {}", written(signature))));
        }
    }
    Ok(())
}

/// Refuses the three declaration shapes a host cannot call, whatever values it
/// brought.
///
/// Each is refused here, from the declaration, rather than by a backend,
/// because the two backends would otherwise answer differently: the
/// linear-memory backend places one value per lowered slot and the
/// interpreter binds parameters by name, so a defaulted or variadic call
/// means something to one of them and nothing to the other. ADR 0019 asks
/// that anything both backends answer be answered once.
fn callable_shape(decl: &FnDecl, shown: &str) -> Result<(), RuntimeError> {
    if let Some(generic) = decl.generics.first() {
        return Err(RuntimeError::new(format!(
            "`{shown}` declares the type parameter `{}`, which an invocation cannot settle",
            generic.name.node
        ))
        .at(generic.name.span)
        .with_rule(AN_INVOCATION_KEEPS_THE_DECLARATION)
        .with_help(
            "a host supplies values, not types; invoke a function whose parameters are written out",
        ));
    }
    for param in &decl.params {
        if param.is_var {
            return Err(RuntimeError::new(format!(
                "`{shown}` declares `var {}`, which an invocation cannot supply",
                param.name.node
            ))
            .at(param.span)
            .with_rule(AN_INVOCATION_KEEPS_THE_DECLARATION)
            .with_help(
                "a `var` parameter aliases a place in the caller's frame, and a host has no frame; take the value and answer the new one",
            ));
        }
        if param.variadic {
            return Err(RuntimeError::new(format!(
                "`{shown}` declares the variadic parameter `{}`, which an invocation cannot supply",
                param.name.node
            ))
            .at(param.span)
            .with_rule(AN_INVOCATION_KEEPS_THE_DECLARATION)
            .with_help(format!(
                "declare `{}: Array<..>` instead, and hand it one array",
                param.name.node
            )));
        }
    }
    Ok(())
}

/// `1 parameter` or `n parameters`, so an arity refusal reads as a sentence.
fn parameters(count: usize) -> String {
    if count == 1 {
        "1 parameter".to_string()
    } else {
        format!("{count} parameters")
    }
}

/// The help an arity refusal carries, which says the one thing about arity
/// that is not obvious: a default does not make a parameter optional here.
fn defaulted_help(decl: &FnDecl) -> String {
    let names: Vec<String> = decl
        .params
        .iter()
        .filter(|param| param.default.is_some())
        .map(|param| format!("`{}`", param.name.node))
        .collect();
    if names.is_empty() {
        "supply one value for each declared parameter".to_string()
    } else {
        format!(
            "{} {} a default, which a call site may omit and an invocation may not: supply one value for each declared parameter",
            names.join(", "),
            if names.len() == 1 { "has" } else { "have" }
        )
    }
}

/// Where a refused argument's parameter was written, so a diagnostic points at
/// the declaration rather than at the whole function.
fn param_span(decl: &FnDecl, position: usize) -> Span {
    decl.params
        .get(position)
        .map_or(decl.span, |param| param.span)
}

/// The signature as a reader would write it, for the help line.
fn written(signature: &Signature) -> String {
    let mut text = String::from("fn(");
    for (index, ty) in signature.params.iter().enumerate() {
        if index > 0 {
            text.push_str(", ");
        }
        let _ = write!(text, "{ty}");
    }
    text.push(')');
    if signature.ret != Ty::Unit {
        let _ = write!(text, " -> {}", signature.ret);
    }
    text
}

/// Where a value stopped agreeing with the type declared for it.
///
/// The twin of [`crate::schema::Mismatch`], which asks the same question of a
/// `HostType`. They are separate types rather than one generic one because
/// the two describe different things: a `HostType` is what an embedder wrote
/// in Rust about its own module, and a [`Ty`] is what the checker worked out
/// about this package. Only the phrasing is shared, and it is shared by being
/// written to read the same way.
enum Wrong {
    /// A value of a type the declared one does not admit.
    Type {
        /// How the offending part is reached from the whole argument, such as
        /// `Ok(_)[1]`. Empty when the argument itself is the disagreement.
        path: String,
        /// The declared type at that point, spelled as the checker spells it.
        expected: String,
        /// What was found there, named the way a diagnostic names a value.
        found: String,
    },
    /// A struct of the declared type, carrying fields the declaration does
    /// not.
    ///
    /// Its own case rather than one more `Type`, because the type is right
    /// and the thing that is wrong has no type to name: `id, weight` where
    /// `id, tags, weight` was declared is not a value of another type, it is
    /// a value of this one that nothing can read. See [`admits`] for why that
    /// distinction has to be made here rather than left to the run.
    Fields {
        path: String,
        /// The declared struct, qualified.
        type_name: String,
        /// Its fields, in declaration order.
        expected: Vec<String>,
        /// The fields the value carried, in the order it carried them.
        found: Vec<String>,
    },
}

impl Wrong {
    /// Re-anchors this mismatch one level out, where the part that disagrees
    /// is reached by `step`.
    fn inside(mut self, step: &str) -> Wrong {
        match &mut self {
            Wrong::Type { path, .. } | Wrong::Fields { path, .. } => path.insert_str(0, step),
        }
        self
    }

    /// The disagreement, phrased for a host that handed the wrong value to
    /// argument `position`, counted from one as a reader counts.
    fn describe(&self, shown: &str, position: usize) -> String {
        let whole = |path: &str| {
            if path.is_empty() {
                format!("as argument {position}")
            } else {
                format!("at `{path}` of argument {position}")
            }
        };
        match self {
            Wrong::Type {
                path,
                expected,
                found,
            } => format!(
                "`{shown}` was given `{found}` {}, but it declares `{expected}` there",
                whole(path)
            ),
            Wrong::Fields {
                path,
                type_name,
                expected,
                found,
            } => format!(
                "`{shown}` was given a `{type_name}` carrying {} {}, but `{type_name}` declares {}, in that order",
                list(found),
                whole(path),
                list(expected)
            ),
        }
    }
}

/// Names as a sentence lists them, or `no fields` for none.
fn list(names: &[String]) -> String {
    if names.is_empty() {
        return "no fields".to_string();
    }
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A declared type and a value that is none of it.
fn mismatched(declared: &Ty, value: &Value, module: &str) -> Wrong {
    Wrong::Type {
        path: String::new(),
        expected: qualified(declared, module),
        found: value.type_name(),
    }
}

/// Whether `value` is one `ty` admits, as the checker resolved `ty` for a
/// declaration written in `module`.
///
/// `module` is there for one reason. A nominal type a module declares for
/// itself is recorded bare — `PullRequest` — and one it imported is recorded
/// qualified — `rules.policy.PullRequest` — which is exactly the distinction
/// `cove_sema::typeck::qualify` relies on. A runtime value's own type name is
/// always qualified, so the bare half has to be completed before the two can
/// be compared, and the module that completes it is the one the declaration
/// was written in.
///
/// # Why a declared struct is followed into its fields and a host type is not
///
/// [`crate::schema::Admits`] stops at a nominal name: an `http.Response` is
/// taken at its word about what is inside it, which is the line ADR 0013's
/// amendment draws. That line holds there because a host type's fields are
/// read *by name* wherever a program reads one, so a value that is missing
/// one is told so.
///
/// A type this package declares is not read that way. `cove_ir::lower` spends
/// the checker's answer and emits a `LoadField` at the payload word offset
/// the declaration's field order lays out, because every value of a declared
/// type is built by the lowering and stands in that order. A host's is not: a
/// struct carrying nine of ten fields would have that offset read past its
/// end, and one carrying ten in another order would answer the wrong field
/// with no sign of it. So this follows a declared struct all the way down —
/// the fields it carries, their order, and each one's declared type — and
/// what it costs is a walk of the value, once, at a call a host makes once
/// per request.
///
/// A declared *enum* is checked the same way for the same reason, except that
/// its case is checked by name, which is how both backends read one.
fn admits(program: &Program, ty: &Ty, value: &Value, module: &str) -> Result<(), Wrong> {
    match (ty, value) {
        // The checker settled nothing here, so there is nothing to hold the
        // value to. See the module docs.
        (Ty::Unknown(_), _) => Ok(()),
        (Ty::Unit, Value(Repr::Unit))
        | (Ty::Bool, Value(Repr::Bool(_)))
        | (Ty::Int, Value(Repr::Int(_)))
        | (Ty::Float, Value(Repr::Float(_)))
        | (Ty::Str, Value(Repr::Str(_)))
        | (Ty::Duration, Value(Repr::Duration(_)))
        | (Ty::Range, Value(Repr::Range { .. }))
        | (Ty::Scope, Value(Repr::TaskScope(_))) => Ok(()),
        (Ty::Error, Value(Repr::Struct(fields))) if &*fields.type_name == ERROR.name => Ok(()),
        (Ty::Array(item), Value(Repr::Array(items))) => {
            elements(program, item, items.iter(), module)
        }
        (Ty::Vector(item), Value(Repr::Vector(storage))) => {
            elements(program, item, storage.elements.borrow().iter(), module)
        }
        (Ty::Set(item), Value(Repr::Set(keys))) => {
            for key in keys.iter() {
                admits(program, item, &key.to_value(), module).map_err(|m| m.inside("{}"))?;
            }
            Ok(())
        }
        (Ty::Map(key_ty, value_ty), Value(Repr::Map(entries))) => {
            for (key, held) in entries.iter() {
                admits(program, key_ty, &key.to_value(), module).map_err(|m| m.inside("{key}"))?;
                admits(program, value_ty, held, module).map_err(|m| m.inside("{value}"))?;
            }
            Ok(())
        }
        // The two builtin enums, read the way `crate::schema` reads them: the
        // case names come from `cove_schema::builtins`, which is also where
        // `Value::some` and `Value::ok` get them.
        (Ty::Option(some), Value(Repr::Enum(case))) if &*case.type_name == OPTION.name => {
            match (&*case.case, case.payload.as_slice()) {
                (name, [inner]) if name == SOME_CASE.name => admits(program, some, inner, module)
                    .map_err(|m| m.inside(&SOME_CASE.wildcard_pattern())),
                (name, []) if name == NONE_CASE.name => Ok(()),
                _ => Err(mismatched(ty, value, module)),
            }
        }
        (Ty::Result(ok, error), Value(Repr::Enum(case))) if &*case.type_name == RESULT.name => {
            match (&*case.case, case.payload.as_slice()) {
                (name, [inner]) if name == OK_CASE.name => admits(program, ok, inner, module)
                    .map_err(|m| m.inside(&OK_CASE.wildcard_pattern())),
                (name, [inner]) if name == ERR_CASE.name => admits(program, error, inner, module)
                    .map_err(|m| m.inside(&ERR_CASE.wildcard_pattern())),
                _ => Err(mismatched(ty, value, module)),
            }
        }
        (Ty::MapEntry(..), Value(Repr::Struct(fields))) if &*fields.type_name == MAP_ENTRY.name => {
            Ok(())
        }
        (Ty::Struct(name, args), Value(Repr::Struct(held)))
            if named(name, module, &held.type_name) =>
        {
            declared_struct(program, name, args, held, module)
        }
        (Ty::Enum(name, args), Value(Repr::Enum(case))) if named(name, module, &case.type_name) => {
            declared_enum(program, name, args, case, module)
        }
        (Ty::Dyn(name), Value(Repr::Dyn(held))) if named(name, module, &held.trait_name) => Ok(()),
        // A host type is written the way source writes it — `http.Request` —
        // and is therefore already qualified, whichever module reads it. Its
        // fields are read by name, so they are not followed: see above.
        (Ty::Host(name), Value(Repr::Struct(fields))) if **name == *fields.type_name => Ok(()),
        (Ty::Host(name), Value(Repr::Resource(handle))) if handle.qualified_type() == **name => {
            Ok(())
        }
        // A callable is admitted by being callable. Its parameter types are
        // not checked, because a `Value::Closure` carries an arity and no
        // types: what it would take is the checker's answer about the body it
        // came from, and that body is not this declaration.
        (Ty::Fn(_), Value(Repr::Closure(_)) | Value(Repr::HostFn(_))) => Ok(()),
        (Ty::Task(_), Value(Repr::Task(_))) => Ok(()),
        (Ty::Shared(_), Value(Repr::Shared(_))) => Ok(()),
        _ => Err(mismatched(ty, value, module)),
    }
}

/// A struct value of a type this package declares, against the declaration.
///
/// The fields must be exactly the declared ones, in declaration order, and
/// each must be a value of its declared type — which for a generic struct is
/// the field's written type completed with the arguments the *use* was
/// written with.
///
/// A declaration this cannot find is admitted on its name alone. That is
/// unreachable for a checked package, because a `Ty::Struct` is only ever
/// built from a key the checker's own table answered; it is written as an
/// admission rather than as a panic because nothing here is worth ending a
/// process over.
fn declared_struct(
    program: &Program,
    name: &str,
    args: &[Ty],
    held: &StructValue,
    module: &str,
) -> Result<(), Wrong> {
    let (owner, type_name) = declaring(name, module);
    let Some(decl) = program
        .modules
        .get(owner)
        .and_then(|resolved| resolved.structs.get(type_name))
        .map(|entry| entry.decl.clone())
    else {
        return Ok(());
    };
    let declared: Vec<String> = decl
        .fields
        .iter()
        .map(|field| field.name.node.clone())
        .collect();
    let carried: Vec<String> = held
        .fields
        .iter()
        .map(|(field, _)| field.to_string())
        .collect();
    if declared != carried {
        return Err(Wrong::Fields {
            path: String::new(),
            type_name: held.type_name.to_string(),
            expected: declared,
            found: carried,
        });
    }
    let Some(signature) = program.facts.signature(decl.span.file, decl.span) else {
        return Ok(());
    };
    let generics = generics(&decl.generics);
    for (declared, (field, value)) in signature.params.iter().zip(&held.fields) {
        admits(
            program,
            &declared.instantiate(&generics, args),
            value,
            owner,
        )
        .map_err(|wrong| wrong.inside(&format!(".{field}")))?;
    }
    Ok(())
}

/// An enum value of a type this package declares, against the declaration.
///
/// The case must be one the declaration lists — both backends read a case by
/// name, so one they have never heard of would match no arm and end the run
/// somewhere the host cannot act on — and its payload must be the declared
/// one, in length and in type.
fn declared_enum(
    program: &Program,
    name: &str,
    args: &[Ty],
    held: &EnumValue,
    module: &str,
) -> Result<(), Wrong> {
    let (owner, type_name) = declaring(name, module);
    let Some(decl) = program
        .modules
        .get(owner)
        .and_then(|resolved| resolved.enums.get(type_name))
        .map(|entry| entry.decl.clone())
    else {
        return Ok(());
    };
    let Some(case) = decl.cases.iter().find(|case| case.name.node == *held.case) else {
        return Err(Wrong::Type {
            path: String::new(),
            expected: qualify(type_name, owner),
            found: format!("{}.{}", held.type_name, held.case),
        });
    };
    let Some(signature) = program.facts.signature(case.span.file, case.span) else {
        return Ok(());
    };
    if signature.params.len() != held.payload.len() {
        return Err(Wrong::Type {
            path: String::new(),
            expected: carrying(
                &qualify(type_name, owner),
                &case.name.node,
                signature.params.len(),
            ),
            found: carrying(&held.type_name, &held.case, held.payload.len()),
        });
    }
    let generics = generics(&decl.generics);
    for (position, (declared, value)) in signature.params.iter().zip(&held.payload).enumerate() {
        admits(
            program,
            &declared.instantiate(&generics, args),
            value,
            owner,
        )
        .map_err(|wrong| wrong.inside(&format!("{}(_)[{position}]", held.case)))?;
    }
    Ok(())
}

/// One case and how much it carries, for a payload of the wrong length.
fn carrying(type_name: &str, case: &str, values: usize) -> String {
    format!(
        "{type_name}.{case} carrying {values} {}",
        if values == 1 { "value" } else { "values" }
    )
}

/// A declaration's type parameters, named the way a recorded type names them.
fn generics(declared: &[GenericParam]) -> Vec<Arc<str>> {
    declared
        .iter()
        .map(|param| Arc::from(param.name.node.as_str()))
        .collect()
}

/// The module a nominal name belongs to, and the name inside it.
///
/// A qualified name carries its own; a bare one belongs to the module the
/// declaration reading it was written in.
fn declaring<'n>(name: &'n str, module: &'n str) -> (&'n str, &'n str) {
    match name.rsplit_once('.') {
        Some((owner, type_name)) => (owner, type_name),
        None => (module, name),
    }
}

/// Every element of a sequence, against the element type, reporting the index
/// the first disagreement was found at.
fn elements<'v>(
    program: &Program,
    item: &Ty,
    values: impl Iterator<Item = &'v Value>,
    module: &str,
) -> Result<(), Wrong> {
    for (index, element) in values.enumerate() {
        admits(program, item, element, module).map_err(|m| m.inside(&format!("[{index}]")))?;
    }
    Ok(())
}

/// Whether a nominal name the checker recorded for a declaration in `module`
/// is the name `value_name` a value carries.
fn named(declared: &str, module: &str, value_name: &str) -> bool {
    declared == value_name || (!declared.contains('.') && qualify(declared, module) == value_name)
}

/// A bare nominal name completed with the module that declared it.
fn qualify(name: &str, module: &str) -> String {
    format!("{module}.{name}")
}

/// A declared type as a diagnostic should spell it: the way the checker
/// spells it, with a bare nominal name completed the way a value's own name
/// is.
///
/// Only the outermost name is completed, because only a mismatch at the
/// outermost name reaches here with a bare one — a mismatch further in is
/// reported against the type at that point.
fn qualified(ty: &Ty, module: &str) -> String {
    match ty {
        Ty::Struct(name, _) | Ty::Enum(name, _) if !name.contains('.') => qualify(name, module),
        Ty::Dyn(name) if !name.contains('.') => format!("dyn {}", qualify(name, module)),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These hold the vocabulary — what a declared type admits, and how a
    /// refusal reads — over a program with no declarations in it, so that a
    /// nominal name is checked by name and stops there. Following a declared
    /// struct into its fields needs a checked package, and is held to in
    /// `crates/cove-runtime/tests/invoking.rs` on both backends.
    fn nothing() -> Program {
        Program::default()
    }

    fn structure(type_name: &str) -> Value {
        Value::structure(type_name, Vec::<(&str, Value)>::new())
    }

    #[test]
    fn a_declared_type_admits_the_value_it_names() {
        assert!(admits(&nothing(), &Ty::Int, &Value(Repr::Int(3)), "m").is_ok());
        assert!(admits(&nothing(), &Ty::Str, &Value(Repr::Str("t".into())), "m").is_ok());
        assert!(admits(&nothing(), &Ty::Bool, &Value(Repr::Bool(true)), "m").is_ok());
        assert!(admits(&nothing(), &Ty::Error, &Value::error("gone"), "m").is_ok());
        assert!(admits(&nothing(), &Ty::Unit, &Value(Repr::Unit), "m").is_ok());
    }

    #[test]
    fn a_declared_type_is_followed_all_the_way_down() {
        let declared = Ty::Array(Box::new(Ty::Str));
        assert!(admits(
            &nothing(),
            &declared,
            &Value::array([Value(Repr::Str("one".into()))]),
            "m"
        )
        .is_ok());

        let mismatch = admits(
            &nothing(),
            &declared,
            &Value::array([Value(Repr::Str("one".into())), Value(Repr::Int(2))]),
            "m",
        )
        .expect_err("an `Int` among the declared strings is not admitted");
        assert_eq!(
            mismatch.describe("m.f", 1),
            "`m.f` was given `Int` at `[1]` of argument 1, but it declares `String` there"
        );
    }

    /// The point of `module`: a struct a module declares for itself is
    /// recorded bare and a value's own name is always qualified.
    #[test]
    fn a_nominal_name_is_completed_by_the_module_that_declared_it() {
        let bare = Ty::Struct(Arc::from("PullRequest"), Vec::new());
        assert!(admits(
            &nothing(),
            &bare,
            &structure("rules.policy.PullRequest"),
            "rules.policy"
        )
        .is_ok());
        assert!(
            admits(&nothing(), &bare, &structure("PullRequest"), "rules.policy").is_ok(),
            "a name that is already the value's is not completed twice"
        );

        let imported = Ty::Struct(Arc::from("rules.policy.PullRequest"), Vec::new());
        assert!(admits(
            &nothing(),
            &imported,
            &structure("rules.policy.PullRequest"),
            "rules.embedded"
        )
        .is_ok());
        let mismatch = admits(
            &nothing(),
            &imported,
            &structure("rules.policy.Decision"),
            "rules.embedded",
        )
        .expect_err("another struct is not a `PullRequest`");
        assert_eq!(
            mismatch.describe("rules.embedded.evaluate", 1),
            "`rules.embedded.evaluate` was given `rules.policy.Decision` as argument 1, but it declares `rules.policy.PullRequest` there"
        );
    }

    #[test]
    fn a_refusal_spells_the_declared_type_the_way_the_checker_spells_it() {
        let bare = Ty::Struct(Arc::from("PullRequest"), Vec::new());
        let mismatch = admits(&nothing(), &bare, &Value(Repr::Int(3)), "rules.policy")
            .expect_err("an `Int` is not a `PullRequest`");
        assert_eq!(
            mismatch.describe("rules.decide", 1),
            "`rules.decide` was given `Int` as argument 1, but it declares `rules.policy.PullRequest` there"
        );
    }

    #[test]
    fn an_unknown_admits_whatever_it_is_given() {
        let unknown = Ty::Array(Box::new(
            // The checker's own `Unknown` constructors are private, so this
            // borrows one the only way a test can: from a signature it built.
            Ty::Unknown(cove_sema::typeck::Unknown::DynamicBoundary),
        ));
        assert!(admits(
            &nothing(),
            &unknown,
            &Value::array([Value(Repr::Int(1)), Value(Repr::Unit)]),
            "m"
        )
        .is_ok());
    }

    #[test]
    fn the_two_builtin_enums_are_admitted_by_either_case() {
        let option = Ty::Option(Box::new(Ty::Str));
        assert!(admits(&nothing(), &option, &Value::none(), "m").is_ok());
        assert!(admits(
            &nothing(),
            &option,
            &Value::some(Value(Repr::Str("s".into()))),
            "m"
        )
        .is_ok());
        assert_eq!(
            admits(&nothing(), &option, &Value::some(Value(Repr::Int(1))), "m")
                .expect_err("`Some(1)` is not an `Option<String>`")
                .describe("m.f", 1),
            "`m.f` was given `Int` at `Some(_)` of argument 1, but it declares `String` there"
        );

        let result = Ty::Result(Box::new(Ty::Int), Box::new(Ty::Error));
        assert!(admits(&nothing(), &result, &Value::ok(Value(Repr::Int(1))), "m").is_ok());
        assert!(admits(&nothing(), &result, &Value::err(Value::error("gone")), "m").is_ok());
    }

    #[test]
    fn a_signature_is_written_the_way_a_reader_writes_one() {
        assert_eq!(
            written(&Signature {
                receiver: None,
                params: vec![Ty::Struct(
                    Arc::from("rules.policy.PullRequest"),
                    Vec::new()
                )],
                ret: Ty::Struct(Arc::from("rules.policy.Decision"), Vec::new()),
            }),
            "fn(rules.policy.PullRequest) -> rules.policy.Decision"
        );
        assert_eq!(
            written(&Signature {
                receiver: None,
                params: Vec::new(),
                ret: Ty::Unit,
            }),
            "fn()"
        );
    }
}
