//! Static type checking, between resolution and execution.
//!
//! ADR 0004 decides what this checks and how: annotations are mandatory at
//! boundaries and inferred inside, types are nominal with no subtyping, and
//! checking is per-module. A module sees its own declarations, whatever it
//! imports with `use`, and the builtins. ADR 0006 replaces that ADR's
//! "parametric and unbounded" with "parametric with bounds", which is the
//! change it anticipated.
//!
//! # The import environment
//!
//! ADR 0005 makes a module able to name another module's exported
//! declarations, and ADR 0004 anticipated exactly one change for it: the
//! checker gains an import environment. That is `Checker::import`, and
//! nothing else about the pass is different.
//!
//! One rule holds it together. A declaration is known by the module that
//! declares it: its table key is its bare name inside that module, and
//! `module.Name` everywhere else. So two modules may each declare a
//! `Config` without the checker confusing them, and a type keeps one
//! identity however many imports it is reached through. Traits and
//! conformances travel with it, so an imported trait can be named in a bound
//! and a conformance declared anywhere in the package is found wherever the
//! trait and the type are both in scope. Modules are checked in dependency
//! order, which exists because ADR 0005 forbids import cycles.
//!
//! # Traits and the two dispatch forms
//!
//! A bound (`fn render<T: Display>(value: T)`) is checked at the call site
//! that instantiates `T`, because that is the only place a type parameter is
//! given a type. Inside the body the parameter is rigid and its bound is a
//! fact: a method call on a value of type `T` resolves through `T`'s bounds,
//! and a parameter with no bound has no methods at all.
//!
//! `dyn Display` is a type of its own, not a type parameter. Only the
//! trait's plain `self`-taking methods can be called on it: an associated
//! function has no receiver to dispatch on, and a `var self` method needs the
//! caller's own place, which a converted value is not. It never satisfies a
//! bound either — not even its own trait's — because it is not a type
//! parameter.
//!
//! # The one implicit conversion
//!
//! A concrete value is accepted where a `dyn Trait` is expected, exactly when
//! it conforms to that trait. That is the language's only implicit
//! conversion, and it is deliberately narrow:
//!
//! - it runs one way only: a `dyn Trait` value is never a concrete type, and
//!   never converts to another `dyn Trait`;
//! - it never reaches inside a generic argument. `Array<Booking>` is not an
//!   `Array<dyn Display>`, because generic arguments are invariant here like
//!   everywhere else. `[booking, receipt]` *is* an `Array<dyn Display>`,
//!   because each element is checked against `dyn Display` on its own;
//! - it satisfies no bound. `render(someDyn)` is an error even when
//!   `render<T: Display>` and the value is a `dyn Display`.
//!
//! It is spelled out here, in `coerces`, and nowhere else: every place that
//! compares a found type against an expected one goes through
//! `Checker::expect` or `unify`, and both consult it.
//!
//! # The type representation
//!
//! [`Ty`] is a closed enum of the builtin types, the structs and enums the
//! module declares, function types, rigid type parameters, and `dyn Trait`.
//! Two types are equal when they name the same declaration and their
//! arguments are equal: there is no subtyping and no variance, so
//! `Array<Int>` is not an `Array<Any>` — there is no `Any`.
//!
//! Two variants are not types a program can write:
//!
//! - [`Ty::Unknown`] is *the checker does not know*, and carries an
//!   [`Unknown`] saying why. It compares equal to every type whatever the
//!   reason, and every operation on it produces an unknown again, so one
//!   unknown never becomes a cascade of wrong errors.
//! - [`Ty::Never`] is the type of an expression that does not produce a
//!   value, such as `return`. It also compares equal to every type, because
//!   an arm that never produces a value never disagrees with one that does.
//!
//! # The four kinds of unknown
//!
//! `Unknown` is one variant doing four jobs, and telling them apart is what
//! makes a successful `cove check` worth reading. The kind is *carried by the
//! type*, not implied by which constructor built it, so a form can ask what
//! the silence around it is made of — and so one kind can be asserted never
//! to escape:
//!
//! | kind | constructor | `cove check` |
//! |------|-------------|--------------|
//! | [`Unknown::Recovery`] | `Ty::recovery` | silent |
//! | [`Unknown::DynamicBoundary`] | `Ty::dynamic_boundary` | silent here; see below |
//! | [`Unknown::Unconstrained`] | `Ty::unconstrained` | note, warning, or silent |
//! | [`Unknown::Placeholder`] | `Ty::placeholder` | must not escape |
//! | language gap | *none* | warning or error |
//!
//! **Recovery** is an unknown the checker owes no further word about,
//! because everything there was to say was said — here, a few lines above
//! the constructor, or upstream where the unknown being propagated came
//! from. Every "the receiver is already unknown, so abstain" branch is one
//! of these, and none of them adds a diagnostic. This is the job that keeps
//! one mistake from printing as ten. Its reach goes one step further than
//! the branch that builds it: the arguments of a rejected call are walked
//! against a recovery expectation, so an empty array or an unannotated
//! lambda parameter written inside one is not reported as a second mistake.
//!
//! **A dynamic boundary** is a host no Host API schema describes. ADR 0001's
//! Host API schema is [`cove_schema`], which this crate reads:
//! `console.println`, `http.Request`, and every other operation and type of
//! a shipped host module is checked against the same description the
//! boundary dispatches it through — and so is every operation and type of a
//! module an *embedder* describes, because ADR 0017 lets an embedding hand
//! its own [`cove_schema::ModuleSchema`] to `Compiler::with_host_schema` and
//! this pass reads the two the same way. What stays unknown is a module
//! neither table names: a host may register whatever it likes, and one it
//! never described is one no compiler could read.
//!
//! Nothing is reported per call into one. The fact is about the `use` that
//! named the module and about the compilation that was not shown it: no edit
//! to `sensors.read` can fix it, the remedy is one thing to say however many
//! calls a program makes, and it is the same remedy for a call, a member
//! read, and a value passed in. `cove::resolve::unchecked_host` puts that
//! warning at the `use`, where it belongs. What this pass owes such a call
//! is the abstention itself, handed to the arguments as their expected type,
//! so that a callback registered with an unschema'd host — the shape an
//! embedding is written in — is not asked to state a type nothing on this
//! side could have stated. A *type* named through such a module still warns
//! ([`HOST_TYPE`]), as it did before this classification existed.
//!
//! `Checker::host_schema` is the one place an embedder-supplied schema has
//! to reach, and reaching it is all it takes to turn any of that back into
//! ordinary checking.
//!
//! **An unconstrained** unknown is a type nothing that has been read states.
//! It has two sources. One is a shipped schema saying, in
//! `cove_schema::HostType::Any`, that there is nothing here that depends on
//! a type: in a parameter that costs nothing, because the operation accepts
//! every value, and in a result or a field it costs the rest of the program,
//! so those are noted ([`UNCONSTRAINED_RESULT`], [`UNCONSTRAINED_FIELD`]). A
//! note rather than a warning, because the schema chose this and no
//! strictness setting can make the checker prove what nobody stated. The
//! other is a type parameter no argument, annotation, or expected type
//! settles. Where the program could have said and did not — an empty array,
//! a bare `None`, a struct's parameter no field mentions — that warns
//! ([`UNCONSTRAINED`]); where nothing was asked of it at all, `Ok(1)` in a
//! place expecting no `Result`, it is carried silently, which is the second
//! hole named under *What a clean check guarantees* below.
//!
//! **A placeholder** is not a fourth kind of not-knowing; it is the marker
//! for a position no reachable program observes. Some are internal
//! positions the surrounding form settles before reading them, and some are
//! branches no reachable program takes at all. `Checker::expr` and
//! `Checker::declare` assert in debug builds that one never reaches a type
//! a program can observe, so the claim each site makes about itself is one
//! the test suite holds it to rather than a comment. Two sites used to break
//! it — a struct's type parameter that no field mentions, and
//! `Result.mapError`'s expected callback result — and each let a program
//! check clean and then be wrong at run time.
//!
//! **A language gap** is information the checker should have been given and
//! was not. These are the ones that used to pass silently, and none of them
//! does now:
//!
//! - a name nothing in scope explains, capitalized or not, is an error
//!   ([`UNRESOLVED_NAME`], [`UNKNOWN_NAME`], [`UNKNOWN_TYPE`]). A
//!   capitalized one used to be assumed to come from a host and warn; a host
//!   reaches a module through `use` like everything else, so the assumption
//!   named no real way for the name to arrive and only let an unknown
//!   through to validate whatever was done with it;
//! - a type or a module written where a value belongs is an error
//!   ([`NOT_A_VALUE`]). `Vector` in `Vector.of(1, 2)` is understood as part
//!   of the call; a bare `Vector`, `console`, or `Counter` is not a form
//!   with a type in this system, and never was. A host *operation* is not
//!   one of these: it is a value, and reading the schema gives it the
//!   function type it declares, so `let log = console.println` keeps working
//!   and a call through the value is checked. The one exception is a
//!   variadic operation, which no `fn` type in this language can describe —
//!   the language's own gap, said out loud as a note
//!   ([`VARIADIC_AS_VALUE`]) rather than hidden or refused;
//! - an early `return` in a function value nothing expects is an error
//!   ([`LAMBDA_RETURN`]). Such a lambda takes its result from its body's
//!   value, so a `return` produces one where the body's value is not, and
//!   nothing written anywhere says what the two have to agree on. "Nothing
//!   expects it" is asked of the expected *result* type: an expectation this
//!   pass abstained about answers for it, and one whose own result is a
//!   placeholder does not;
//! - an unannotated lambda parameter, an empty array literal, a bare `None`,
//!   and a struct's type parameter no field mentions, each in a place that
//!   expects nothing in particular, warn ([`UNCONSTRAINED`]). These are
//!   warnings rather than errors because the value is still usable and the
//!   operations that do not depend on the missing type are still checked —
//!   and because writing the type is always available, which is what each
//!   `help` says. "Expects nothing in particular" excludes a place this pass
//!   already abstained about, and a sibling or a branch that settles the
//!   type counts as saying it: `[[], [1]]` and
//!   `if c { None } else { Some(1) }` are proved, and are silent.
//!
//! One thing is deliberately *not* an unknown: the value a `scope` binds is
//! [`Ty::Scope`], a type the language gives no name to but this pass knows
//! exactly.
//!
//! # What a clean check guarantees
//!
//! `cove check` reporting nothing at all means every type the package wrote
//! down was checked: every struct field, declared parameter, call to a
//! declared or imported function, and call into a Host API module some
//! schema describes — shipped or embedder-supplied — was checked against a
//! written or schema-declared type.
//!
//! Two silences are not covered by that, and both are named here rather than
//! left to be discovered:
//!
//! - a host module no schema describes. Nothing about a call into one is
//!   proved, and nothing is said about it here either, because the fact
//!   belongs to the `use` that names the module, where
//!   `cove::resolve::unchecked_host` warns about it once. So a package
//!   reaching such a host does not have a clean check: it has one warning
//!   per `use`, naming the module whose schema was never handed over;
//! - a type parameter of a builtin constructor that nothing settles. `Ok(1)`
//!   in a place expecting no `Result` is a `Result<Int, _>`, and the `_` is
//!   carried rather than reported. Closing this means deciding what `Ok(1)`
//!   alone should mean, which is a language question and not this pass's to
//!   answer.
//!
//! A check whose only output is *notes* means the same, except at the places
//! the notes name: a shipped schema declared `Any` there, or a variadic host
//! operation was used as a value, and what the program does with the value
//! from that point on is the boundary's to check.
//!
//! A check with *warnings* means the package left the checker something to
//! infer that nothing written settles. `cove check --deny-warnings` is
//! exactly the request that it did not.
//!
//! What none of these guarantee is anything the runtime keeps for itself:
//! task safety of a host resource, and every rule listed under *What the
//! runtime keeps* below.
//!
//! Two things about a shipped host module are read here and *not* enforced by
//! the boundary, which is worth stating in one place. A host type's fields
//! are typed from the schema — `request.path` is a `String` because the
//! schema says a `Request` has one — while the boundary checks a declared
//! type by name only, so what this checks is that the *program* built the
//! value the schema describes. And a host resource's declared task-safety is
//! still the runtime's alone: `Ty::Host` says nothing about crossing a task
//! boundary, so a resource declaring `task_safe: false` is refused where it
//! crosses and not before.
//!
//! # Places, and what kind of analysis this pass is
//!
//! This pass is not only a type checker. ADR 0021 settles what else it is:
//! it may decide any fact the source settles through the binding structure
//! it already walks, and mutability is one. `let` creates a read-only place
//! and `var` a mutable one, so which places a program may write is read off
//! the scope stack — `Checker::place_mutability` is the definition, and
//! the interpreter and `cove_ir::lower` had a reading of it each until it
//! moved here.
//!
//! Four constructs are refused by it, in the words the interpreter refused
//! them in: an assignment to a read-only place, a `var` argument that is a
//! read-only place or is no place at all, and a mutating receiver that is
//! either. A fifth is the same kind of fact about a call's shape rather than
//! about a place: labeled arguments appear in declaration order, and
//! [`LABEL_ORDER`] is what says so.
//!
//! Two more are facts about a *declaration* rather than about a call, and
//! ADR 0021's rule reaches them by the same test — the parameter list is
//! structure this pass already walks to build a signature. A variadic
//! parameter is the last one its declaration writes ([`VARIADIC_POSITION`])
//! and is not written with a default ([`VARIADIC_DEFAULT`]).
//! `Checker::check_variadic_shape` is where both are decided. Unlike the
//! five above, these two are wording of this pass's own, because there was
//! no behaviour to keep: `Interpreter::assign_labels` and `bind_params`
//! disagreed about what a non-last variadic parameter binds, and nothing in
//! either backend could ever reach a variadic parameter's default.
//!
//! A third is about where a variadic parameter may be written at all: on a
//! declaration, and not on a function value ([`VARIADIC_LAMBDA`],
//! `Checker::lambda`). This one does not come from ADR 0021 but from
//! ADR 0016 — a function type names a fixed list of parameters, and a
//! function value has exactly the parameters its type names. It was the VM's
//! lowering that refused it first, and the two backends disagreed underneath
//! that refusal: this pass typed such a parameter as its element type and
//! dropped the `...`, while `Interpreter::bind_params` wrapped the argument
//! in an `Array` as it does for any variadic slot, so `fn(items: Int...)`
//! called with `1` bound `1` on one backend and `[1]` on the other. Deciding
//! it here makes it one diagnostic on both rather than one backend's
//! silence; issue #168 is where the question it does *not* settle — what
//! such a parameter would mean — is written down.
//!
//! Two things bound it, and both are abstentions rather than gaps in the
//! rule. A name this pass did not bind is not a place and is not reported as
//! one — see `Checker::not_a_place`. And a receiver whose type is an
//! unknown or a host type is left alone, because the interpreter reaches a
//! host resource's own operations before it reaches any of this.
//!
//! # What the runtime keeps
//!
//! The interpreter's own checks stay, as ADR 0004 says. One rule about a
//! call is left to it entirely, and it is worth naming because it is
//! decidable here and is not decided here: whether a `var` marking written
//! at a call site agrees with the one written at the declaration. A function
//! *type* carries no marking, so a call through a value has nothing to check
//! against, and the parameters this pass builds for a builtin, a host
//! operation and a struct's initializer are not written with a marking at
//! all. ADR 0021 records it as unfinished rather than as decided.
//!
//! # Where a form's value comes from
//!
//! `docs/LANGUAGE_REFERENCE.md` states one rule per expression form, and the
//! two that this pass and the interpreter used to answer differently are
//! stated there because they had to be decided rather than discovered:
//!
//! - An `if` with no `else` produces `()`, and its branch's value is
//!   discarded. There is no second branch to give the missing case a value,
//!   so the branch that runs does not supply one either.
//! - Every loop produces `()`. A `for` runs out of items and a `while` runs
//!   out of condition, so a loop can reach its end without breaking and
//!   there is nothing at that end to produce but `()`; a `break` operand is
//!   checked on its own and its value discarded, exactly as an `if`'s
//!   branch value is. Whether a loop should ever carry a value is issue #87.
//!
//! The interpreter obeys both, so a checked program's static and dynamic
//! answers are the same one.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use cove_diag::{Diagnostic, FileId, Span};
use cove_schema::builtins::{
    BuiltinSchema, BuiltinType, FreeBuiltinKind, FreeBuiltinSchema, MethodSchema, ParamSchema,
    MAP_ENTRY, NONE_CASE, SCOPE,
};
use cove_schema::{
    HostSchemas, HostType, ModuleSchema, OperationSchema, ResourceSchema, TypeSchema,
};
use cove_syntax::ast::{
    Arg, BinaryOp, Block, EnumDecl, Expr, ExprId, ExprKind, FnDecl, GenericParam, Ident, ItemKind,
    MatchArm, Param, Pattern, PatternKind, Stmt, StmtKind, StrPart, StructDecl, TraitMethod, Type,
    TypeKind, UnaryOp,
};

use crate::facts::{Facts, MethodTarget, Signature};
use crate::package::Package;
use crate::resolve::{Conformance, Program, ResolvedModule, TraitEntry};

/// An argument's type does not match the parameter, field, or payload it is
/// given to.
pub const MISMATCH: &str = "cove::type::mismatch";
/// A call passes more arguments than the callee declares parameters.
pub const ARITY: &str = "cove::type::arity";
/// A call omits a parameter that has no default.
pub const MISSING_ARGUMENT: &str = "cove::type::missing_argument";
/// An argument label names no parameter of the callee.
pub const UNKNOWN_LABEL: &str = "cove::type::unknown_label";
/// A lowercase name is not in scope.
pub const UNKNOWN_NAME: &str = "cove::type::unknown_name";
/// A capitalized name no module declares and no `use` reaches.
pub const UNRESOLVED_NAME: &str = "cove::type::unresolved_name";
/// A type name no module declares.
pub const UNKNOWN_TYPE: &str = "cove::type::unknown_type";
/// A type reached through a host module the schema does not describe
/// (warning).
pub const HOST_TYPE: &str = "cove::type::host_type";
/// A host module's schema declares no type of that name.
pub const UNKNOWN_HOST_TYPE: &str = "cove::type::unknown_host_type";
/// A host module's schema declares no operation of that name, on the module
/// or on one of its resources.
pub const UNKNOWN_HOST_OPERATION: &str = "cove::type::unknown_host_operation";
/// A generic type is given the wrong number of type arguments.
pub const TYPE_ARGUMENTS: &str = "cove::type::type_arguments";
/// A type alias expands to itself.
pub const ALIAS_CYCLE: &str = "cove::type::alias_cycle";
/// A field access names no field of the receiver's type.
pub const UNKNOWN_FIELD: &str = "cove::type::unknown_field";
/// A field of an `export opaque struct` is named outside the module that
/// declares it.
pub const OPAQUE_FIELD: &str = "cove::type::opaque_field";
/// The synthesized labeled constructor of an `export opaque struct` is
/// called outside the module that declares it.
pub const OPAQUE_CONSTRUCTION: &str = "cove::type::opaque_construction";
/// A method call names no method of the receiver's type.
pub const UNKNOWN_METHOD: &str = "cove::type::unknown_method";
/// An associated call names no associated function of the type.
pub const UNKNOWN_ASSOCIATED: &str = "cove::type::unknown_associated_function";
/// A qualified case names no case of the enum.
pub const UNKNOWN_CASE: &str = "cove::type::unknown_case";
/// An enum case is constructed with the wrong number of payload values.
pub const PAYLOAD_ARITY: &str = "cove::type::payload_arity";
/// An operator is not defined for these operand types.
pub const OPERATOR: &str = "cove::type::operator";
/// A condition is not a `Bool`.
pub const CONDITION: &str = "cove::type::condition";
/// The branches of an `if` or the arms of a `match` produce different types.
pub const BRANCHES: &str = "cove::type::branches";
/// `?` was applied to something that is not a `Result` or an `Option`.
pub const TRY_OPERAND: &str = "cove::type::try_operand";
/// `?` propagates a failure the enclosing function cannot return.
pub const TRY_RETURN: &str = "cove::type::try_return";
/// `await` was applied to something that is not a `Task`.
pub const AWAIT_OPERAND: &str = "cove::type::await_operand";
/// `for` was given something it cannot iterate.
pub const ITERABLE: &str = "cove::type::iterable";
/// A call was made to something that is not a function.
pub const NOT_CALLABLE: &str = "cove::type::not_callable";
/// A pattern matches a different type than the scrutinee.
pub const PATTERN: &str = "cove::type::pattern";
/// A method was called without a receiver, or an associated function with one.
pub const RECEIVER: &str = "cove::type::receiver";
/// An expression written where a place is required is not one: an
/// assignment's target, a `var` argument, or a `var self` receiver.
pub const NOT_A_PLACE: &str = "cove::type::not_a_place";
/// A place `let` made read-only is written, passed as `var`, or given to a
/// `var self` receiver.
///
/// One code for the one rule — `let` creates a read-only place; `var`
/// creates a mutable place — however it is broken. The three messages differ
/// because what the program was doing differs; the fact reported is the
/// same, and a reader who wants to suppress or search for it wants all three.
pub const READ_ONLY_PLACE: &str = "cove::type::read_only_place";
/// A labeled argument fills a parameter that stands before one an earlier
/// argument already filled.
pub const LABEL_ORDER: &str = "cove::type::label_order";
/// An entry function's shape does not fit the host boundary.
pub const ENTRY: &str = "cove::type::entry";
/// A `dyn` or a bound names something that is not a trait this module can see.
pub const UNKNOWN_TRAIT: &str = "cove::type::unknown_trait";
/// A type argument does not conform to the bound its type parameter declares.
pub const UNSATISFIED_BOUND: &str = "cove::type::unsatisfied_bound";
/// A method was called on a type parameter that declares no bound.
pub const UNBOUNDED_PARAMETER: &str = "cove::type::unbounded_parameter";
/// A conformance's method does not have the signature its trait declares.
pub const CONFORMANCE_SIGNATURE: &str = "cove::type::conformance_signature";
/// A trait method with no `self` was called through `dyn Trait`.
pub const DYN_ASSOCIATED: &str = "cove::type::dyn_associated_function";
/// A `var self` trait method was called through `dyn Trait`.
pub const DYN_MUTATING: &str = "cove::type::dyn_mutating_method";
/// A bound was written where the MVP does not check one.
pub const UNSUPPORTED_BOUND: &str = "cove::type::unsupported_bound";
/// A qualified name reaches nothing an imported module exports.
pub const UNKNOWN_MEMBER: &str = "cove::type::unknown_member";
/// A `test fn` does not have the shape the test runner calls.
pub const TEST: &str = "cove::type::test";
/// A type that may not cross a task boundary was written where one must.
pub const TASK_SAFETY: &str = "cove::type::task_safety";
/// A declaration's parameter has no written type. Unlike a lambda's, it has
/// no expected type at a call site to infer from.
pub const MISSING_PARAMETER_TYPE: &str = "cove::type::missing_parameter_type";
/// A variadic parameter stands somewhere other than last in its
/// declaration's parameter list.
pub const VARIADIC_POSITION: &str = "cove::type::variadic_position";
/// A variadic parameter is written with a default.
pub const VARIADIC_DEFAULT: &str = "cove::type::variadic_default";
/// A variadic parameter is written on a function value, whose parameters are
/// its function type's and so are a fixed list.
pub const VARIADIC_LAMBDA: &str = "cove::type::variadic_lambda";
/// A host operation whose schema declares its result `Any`, so the checker
/// can prove nothing about the value it produced (note).
pub const UNCONSTRAINED_RESULT: &str = "cove::type::unconstrained_result";
/// A host type's field whose schema declares it `Any`, so the checker can
/// prove nothing about the value read from it (note).
pub const UNCONSTRAINED_FIELD: &str = "cove::type::unconstrained_field";
/// A variadic host operation used as a value, which no function type in this
/// language can describe (note).
pub const VARIADIC_AS_VALUE: &str = "cove::type::variadic_as_value";
/// A type or a module is written where a value belongs.
pub const NOT_A_VALUE: &str = "cove::type::not_a_value";
/// A function value uses `return`, with nothing saying what it produces.
pub const LAMBDA_RETURN: &str = "cove::type::lambda_return";
/// Nothing written anywhere says what a type is: an unannotated lambda
/// parameter, an empty array literal, a bare `None`, or a struct's type
/// parameter no field mentions, in a place that expects nothing in
/// particular (warning).
pub const UNCONSTRAINED: &str = "cove::type::unconstrained";

/// The Language Card sentence a task-safety diagnostic quotes.
///
/// `cove_runtime::task` states the same rule for values; the compiler does
/// not depend on the runtime, so the sentence appears in both places, and it
/// is the card's own words in both.
const TASK_SAFETY_RULE: &str = "Immutable task-safe values such as arrays may cross task boundaries. A vector cannot cross, even through `let`; finish it as an array or wrap mutable state in `Shared` or another synchronized type. Closures are task-safe only when every capture is.";

/// Type-checks a resolved program.
///
/// Every module is checked against its own declarations, the declarations it
/// imported, and the builtins, and every `[run.<name>]` entry against the
/// shape the host boundary calls. The result holds both errors and warnings;
/// an empty result means the program checks.
///
/// Modules are checked in dependency order, so a module's imports are
/// already resolved signatures by the time its own declarations are. ADR
/// 0005 forbids import cycles, which is what makes such an order exist.
pub fn check(package: &Package, program: &Program) -> Vec<Diagnostic> {
    check_with(package, program, &HostSchemas::new())
}

/// Type-checks a resolved program against `schemas`, the host modules this
/// compilation may name.
///
/// This is [`check`] with the one thing an embedder can change. A module in
/// `schemas` is checked exactly as a shipped one is: its operations' arity,
/// argument types, and results; the fields of the types it declares; the
/// cases of its enums; and the operations its resources answer.
pub fn check_with(package: &Package, program: &Program, schemas: &HostSchemas) -> Vec<Diagnostic> {
    check_facts(package, program, schemas).0
}

/// Type-checks a resolved program against `schemas`, keeping what the check
/// worked out about each expression.
///
/// This is [`check_with`] with its second answer. The check is the same one
/// — the facts are written as the walk settles them and read by nothing
/// during it — so a caller that wants only the diagnostics loses nothing by
/// taking this instead, and one that wants the types does not have to derive
/// them a second time. [`Facts`] says why deriving them a second time is the
/// thing worth avoiding.
pub fn check_facts(
    package: &Package,
    program: &Program,
    schemas: &HostSchemas,
) -> (Vec<Diagnostic>, Facts) {
    let mut diagnostics = Vec::new();
    let mut envs: BTreeMap<&str, ImportEnv> = BTreeMap::new();
    let mut checked: BTreeMap<&str, Checker> = BTreeMap::new();
    for name in import_order(program) {
        let module = &program.modules[name];
        let mut checker = Checker::new(module, program, schemas);
        checker.import(&envs);
        checker.prepare();
        envs.insert(name, checker.export_env());
        checker.check_bodies();
        diagnostics.append(&mut checker.diagnostics);
        checked.insert(name, checker);
    }
    check_entries(package, &checked, &mut diagnostics);
    check_tests(program, &checked, &mut diagnostics);
    // Each module is checked by a checker of its own, so the facts arrive in
    // as many tables as there are modules. A file belongs to one module, so
    // gathering them into one table keyed by file loses nothing.
    let mut facts = Facts::default();
    for (_, checker) in checked {
        facts.merge(checker.facts);
    }
    (diagnostics, facts)
}

/// Checks every `test fn` against the one shape the test runner calls.
///
/// The runner passes nothing and reports the test's failure through its
/// `Err`, so a test takes no parameters and returns `Result<Unit, Error>`.
/// Anything else is rejected here rather than at run time, naming the shape
/// that is required.
fn check_tests(
    program: &Program,
    checked: &BTreeMap<&str, Checker<'_>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let required = Ty::Result(Box::new(Ty::Unit), Box::new(Ty::Error));
    for test in program.tests() {
        let Some(checker) = checked.get(test.module) else {
            continue;
        };
        let Some(sig) = checker.functions.get(test.name) else {
            continue;
        };
        let shape = format!("write `test fn {}() -> Result<Unit, Error>`", test.name);

        if let Some(param) = sig.params.first() {
            diagnostics.push(
                Diagnostic::error(
                    TEST,
                    format!(
                        "test `{}` declares {} parameter(s)",
                        test.qualified_name(),
                        sig.params.len()
                    ),
                )
                .at(param.span)
                .rule("A `test fn` takes no parameters: the test runner is its only caller, and it passes nothing.")
                .help(shape.clone()),
            );
        }

        if sig.is_async {
            diagnostics.push(
                Diagnostic::error(
                    TEST,
                    format!("test `{}` is `async`", test.qualified_name()),
                )
                .at(test.entry.decl.name.span)
                .rule("A `test fn` is an ordinary function the test runner calls and awaits nothing of.")
                .help(shape.clone()),
            );
        }

        if !sig.ret.matches(&required) {
            diagnostics.push(
                Diagnostic::error(
                    TEST,
                    format!(
                        "test `{}` returns `{}`, but a test returns `Result<Unit, Error>`",
                        test.qualified_name(),
                        sig.ret
                    ),
                )
                .at(sig.ret_span)
                .rule("A test reports failure the way every other Cove function reports expected failure, so it returns `Result<Unit, Error>` and `?` works inside it.")
                .help(shape),
            );
        }
    }
}

/// Every module of `program`, each after the modules it imports from.
///
/// A package whose modules form a cycle never reaches this pass, since
/// resolution rejects one; if one somehow does, the modules left over are
/// checked in name order rather than dropped.
fn import_order(program: &Program) -> Vec<&str> {
    let mut order: Vec<&str> = Vec::new();
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    loop {
        let mut progressed = false;
        for (name, module) in &program.modules {
            if placed.contains(name.as_str()) {
                continue;
            }
            let ready = module
                .dependencies()
                .iter()
                .all(|dep| placed.contains(dep) || !program.modules.contains_key(*dep));
            if ready {
                order.push(name.as_str());
                placed.insert(name.as_str());
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    order.extend(
        program
            .modules
            .keys()
            .map(String::as_str)
            .filter(|name| !placed.contains(name)),
    );
    order
}

/// The canonical `(trait key, type key)` a recorded conformance names, as
/// the module that declared it sees them: bare for a party this module
/// declares, `module.Name` for an imported one.
fn conformance_key(module: &ResolvedModule, conformance: &Conformance) -> (String, String) {
    let key = |owner: &str, name: &str| {
        if owner == module.name {
            name.to_string()
        } else {
            format!("{owner}.{name}")
        }
    };
    (
        key(&conformance.trait_module, &conformance.trait_name),
        key(&conformance.type_module, &conformance.type_name),
    )
}

/// The canonical key of a declaration `module` makes, leaving a name that
/// already carries a module alone.
fn qualified_name(name: &Arc<str>, module: &str) -> Arc<str> {
    if name.contains('.') {
        name.clone()
    } else {
        format!("{module}.{name}").into()
    }
}

/// Splits a table key into the module that declares the type and the type's
/// own name, when the key names a type this module did not declare.
///
/// A checker keys its own module's declarations by their bare name and every
/// imported one by `module.Name`, so a key that carries a module in front is
/// exactly a declaration written somewhere else. That is the whole test an
/// opaque type's boundary needs: inside the declaring module the key is
/// bare, and the representation is in reach.
fn foreign_type(key: &str) -> Option<(&str, &str)> {
    key.rsplit_once('.')
}

/// What a field expression is doing with the field it names: taking the
/// value out, or being the place a value goes.
///
/// Both reach a field through the same check, so refusing one across an
/// opaque boundary refuses the other — but the two need different words,
/// since telling someone to "read the value through an exported method" is
/// no answer to an assignment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FieldUse {
    Read,
    Write,
}

impl FieldUse {
    /// What this use cannot do, for the message.
    fn refused(self) -> &'static str {
        match self {
            FieldUse::Read => "read",
            FieldUse::Write => "assigned",
        }
    }

    /// How to do it through the interface instead, for the help.
    fn correction(self) -> &'static str {
        match self {
            FieldUse::Read => "read the value through an exported method, such as",
            FieldUse::Write => "change the value through an exported method, such as",
        }
    }
}

/// Everything one module offers the modules that import it: the signatures
/// of its own declarations, keyed by the canonical `module.Name` a foreign
/// declaration is known by, plus every foreign signature it imported in
/// turn, so a type reached through two imports keeps one identity.
#[derive(Clone, Debug, Default)]
struct ImportEnv {
    structs: BTreeMap<String, StructSig>,
    enums: BTreeMap<String, EnumSig>,
    aliases: BTreeMap<String, (Vec<Arc<str>>, Ty)>,
    functions: BTreeMap<String, FnSig>,
    methods: BTreeMap<(String, String), FnSig>,
    traits: BTreeMap<String, BTreeMap<String, FnSig>>,
    /// Every conformance the module declares or can see, as canonical
    /// `(trait key, type key)` pairs.
    ///
    /// Conformance travels with the declarations it joins because it is a
    /// fact about them, not about the module that wrote it down: a bound is
    /// satisfied wherever both parties are in scope, and the orphan rule is
    /// what guarantees the conformance is somewhere on the import path that
    /// brought them here.
    conformances: BTreeSet<(String, String)>,
}

/// The first part of `ty` that may not cross a task boundary, if there is
/// one.
///
/// A `Vector` may not cross even through `let`, and neither may a task or a
/// task scope, which belong to the task that holds them. Everything else is
/// task-safe exactly when what it contains is — except a `Shared`, which
/// crosses by sharing rather than by copying and so answers for itself.
fn not_task_safe(ty: &Ty) -> Option<&Ty> {
    match ty {
        Ty::Vector(_) | Ty::Task(_) | Ty::Scope => Some(ty),
        Ty::Shared(_) => None,
        Ty::Array(inner) | Ty::Set(inner) | Ty::Option(inner) => not_task_safe(inner),
        Ty::Map(key, value) | Ty::MapEntry(key, value) | Ty::Result(key, value) => {
            not_task_safe(key).or_else(|| not_task_safe(value))
        }
        Ty::Struct(_, args) | Ty::Enum(_, args) => args.iter().find_map(not_task_safe),
        // A closure is task-safe when every capture is, which is a fact about
        // the values it closed over rather than about its type.
        _ => None,
    }
}

/// Rewrites the nominal names `module` declares into the canonical
/// `module.Name` form.
///
/// A name that already carries a module is left alone: it is already
/// absolute, so a type reached through two imports keeps one identity. A
/// name a module writes for its own declaration is bare, which is what makes
/// the two cases distinguishable.
fn qualify(ty: &Ty, module: &str) -> Ty {
    let qualified = |name: &Arc<str>| qualified_name(name, module);
    match ty {
        Ty::Array(inner) => Ty::Array(Box::new(qualify(inner, module))),
        Ty::Vector(inner) => Ty::Vector(Box::new(qualify(inner, module))),
        Ty::Set(inner) => Ty::Set(Box::new(qualify(inner, module))),
        Ty::Option(inner) => Ty::Option(Box::new(qualify(inner, module))),
        Ty::Task(inner) => Ty::Task(Box::new(qualify(inner, module))),
        Ty::Shared(inner) => Ty::Shared(Box::new(qualify(inner, module))),
        Ty::Map(k, v) => Ty::Map(Box::new(qualify(k, module)), Box::new(qualify(v, module))),
        Ty::MapEntry(k, v) => {
            Ty::MapEntry(Box::new(qualify(k, module)), Box::new(qualify(v, module)))
        }
        Ty::Result(t, e) => Ty::Result(Box::new(qualify(t, module)), Box::new(qualify(e, module))),
        Ty::Struct(name, args) => Ty::Struct(
            qualified(name),
            args.iter().map(|arg| qualify(arg, module)).collect(),
        ),
        Ty::Enum(name, args) => Ty::Enum(
            qualified(name),
            args.iter().map(|arg| qualify(arg, module)).collect(),
        ),
        // A `dyn Trait` names a trait, which belongs to a module exactly as
        // a struct or an enum does.
        Ty::Dyn(name) => Ty::Dyn(qualified(name)),
        Ty::Fn(f) => Ty::func(
            f.is_async,
            f.params.iter().map(|p| qualify(p, module)).collect(),
            qualify(&f.ret, module),
        ),
        other => other.clone(),
    }
}

// ------------------------------------------------------------------- types

/// Why the checker does not know a type.
///
/// This is the classification the module documentation describes, carried by
/// the type rather than implied by which constructor built it. Every kind
/// compares equal to every type — telling them apart decides what a reader is
/// told, never what type-checks — so the only thing this changes about the
/// checking rules is that a form can ask whether the unknown standing in for
/// its context has already been accounted for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unknown {
    /// An error was already reported about this place, or about the value
    /// this one was derived from. Silent.
    Recovery,
    /// A host module no schema describes — neither a shipped one nor one an
    /// embedder handed over. The remedy is at the `use` that names the
    /// module, not here.
    DynamicBoundary,
    /// Nothing that has been read states this type: a shipped schema's
    /// `HostType::Any`, or a type parameter no argument, annotation, or
    /// expected type settles.
    Unconstrained,
    /// A position no reachable program observes.
    ///
    /// This is not a classification of a program's type. It marks the
    /// internal positions the surrounding form settles before anything reads
    /// them and the ones no reachable program produces at all, and
    /// `Checker::expr` and `Checker::declare` assert in debug builds that
    /// one never reaches a type a program can observe. If one ever does, the
    /// assertion names the site rather than leaving the unknown to validate
    /// whatever came after it.
    Placeholder,
}

impl Unknown {
    /// Whether the checker has already said whatever it has to say about the
    /// place this unknown stands for.
    ///
    /// Every kind but [`Unknown::Placeholder`] has: a recovery unknown has a
    /// diagnostic above it, a dynamic boundary belongs to a `use` naming a
    /// module this build cannot see, and an unconstrained one is either a
    /// schema's own `Any` or a parameter reported where it was left open. So
    /// a form given to a place typed by one of those adds nothing by
    /// complaining that the place said nothing — that is the whole content of
    /// the diagnostic already given. A placeholder has said nothing anywhere,
    /// which is why it must never reach a place a form can be given to.
    fn is_accounted_for(self) -> bool {
        !matches!(self, Unknown::Placeholder)
    }
}

/// A Cove type.
///
/// A name inside one is shared by [`Arc`] rather than [`std::rc::Rc`]
/// because a type outlives the check that settled it: it is recorded in
/// [`Facts`] and published on [`Program`], which the runtime holds behind an
/// `Arc` and moves onto the stack it runs a program on. Sharing atomically
/// is what lets a type be a fact about a checked package rather than a value
/// that dies with the checker.
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    /// The checker could not determine this type; see the module docs.
    Unknown(Unknown),
    /// The type of an expression that never produces a value, such as
    /// `return`.
    Never,
    Unit,
    Bool,
    Int,
    Float,
    Str,
    Duration,
    Error,
    Range,
    Array(Box<Ty>),
    Vector(Box<Ty>),
    Set(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    /// One `key`/`value` pair: what `Map.of` collects and what `for` binds
    /// over a `Map`.
    MapEntry(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    Task(Box<Ty>),
    /// `Shared<T>`: mutable state more than one task may reach.
    ///
    /// The Language Card names it in the sentence that keeps a vector out of
    /// a task, and ADR 0008 makes it the one value that crosses a task
    /// boundary by sharing rather than by copying. Its argument must
    /// therefore be task-safe itself: a `Shared<Vector<T>>` would let a
    /// vector be reached from two tasks, which is what that sentence forbids.
    Shared(Box<Ty>),
    /// The value `scope name { ... }` binds.
    Scope,
    /// A struct this module declares, with its type arguments.
    Struct(Arc<str>, Vec<Ty>),
    /// An enum this module declares, with its type arguments.
    Enum(Arc<str>, Vec<Ty>),
    Fn(Arc<FnTy>),
    /// A type parameter, rigid inside the body that declares it.
    Param(Arc<str>),
    /// `dyn Display`: a value of some type that conforms to the named trait,
    /// carrying its implementation with it.
    ///
    /// This is a type of its own, not a type parameter: it cannot be written
    /// where a bounded type parameter is expected, and only the trait's
    /// `self`-taking methods can be called on it.
    Dyn(Arc<str>),
    /// A type a host module declares, named the way Cove source writes it:
    /// `http.Request`, `database.Connection`.
    ///
    /// It is nominal like every other type here and carries no arguments,
    /// because [`cove_schema::HostType`] has none to carry. Whether the host
    /// hands the value over or keeps it — a `TypeSchema` or a
    /// `ResourceSchema` — does not change how it is written or compared, so
    /// it does not change this either; what the schema says about it decides
    /// what may be read from it and what may be called on it.
    Host(Arc<str>),
}

/// A function type: `fn(Int) -> Int`, `async fn() -> Result<Unit, Error>`.
#[derive(Clone, Debug, PartialEq)]
pub struct FnTy {
    pub is_async: bool,
    pub params: Vec<Ty>,
    pub ret: Ty,
}

impl Ty {
    fn func(is_async: bool, params: Vec<Ty>, ret: Ty) -> Ty {
        Ty::Fn(Arc::new(FnTy {
            is_async,
            params,
            ret,
        }))
    }

    /// An unknown the checker owes no further word about.
    ///
    /// Everything there was to say about this place has been said, either
    /// here — the diagnostic sits a few lines above every one of these — or
    /// upstream, where the unknown being propagated was first produced. A
    /// recovery unknown therefore never carries a diagnostic of its own,
    /// which is what keeps one mistake from becoming a page of them.
    fn recovery() -> Ty {
        Ty::Unknown(Unknown::Recovery)
    }

    /// An unknown that belongs to a host no schema describes.
    ///
    /// A host may register any module it likes, and one whose schema no
    /// compilation was shown is named in no table this pass could read, so a
    /// call into it, or a value of a type from it, is checked at the boundary
    /// rather than here. Nothing is reported where one of these is produced:
    /// the silence is a fact about this *compilation*, the remedy is to hand
    /// the module's schema to the compiler with
    /// `cove_sema::Compiler::with_host_schema`, and the place to say so is
    /// the `use` that named the module, where
    /// `cove::resolve::unchecked_host` says it once. `Checker::host_schema`
    /// is where an embedder-supplied schema arrives and turns all of it back
    /// into ordinary checking.
    fn dynamic_boundary() -> Ty {
        Ty::Unknown(Unknown::DynamicBoundary)
    }

    /// An unknown nothing that has been read states.
    ///
    /// It has two sources. One is a shipped schema's `HostType::Any`, which
    /// is not a missing type but a statement that there is nothing here that
    /// depends on one: nothing is lost where it is a *parameter*, because the
    /// operation accepts every value, and a *result* or a *field* declared
    /// `Any` is noted where it is read, because from there on the program is
    /// working with a value whose type nothing stated.
    ///
    /// The other is a type parameter no argument, annotation, or expected
    /// type settles. Where the program could have said and did not — an empty
    /// array, a bare `None`, a struct parameter no field mentions — that
    /// warns where it is written; where nothing was asked of it at all, it is
    /// carried, which the module documentation names as one of the two things
    /// a clean check does not cover.
    fn unconstrained() -> Ty {
        Ty::Unknown(Unknown::Unconstrained)
    }

    /// An unknown no program type is read from.
    ///
    /// This is not a fourth kind of not-knowing. It marks the few positions
    /// the surrounding form settles before anything looks at them — a return
    /// type a body is about to supply, a receiver only asked whether it is
    /// there — and the few no reachable program produces at all. Each site
    /// says which of the two it is, and `Checker::expr` and
    /// `Checker::declare` hold it to that claim in debug builds: a
    /// placeholder in the type of an expression or of a binding is a bug in
    /// this pass, not a fact about the program.
    fn placeholder() -> Ty {
        Ty::Unknown(Unknown::Placeholder)
    }

    /// Whether this type carries no information, so a diagnostic about it
    /// would be a guess.
    fn is_wild(&self) -> bool {
        matches!(self, Ty::Unknown(_) | Ty::Never)
    }

    /// Whether a value given to a place of this type needs no diagnostic of
    /// its own about the place saying nothing.
    ///
    /// See `Unknown::is_accounted_for`. `Never` is here for the same
    /// reason: a place that never receives a value is one no diagnostic
    /// describes.
    fn is_accounted_for(&self) -> bool {
        match self {
            Ty::Unknown(kind) => kind.is_accounted_for(),
            Ty::Never => true,
            _ => false,
        }
    }

    /// Whether an [`Unknown::Placeholder`] is anywhere inside this type.
    ///
    /// This is what turns the classification from a convention into
    /// something the test suite holds: a placeholder stands for a position
    /// nothing reads, so finding one in a type a program can observe means
    /// the site that built it was wrong about itself.
    fn holds_placeholder(&self) -> bool {
        match self {
            Ty::Unknown(kind) => matches!(kind, Unknown::Placeholder),
            Ty::Array(inner)
            | Ty::Vector(inner)
            | Ty::Set(inner)
            | Ty::Option(inner)
            | Ty::Task(inner)
            | Ty::Shared(inner) => inner.holds_placeholder(),
            Ty::Map(k, v) | Ty::MapEntry(k, v) | Ty::Result(k, v) => {
                k.holds_placeholder() || v.holds_placeholder()
            }
            Ty::Struct(_, args) | Ty::Enum(_, args) => {
                args.iter().any(|arg| arg.holds_placeholder())
            }
            Ty::Fn(f) => f.params.iter().any(Ty::holds_placeholder) || f.ret.holds_placeholder(),
            _ => false,
        }
    }

    /// Nominal equality, with `Unknown` and `Never` equal to everything.
    ///
    /// Generic arguments are invariant: `Array<Int>` and `Array<Float>` are
    /// unrelated types, in either direction.
    fn matches(&self, other: &Ty) -> bool {
        if self.is_wild() || other.is_wild() {
            return true;
        }
        match (self, other) {
            (Ty::Array(a), Ty::Array(b))
            | (Ty::Vector(a), Ty::Vector(b))
            | (Ty::Set(a), Ty::Set(b))
            | (Ty::Option(a), Ty::Option(b))
            | (Ty::Task(a), Ty::Task(b))
            | (Ty::Shared(a), Ty::Shared(b)) => a.matches(b),
            (Ty::Map(ak, av), Ty::Map(bk, bv))
            | (Ty::MapEntry(ak, av), Ty::MapEntry(bk, bv))
            | (Ty::Result(ak, av), Ty::Result(bk, bv)) => ak.matches(bk) && av.matches(bv),
            (Ty::Struct(a, aargs), Ty::Struct(b, bargs))
            | (Ty::Enum(a, aargs), Ty::Enum(b, bargs)) => {
                a == b
                    && aargs.len() == bargs.len()
                    && aargs.iter().zip(bargs).all(|(a, b)| a.matches(b))
            }
            (Ty::Fn(a), Ty::Fn(b)) => {
                a.is_async == b.is_async
                    && a.params.len() == b.params.len()
                    && a.params.iter().zip(&b.params).all(|(a, b)| a.matches(b))
                    && a.ret.matches(&b.ret)
            }
            (Ty::Param(a), Ty::Param(b))
            | (Ty::Dyn(a), Ty::Dyn(b))
            | (Ty::Host(a), Ty::Host(b)) => a == b,
            (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
        }
    }

    /// The more informative of two types that already [`Ty::matches`], used
    /// where two branches must agree: a known type wins over `Unknown`, and
    /// any type wins over `Never`.
    ///
    /// It reaches inside a shared shape as well, because that is where the
    /// information usually is: `[[], [1]]` joins `Array<_>` with
    /// `Array<Int>`, and answering `Array<_>` would leave the array's
    /// elements unchecked for the rest of the program even though one of
    /// them said exactly what they are.
    fn join(&self, other: &Ty) -> Ty {
        match (self, other) {
            (Ty::Never, other) | (other, Ty::Never) => other.clone(),
            (Ty::Unknown(_), other) | (other, Ty::Unknown(_)) => other.clone(),
            (Ty::Array(a), Ty::Array(b)) => Ty::Array(Box::new(a.join(b))),
            (Ty::Vector(a), Ty::Vector(b)) => Ty::Vector(Box::new(a.join(b))),
            (Ty::Set(a), Ty::Set(b)) => Ty::Set(Box::new(a.join(b))),
            (Ty::Option(a), Ty::Option(b)) => Ty::Option(Box::new(a.join(b))),
            (Ty::Task(a), Ty::Task(b)) => Ty::Task(Box::new(a.join(b))),
            (Ty::Shared(a), Ty::Shared(b)) => Ty::Shared(Box::new(a.join(b))),
            (Ty::Map(ak, av), Ty::Map(bk, bv)) => {
                Ty::Map(Box::new(ak.join(bk)), Box::new(av.join(bv)))
            }
            (Ty::MapEntry(ak, av), Ty::MapEntry(bk, bv)) => {
                Ty::MapEntry(Box::new(ak.join(bk)), Box::new(av.join(bv)))
            }
            (Ty::Result(ak, av), Ty::Result(bk, bv)) => {
                Ty::Result(Box::new(ak.join(bk)), Box::new(av.join(bv)))
            }
            (Ty::Struct(a, aargs), Ty::Struct(b, bargs))
                if a == b && aargs.len() == bargs.len() =>
            {
                Ty::Struct(
                    a.clone(),
                    aargs.iter().zip(bargs).map(|(a, b)| a.join(b)).collect(),
                )
            }
            (Ty::Enum(a, aargs), Ty::Enum(b, bargs)) if a == b && aargs.len() == bargs.len() => {
                Ty::Enum(
                    a.clone(),
                    aargs.iter().zip(bargs).map(|(a, b)| a.join(b)).collect(),
                )
            }
            (Ty::Fn(a), Ty::Fn(b))
                if a.is_async == b.is_async && a.params.len() == b.params.len() =>
            {
                Ty::func(
                    a.is_async,
                    a.params
                        .iter()
                        .zip(&b.params)
                        .map(|(a, b)| a.join(b))
                        .collect(),
                    a.ret.join(&b.ret),
                )
            }
            _ => self.clone(),
        }
    }

    /// This type as it stands when `generics` are the arguments `args`.
    ///
    /// A declared type's fields and an enum case's payload are recorded once,
    /// in terms of the type parameters the declaration binds:
    /// `Box<T>`'s field is a `T` however many `Box<Int>`s a program holds. A
    /// consumer holding a *use* — a `Ty::Struct(name, args)` — completes them
    /// with this. `args` shorter than `generics` leaves the rest unknown,
    /// which is what an unsettled type argument already means.
    pub fn instantiate(&self, generics: &[Arc<str>], args: &[Ty]) -> Ty {
        self.substitute(&substitution(generics, args))
    }

    /// Replaces every type parameter bound in `subst`, leaving the rest.
    fn substitute(&self, subst: &BTreeMap<Arc<str>, Ty>) -> Ty {
        if subst.is_empty() {
            return self.clone();
        }
        match self {
            Ty::Param(name) => subst.get(name).cloned().unwrap_or_else(|| self.clone()),
            Ty::Array(inner) => Ty::Array(Box::new(inner.substitute(subst))),
            Ty::Vector(inner) => Ty::Vector(Box::new(inner.substitute(subst))),
            Ty::Set(inner) => Ty::Set(Box::new(inner.substitute(subst))),
            Ty::Option(inner) => Ty::Option(Box::new(inner.substitute(subst))),
            Ty::Task(inner) => Ty::Task(Box::new(inner.substitute(subst))),
            Ty::Shared(inner) => Ty::Shared(Box::new(inner.substitute(subst))),
            Ty::Map(k, v) => Ty::Map(Box::new(k.substitute(subst)), Box::new(v.substitute(subst))),
            Ty::MapEntry(k, v) => {
                Ty::MapEntry(Box::new(k.substitute(subst)), Box::new(v.substitute(subst)))
            }
            Ty::Result(t, e) => {
                Ty::Result(Box::new(t.substitute(subst)), Box::new(e.substitute(subst)))
            }
            Ty::Struct(name, args) => Ty::Struct(
                name.clone(),
                args.iter().map(|a| a.substitute(subst)).collect(),
            ),
            Ty::Enum(name, args) => Ty::Enum(
                name.clone(),
                args.iter().map(|a| a.substitute(subst)).collect(),
            ),
            Ty::Fn(f) => Ty::func(
                f.is_async,
                f.params.iter().map(|p| p.substitute(subst)).collect(),
                f.ret.substitute(subst),
            ),
            other => other.clone(),
        }
    }
}

/// Renders a type in the source form it would be written in, so a diagnostic
/// shows the type the reader wrote.
impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // An unknown type reads as `_` because that is how an
            // unconstrained position is written in Cove source.
            Ty::Unknown(_) => f.write_str("_"),
            Ty::Never => f.write_str("!"),
            Ty::Unit => f.write_str("()"),
            Ty::Bool => f.write_str("Bool"),
            Ty::Int => f.write_str("Int"),
            Ty::Float => f.write_str("Float"),
            Ty::Str => f.write_str("String"),
            Ty::Duration => f.write_str("Duration"),
            Ty::Error => f.write_str("Error"),
            Ty::Range => f.write_str("Range"),
            Ty::Scope => f.write_str("Scope"),
            Ty::Array(inner) => write!(f, "Array<{inner}>"),
            Ty::Vector(inner) => write!(f, "Vector<{inner}>"),
            Ty::Set(inner) => write!(f, "Set<{inner}>"),
            Ty::Option(inner) => write!(f, "Option<{inner}>"),
            Ty::Task(inner) => write!(f, "Task<{inner}>"),
            Ty::Shared(inner) => write!(f, "Shared<{inner}>"),
            Ty::Map(k, v) => write!(f, "Map<{k}, {v}>"),
            Ty::MapEntry(k, v) => write!(f, "MapEntry<{k}, {v}>"),
            Ty::Result(t, e) => write!(f, "Result<{t}, {e}>"),
            Ty::Param(name) => f.write_str(name),
            Ty::Host(name) => f.write_str(name),
            Ty::Dyn(name) => write!(f, "dyn {name}"),
            Ty::Struct(name, args) | Ty::Enum(name, args) => {
                f.write_str(name)?;
                if !args.is_empty() {
                    let args: Vec<String> = args.iter().map(Ty::to_string).collect();
                    write!(f, "<{}>", args.join(", "))?;
                }
                Ok(())
            }
            Ty::Fn(func) => {
                if func.is_async {
                    f.write_str("async ")?;
                }
                let params: Vec<String> = func.params.iter().map(Ty::to_string).collect();
                write!(f, "fn({})", params.join(", "))?;
                if func.ret != Ty::Unit {
                    write!(f, " -> {}", func.ret)?;
                }
                Ok(())
            }
        }
    }
}

// -------------------------------------------------------------- signatures

/// One parameter of a declared function, method, or synthesized struct
/// initializer.
#[derive(Clone, Debug)]
struct ParamSig {
    name: String,
    ty: Ty,
    variadic: bool,
    has_default: bool,
    /// Whether the declaration wrote `var` before this parameter's name, so
    /// that a body binds it as the mutable place it is.
    ///
    /// Only a `fn` declaration, a method and a lambda can write one; a
    /// struct's field, a builtin's parameter, a host operation's and the
    /// parameters read off a function *type* are all `false`, because none
    /// of those is written with a marking at all.
    is_var: bool,
    span: Span,
}

/// One trait a type parameter is bounded by, and where the bound was
/// written, so an unsatisfied bound can point at it.
#[derive(Clone, Debug)]
struct TraitBound {
    name: Arc<str>,
    span: Span,
}

/// A declared function's or method's type, as written at its boundary.
#[derive(Clone, Debug)]
struct FnSig {
    /// Type parameters this signature binds, rigid inside its own body.
    generics: Vec<Arc<str>>,
    /// The traits each type parameter is bounded by. A parameter with no
    /// bound is absent, which is also what makes a method call on it an
    /// error: it has no operations.
    bounds: BTreeMap<Arc<str>, Vec<TraitBound>>,
    params: Vec<ParamSig>,
    ret: Ty,
    ret_span: Span,
    is_async: bool,
    /// The type of `self`, for a method.
    receiver: Option<Ty>,
    /// Whether that receiver is written `var self`, which is what makes a
    /// call through it a write to the caller's place.
    receiver_is_var: bool,
}

impl FnSig {
    /// This signature as a module importing it sees it: every nominal name
    /// `module` declares rewritten into its canonical `module.Name` form.
    fn qualified(&self, module: &str) -> FnSig {
        FnSig {
            generics: self.generics.clone(),
            bounds: self
                .bounds
                .iter()
                .map(|(param, bounds)| {
                    let bounds = bounds
                        .iter()
                        .map(|bound| TraitBound {
                            name: qualified_name(&bound.name, module),
                            span: bound.span,
                        })
                        .collect();
                    (param.clone(), bounds)
                })
                .collect(),
            params: self
                .params
                .iter()
                .map(|param| ParamSig {
                    ty: qualify(&param.ty, module),
                    ..param.clone()
                })
                .collect(),
            ret: qualify(&self.ret, module),
            ret_span: self.ret_span,
            is_async: self.is_async,
            receiver: self.receiver.as_ref().map(|ty| qualify(ty, module)),
            receiver_is_var: self.receiver_is_var,
        }
    }

    /// The type of this function used as a value, which is what a bare
    /// reference to it evaluates to.
    fn as_value(&self) -> Ty {
        Ty::func(
            self.is_async,
            self.params.iter().map(|p| p.ty.clone()).collect(),
            self.ret.clone(),
        )
    }
}

/// A struct's fields, in declaration order.
#[derive(Clone, Debug)]
struct StructSig {
    generics: Vec<Arc<str>>,
    fields: Vec<ParamSig>,
    /// `export opaque struct`: the fields below belong to the declaring
    /// module alone.
    ///
    /// They are still recorded, because the declaring module's own bodies
    /// are checked against them and a field's type still has to resolve.
    /// What `opaque` changes is who may name one: see [`foreign_type`].
    opaque: bool,
}

/// An enum's cases, in declaration order.
#[derive(Clone, Debug)]
struct EnumSig {
    generics: Vec<Arc<str>>,
    cases: Vec<CaseSig>,
}

#[derive(Clone, Debug)]
struct CaseSig {
    name: String,
    payload: Vec<Ty>,
    span: Span,
}

/// What a binding in a body means.
#[derive(Clone, Debug)]
struct Binding {
    ty: Ty,
    /// Whether source may write the place this name binds.
    ///
    /// `var` and a `var` parameter make one; `let`, an ordinary parameter, a
    /// variadic parameter, a pattern's binding, a `for` header's binding, a
    /// `scope`'s name and a local `fn` do not. It is `is_var` where a
    /// declaration writes one and `false` everywhere else, which is the same
    /// answer `Place::binding` gives each of them in
    /// `crates/cove-runtime/src/interp.rs`.
    mutable: bool,
}

/// Where an expectation came from, so a mismatch can point at the
/// declaration that imposed it as well as the expression that broke it.
#[derive(Clone, Debug)]
struct Origin {
    span: Span,
    label: String,
}

/// A type an expression is checked against, and the declaration that asks
/// for it.
#[derive(Clone, Debug)]
struct Expected {
    ty: Ty,
    origin: Option<Origin>,
}

impl Expected {
    fn new(ty: Ty, span: Span, label: impl Into<String>) -> Expected {
        Expected {
            ty,
            origin: Some(Origin {
                span,
                label: label.into(),
            }),
        }
    }

    /// An expectation with nothing to point at, made of an unknown the
    /// checker has already accounted for.
    ///
    /// It never disagrees with anything, so it has no mismatch to label. What
    /// it does carry is the answer to *why* the place says nothing, which is
    /// what `Checker::accounted_for` reads: the diagnostic explaining that
    /// silence has already been given, or belongs somewhere else entirely.
    fn abstained(ty: Ty) -> Expected {
        Expected { ty, origin: None }
    }
}

// ---------------------------------------------------------------- checking

/// Checks one module against its own declarations, its imports, and the
/// builtins.
///
/// # How a declaration is named
///
/// Every table below is keyed by a declaration's *canonical name*: the bare
/// name for a declaration this module makes, and `module.Name` for one that
/// belongs to another module — whether this module imported it or only ever
/// meets it as the type of an imported function's result. One declaration
/// therefore has exactly one key here, so `Ty::Struct` and `Ty::Enum` can
/// compare two types by name without confusing two modules' `Config`.
/// [`Checker::key`] turns a name as written into that key.
struct Checker<'a> {
    module: &'a ResolvedModule,
    /// The whole program, for the declarations of the modules this one
    /// imports from.
    program: &'a Program,
    /// The host modules this compilation can see: the shipped ones, and any
    /// an embedder handed to the compiler. A call into a module that is not
    /// here is the one call this checker leaves to the boundary.
    schemas: &'a HostSchemas,
    diagnostics: Vec<Diagnostic>,
    functions: BTreeMap<String, FnSig>,
    methods: BTreeMap<(String, String), FnSig>,
    structs: BTreeMap<String, StructSig>,
    enums: BTreeMap<String, EnumSig>,
    /// Expanded type aliases, resolved once so the names inside an alias are
    /// reported once however many times it is used.
    aliases: BTreeMap<String, (Vec<Arc<str>>, Ty)>,
    /// Aliases currently being expanded, to catch `type A = A`.
    expanding: Vec<String>,
    /// Every trait the module declares, with the signature of each of its
    /// methods.
    traits: BTreeMap<String, BTreeMap<String, FnSig>>,
    /// Every declared conformance, as `(trait, type)`. Conformance is
    /// explicit, so this set is complete.
    conformances: BTreeSet<(String, String)>,
    /// Type parameters in scope, innermost last.
    type_params: Vec<Arc<str>>,
    /// The bounds of the type parameters currently in scope.
    bounds: BTreeMap<Arc<str>, Vec<TraitBound>>,
    scopes: Vec<BTreeMap<String, Binding>>,
    /// The lowest scope that belongs to the function value currently being
    /// checked.
    ///
    /// A lambda and a local `fn` are checked with the scopes around them
    /// still standing, because a body reads the names it closes over. What
    /// it closes over it holds a *copy* of: `Env::declare_capture` builds a
    /// `Place::binding(value, false)`, so a captured `var` is read-only
    /// inside the closure however it was declared outside. A name found
    /// below this index is therefore a capture, and no capture is writable.
    capture_floor: usize,
    /// The declared return type of the function whose body is being checked,
    /// and where it was written.
    ret: Ty,
    ret_span: Span,
    /// The field expression currently being checked as the target of an
    /// assignment, if any.
    ///
    /// A place is checked by the same walk as a value, so nothing about
    /// `x.field` says whether it is being read or written — the assignment
    /// above it is the only thing that knows. Recording its span is enough
    /// to tell the two apart, because the reads on the way to a place carry
    /// their own: in `a.b.c = v` the target is `a.b.c` and `a.b` is a read
    /// like any other.
    assigned_place: Option<Span>,
    /// Whether anything written says what the function being checked
    /// produces.
    ///
    /// It is true for every declaration, which writes its return type or
    /// returns `Unit`, and for a function value the place holding it typed.
    /// It is false only for a lambda nothing expects, whose result is
    /// whatever its body's value turns out to be — and which therefore has
    /// nothing for an early `return` to agree with.
    ret_stated: bool,
    /// Whether the walk currently running is a `Checker::probe`, whose
    /// diagnostics are discarded.
    probing: bool,
    /// What this checker settled about each expression it walked.
    ///
    /// It is written to and never read by the walk, which is what makes it
    /// unable to change a diagnostic. [`Facts`] says who reads it and why.
    facts: Facts,
}

impl<'a> Checker<'a> {
    fn new(
        module: &'a ResolvedModule,
        program: &'a Program,
        schemas: &'a HostSchemas,
    ) -> Checker<'a> {
        Checker {
            module,
            program,
            schemas,
            diagnostics: Vec::new(),
            functions: BTreeMap::new(),
            methods: BTreeMap::new(),
            structs: BTreeMap::new(),
            enums: BTreeMap::new(),
            aliases: BTreeMap::new(),
            expanding: Vec::new(),
            traits: BTreeMap::new(),
            conformances: module
                .conformances
                .values()
                .map(|conformance| conformance_key(module, conformance))
                .collect(),
            type_params: Vec::new(),
            bounds: BTreeMap::new(),
            scopes: Vec::new(),
            capture_floor: 0,
            // No body is being checked yet, and every one of them sets all
            // three of these before it can reach a `return`.
            ret: Ty::placeholder(),
            ret_span: Span::new(cove_diag::FileId(0), 0, 0),
            assigned_place: None,
            ret_stated: false,
            probing: false,
            facts: Facts::default(),
        }
    }

    /// The Host API schema of the module named `module`, or `None` when this
    /// compilation was shown none for it.
    ///
    /// Every abstention this pass makes about a host goes through here, so
    /// this is the one seam an embedder-supplied schema has to reach. It
    /// answers from the [`HostSchemas`] the compilation was given: the
    /// modules an embedder handed over first, then — unless the set was
    /// built with `HostSchemas::only` — the ones the toolchain ships. That
    /// is what turns each of those abstentions into an ordinary check
    /// without any other part of this pass changing, and what a name no
    /// table answers for still gets is a [`Ty::dynamic_boundary`].
    ///
    /// It is a method taking `&self` and answering with an *owned*
    /// [`ModuleSchema`] rather than a free function answering with a
    /// `&'static` one, and both halves of that are the seam. A table an
    /// embedder registers is owned by the compilation, not by the binary, so
    /// it has no `'static` borrow to hand back; and a set of them is
    /// something the checker is given rather than something it looks up in a
    /// global. `ModuleSchema` is [`Copy`] and everything inside it is
    /// `'static`, so answering by value costs nothing and the schemas
    /// reached *through* the answer — operations, types, resources — are
    /// still `&'static`.
    fn host_schema(&self, module: &str) -> Option<ModuleSchema> {
        self.schemas.module(module)
    }

    /// The schema of the host type `qualified` names, when it is one a host
    /// hands over rather than one it keeps.
    fn host_declared_type(&self, qualified: &str) -> Option<&'static TypeSchema> {
        let (module, name) = qualified.split_once('.')?;
        self.host_schema(module)?.declared_type(name)
    }

    /// The schema of the host resource `qualified` names.
    fn host_resource(&self, qualified: &str) -> Option<&'static ResourceSchema> {
        let (module, name) = qualified.split_once('.')?;
        self.host_schema(module)?.resource(name)
    }

    /// Walks something to find out its type, reporting nothing.
    ///
    /// A form whose type a sibling settles has to be walked once to learn
    /// that and once to be checked against it, and only the second walk
    /// describes the program. Nothing else about a walk is observable —
    /// diagnostics are appended to one vector that nothing reads until the
    /// pass ends, and every scope a walk pushes it pops — so truncating that
    /// vector is all undoing one takes.
    ///
    /// A probe never probes again. One level of it finds the same type a
    /// nested one would, and disabling the nested ones is what keeps a walk
    /// of nested literals from doubling per level.
    fn probe<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let mark = self.diagnostics.len();
        let outer = std::mem::replace(&mut self.probing, true);
        let found = f(self);
        self.probing = outer;
        self.diagnostics.truncate(mark);
        found
    }

    // ------------------------------------------------------------ imports

    /// The table key a name written in this module refers to: the name
    /// itself when this module declares it, and `module.Name` when a `use`
    /// imported it from `module`.
    ///
    /// Resolution refuses a `use` that binds a name the importing module
    /// also declares, so at most one of the two answers ever applies.
    fn key(&self, name: &str) -> String {
        match self.module.imports.get(name) {
            Some(owner) => format!("{owner}.{name}"),
            None => name.to_string(),
        }
    }

    /// Whether `name` as written names a declaration of another module,
    /// which is what makes [`Checker::key`] answer something other than
    /// `name`.
    fn is_imported(&self, name: &str) -> bool {
        self.module.imports.contains_key(name)
    }

    /// The key `head.name` refers to when `head` is a module imported whole
    /// and that module exports `name`.
    ///
    /// A module-private declaration is reported rather than resolved: a
    /// qualified name reaches exactly what a `use` of it would.
    fn qualified_key(&mut self, head: &str, name: &str, span: Span) -> Option<String> {
        let owner_name = self.module.module_imports.get(head)?;
        let owner = self.program.modules.get(owner_name)?;
        let exported = match owner.exported(name) {
            Some(exported) => exported,
            None => {
                self.diagnostics.push(
                    Diagnostic::error(
                        UNKNOWN_MEMBER,
                        format!("module `{owner_name}` declares no `{name}`"),
                    )
                    .at(span)
                    .rule(
                        "A qualified name reaches an exported declaration of the module it names.",
                    )
                    .help(format!(
                        "module `{owner_name}` exports {}",
                        list(&owner.exports())
                    )),
                );
                return None;
            }
        };
        if !exported {
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_MEMBER,
                    format!("`{name}` is declared by module `{owner_name}`, but is not exported"),
                )
                .at(span)
                .rule("An `export` declaration is public; other declarations are module-private.")
                .help(format!(
                    "write `export` on `{name}` in module `{owner_name}`, or name something else"
                )),
            );
            return None;
        }
        Some(format!("{owner_name}.{name}"))
    }

    // ------------------------------------------------------- opaque types

    /// The exported operations of the type `module.type_name` that a caller
    /// outside `module` may use, written as it would call them.
    ///
    /// `with_receiver` picks between the two halves of the interface an
    /// opaque type has: its associated functions, which are how a caller
    /// builds one, and its methods, which are how a caller reads one.
    ///
    /// Only what `module` itself declares counts. `methods_of` also answers
    /// with the methods other modules attach to the type by conforming it to
    /// a trait of their own, and one of those may be the very method being
    /// written — a help that says "call `show()`" inside the body of `show`
    /// is no help at all. What a caller needs is what the declaring module
    /// published.
    fn exported_operations(
        &self,
        module: &str,
        type_name: &str,
        with_receiver: bool,
    ) -> Vec<String> {
        self.program
            .methods_of(module, type_name)
            .into_iter()
            .filter(|declared| {
                declared.module == module
                    && declared.entry.exported
                    && declared.entry.decl.receiver.is_some() == with_receiver
            })
            .map(|declared| {
                if with_receiver {
                    format!("{}()", declared.name)
                } else {
                    format!("{type_name}.{}()", declared.name)
                }
            })
            .collect()
    }

    /// The `help` line that points a caller at what an opaque type does
    /// export, or at the fact that it exports nothing of that kind.
    fn opaque_help(
        &self,
        module: &str,
        type_name: &str,
        with_receiver: bool,
        instead: &str,
        nothing: &str,
    ) -> String {
        let operations = self.exported_operations(module, type_name, with_receiver);
        if operations.is_empty() {
            format!(
                "module `{module}` exports no {nothing} for `{type_name}`, so ask it to export one"
            )
        } else {
            format!("{instead} {}", list(&operations))
        }
    }

    /// Reports a use of an opaque type's representation from outside the
    /// module that declares it, and answers whether it did.
    ///
    /// `export opaque struct` publishes a name and the methods declared for
    /// it, so a caller reaching for a field or for the synthesized labeled
    /// constructor is reaching for something that was deliberately not
    /// exported. The declaring module is unaffected: its own key for the
    /// type is bare, which is what [`foreign_type`] tests.
    fn reject_opaque_field(
        &mut self,
        key: &str,
        sig: &StructSig,
        field: &str,
        usage: FieldUse,
        span: Span,
    ) -> bool {
        if !sig.opaque {
            return false;
        }
        let Some((module, type_name)) = foreign_type(key) else {
            return false;
        };
        let help = self.opaque_help(module, type_name, true, usage.correction(), "method");
        self.diagnostics.push(
            Diagnostic::error(
                OPAQUE_FIELD,
                format!(
                    "`{type_name}` is opaque here, so its field `{field}` cannot be {}",
                    usage.refused()
                ),
            )
            .at(span)
            .rule(
                "An `export opaque struct` exports its name and its exported methods; its fields belong to the module that declares it.",
            )
            .help(help),
        );
        true
    }

    /// Reports a call to the synthesized labeled constructor of an opaque
    /// type from outside the module that declares it, and answers whether it
    /// did. See [`Checker::reject_opaque_field`].
    fn reject_opaque_construction(&mut self, key: &str, sig: &StructSig, span: Span) -> bool {
        if !sig.opaque {
            return false;
        }
        let Some((module, type_name)) = foreign_type(key) else {
            return false;
        };
        let help = self.opaque_help(
            module,
            type_name,
            false,
            "build the value through an exported associated function, such as",
            "constructor",
        );
        self.diagnostics.push(
            Diagnostic::error(
                OPAQUE_CONSTRUCTION,
                format!("`{type_name}` is opaque here, so it cannot be built field by field"),
            )
            .at(span)
            .rule(
                "An `export opaque struct` does not export the labeled constructor its fields synthesize; only the module that declares it may write one.",
            )
            .help(help),
        );
        true
    }

    /// Brings every declaration of every module this one imports from into
    /// its tables, under the canonical `module.Name` keys.
    ///
    /// A module's whole environment is brought in, not only the declarations
    /// a `use` named: a type reached as the result of an imported function
    /// must still have fields and methods, even when this module never
    /// names it. What a `use` decides is which of them this module can
    /// *write*, which is [`Checker::key`]'s business, not this one's.
    fn import(&mut self, envs: &BTreeMap<&str, ImportEnv>) {
        for dependency in self.module.dependencies() {
            let Some(env) = envs.get(dependency) else {
                continue;
            };
            self.structs
                .extend(env.structs.iter().map(|(k, v)| (k.clone(), v.clone())));
            self.enums
                .extend(env.enums.iter().map(|(k, v)| (k.clone(), v.clone())));
            self.aliases
                .extend(env.aliases.iter().map(|(k, v)| (k.clone(), v.clone())));
            self.functions
                .extend(env.functions.iter().map(|(k, v)| (k.clone(), v.clone())));
            self.methods
                .extend(env.methods.iter().map(|(k, v)| (k.clone(), v.clone())));
            self.traits
                .extend(env.traits.iter().map(|(k, v)| (k.clone(), v.clone())));
            self.conformances.extend(env.conformances.iter().cloned());
        }
    }

    /// What this module offers the modules that import it, once its own
    /// declarations are resolved.
    fn export_env(&self) -> ImportEnv {
        let module = self.module.name.as_str();
        let key = |name: &String| {
            if name.contains('.') {
                name.clone()
            } else {
                format!("{module}.{name}")
            }
        };
        ImportEnv {
            structs: self
                .structs
                .iter()
                .map(|(name, sig)| {
                    (
                        key(name),
                        StructSig {
                            generics: sig.generics.clone(),
                            fields: sig
                                .fields
                                .iter()
                                .map(|field| ParamSig {
                                    ty: qualify(&field.ty, module),
                                    ..field.clone()
                                })
                                .collect(),
                            opaque: sig.opaque,
                        },
                    )
                })
                .collect(),
            enums: self
                .enums
                .iter()
                .map(|(name, sig)| {
                    (
                        key(name),
                        EnumSig {
                            generics: sig.generics.clone(),
                            cases: sig
                                .cases
                                .iter()
                                .map(|case| CaseSig {
                                    payload: case
                                        .payload
                                        .iter()
                                        .map(|ty| qualify(ty, module))
                                        .collect(),
                                    ..case.clone()
                                })
                                .collect(),
                        },
                    )
                })
                .collect(),
            aliases: self
                .aliases
                .iter()
                .map(|(name, (generics, ty))| (key(name), (generics.clone(), qualify(ty, module))))
                .collect(),
            functions: self
                .functions
                .iter()
                .map(|(name, sig)| (key(name), sig.qualified(module)))
                .collect(),
            methods: self
                .methods
                .iter()
                .map(|((type_name, name), sig)| {
                    ((key(type_name), name.clone()), sig.qualified(module))
                })
                .collect(),
            traits: self
                .traits
                .iter()
                .map(|(name, methods)| {
                    let methods = methods
                        .iter()
                        .map(|(name, sig)| (name.clone(), sig.qualified(module)))
                        .collect();
                    (key(name), methods)
                })
                .collect(),
            conformances: self
                .conformances
                .iter()
                .map(|(trait_name, type_name)| (key(trait_name), key(type_name)))
                .collect(),
        }
    }

    /// Checks every body of this module, once every signature it can see is
    /// resolved.
    fn check_bodies(&mut self) {
        let fn_names: Vec<String> = self.module.functions.keys().cloned().collect();
        for name in fn_names {
            let decl = self.module.functions[&name].decl.clone();
            let sig = self.functions[&name].clone();
            self.check_body(&decl, &sig);
        }
        let method_keys: Vec<(String, String)> = self.module.methods.keys().cloned().collect();
        for key in method_keys {
            // A trait's default body is checked once against `Self`, below,
            // not once per conformance.
            if self.module.methods[&key].from_trait_default.is_some() {
                continue;
            }
            let decl = self.module.methods[&key].decl.clone();
            // The signature is filed under the type's canonical key, which
            // differs from the name written here when the conformance is for
            // an imported type.
            let sig = self.methods[&(self.key(&key.0), key.1.clone())].clone();
            self.check_body(&decl, &sig);
        }
        self.check_trait_defaults();
    }

    /// Checks every trait's default method bodies, once each, with `self`
    /// typed as a rigid `Self` bounded by that trait.
    ///
    /// A default body is written against its trait's own interface and
    /// nothing else. Checking it once against `Self: Trait` is what makes
    /// that true: checked once per conformance instead, it could reach a
    /// conforming type's fields, and conformance would be structural after
    /// all.
    fn check_trait_defaults(&mut self) {
        let trait_names: Vec<String> = self.module.traits.keys().cloned().collect();
        for trait_name in trait_names {
            let decl = self.module.traits[&trait_name].decl.clone();
            let self_param: Arc<str> = "Self".into();
            for method in &decl.methods {
                let sig = self.traits[&trait_name][&method.name.node].clone();
                self.type_params = vec![self_param.clone()];
                self.bounds = BTreeMap::from([(
                    self_param.clone(),
                    vec![TraitBound {
                        name: trait_name.as_str().into(),
                        span: decl.name.span,
                    }],
                )]);
                self.ret = sig.ret.clone();
                self.ret_span = sig.ret_span;
                self.ret_stated = true;
                self.scopes.push(BTreeMap::new());
                if let Some(receiver) = method.receiver {
                    self.declare("self", Ty::Param(self_param.clone()), receiver.is_var);
                }
                for param in &sig.params {
                    let ty = if param.variadic {
                        Ty::Array(Box::new(param.ty.clone()))
                    } else {
                        param.ty.clone()
                    };
                    self.declare(&param.name, ty, param.is_var);
                }
                for (param, declared) in method.params.iter().zip(&sig.params) {
                    if let Some(default) = &param.default {
                        let expected = Expected::new(
                            declared.ty.clone(),
                            param.name.span,
                            format!("this parameter is `{}`", declared.ty),
                        );
                        self.expr(default, Some(&expected));
                    }
                }
                if let Some(body) = &method.default {
                    let expected = Expected::new(
                        sig.ret.clone(),
                        sig.ret_span,
                        if method.return_type.is_some() {
                            format!("the declared return type is `{}`", sig.ret)
                        } else {
                            "this method declares no return type, so it returns `()`".to_string()
                        },
                    );
                    self.block(body, Some(&expected));
                }
                self.scopes.pop();
                self.type_params.clear();
                self.bounds.clear();
            }
        }
    }

    /// Resolves every written type of every declaration, once.
    ///
    /// A type written once is reported once, however many bodies mention it,
    /// because a body reads the resolved signature rather than the syntax.
    fn prepare(&mut self) {
        let alias_names: Vec<String> = self.module.aliases.keys().cloned().collect();
        for name in alias_names {
            self.alias(&name);
        }

        // Trait signatures come before everything else: a bound, a `dyn`, and
        // a conformance check all read them.
        let trait_names: Vec<String> = self.module.traits.keys().cloned().collect();
        for name in trait_names {
            let decl = self.module.traits[&name].decl.clone();
            let methods = decl
                .methods
                .iter()
                .map(|method| (method.name.node.clone(), self.trait_method_sig(method)))
                .collect();
            self.traits.insert(name, methods);
        }

        let struct_names: Vec<String> = self.module.structs.keys().cloned().collect();
        for name in struct_names {
            let entry = &self.module.structs[&name];
            let (decl, opaque) = (entry.decl.clone(), entry.opaque);
            let sig = self.struct_sig(&decl, opaque);
            self.record_struct_signature(&decl, &sig);
            self.structs.insert(name, sig);
        }

        let enum_names: Vec<String> = self.module.enums.keys().cloned().collect();
        for name in enum_names {
            let decl = self.module.enums[&name].decl.clone();
            let sig = self.enum_sig(&decl);
            self.record_case_signatures(&decl, &sig);
            self.enums.insert(name, sig);
        }

        let fn_names: Vec<String> = self.module.functions.keys().cloned().collect();
        for name in fn_names {
            let decl = self.module.functions[&name].decl.clone();
            let sig = self.fn_sig(&decl, None);
            self.functions.insert(name, sig);
        }

        // A method's table key names the type's own module: a conformance
        // this module declares for an imported type extends *that* type, so
        // its methods have to be found under the same key everyone else
        // reaches it by.
        let method_keys: Vec<(String, String)> = self.module.methods.keys().cloned().collect();
        for key in method_keys {
            let decl = self.module.methods[&key].decl.clone();
            let sig = self.fn_sig(&decl, Some(&key.0));
            self.methods.insert((self.key(&key.0), key.1), sig);
        }

        self.check_conformance_signatures();
    }

    /// The signature of one trait method.
    ///
    /// A trait binds no type parameters of its own in the MVP, so its methods
    /// are checked in an empty generic scope. The receiver type is left
    /// `Unknown` because it is decided by the call site: `T` through a bound,
    /// `dyn Trait` through a trait object, and the concrete type through a
    /// conformance.
    fn trait_method_sig(&mut self, method: &TraitMethod) -> FnSig {
        let outer = std::mem::take(&mut self.type_params);
        self.check_variadic_shape(&method.params);
        let params = method
            .params
            .iter()
            .map(|param| self.param_sig(param))
            .collect::<Vec<_>>();
        let ret = match &method.return_type {
            Some(ty) => self.resolve(ty),
            None => Ty::Unit,
        };
        let ret_span = match &method.return_type {
            Some(ty) => ty.span,
            None => method.name.span,
        };
        self.type_params = outer;
        FnSig {
            generics: Vec::new(),
            bounds: BTreeMap::new(),
            params,
            ret,
            ret_span,
            is_async: method.is_async,
            receiver: method.receiver.map(|_| Ty::placeholder()),
            receiver_is_var: method.receiver.is_some_and(|receiver| receiver.is_var),
        }
    }

    /// Checks that every method a conformance supplies has the signature its
    /// trait declares.
    ///
    /// Resolution already rejected a method the trait does not declare and a
    /// declared method the conformance does not supply; what is left is
    /// whether the two agree on parameters, result, and receiver. They must,
    /// because a call through a bound or through `dyn Trait` is checked
    /// against the trait's signature and dispatched to this one.
    fn check_conformance_signatures(&mut self) {
        // Only the conformances this module declares: one it merely imported
        // was checked where it was written, and its methods are not this
        // module's to fix.
        let conformances: Vec<(String, String, String, String)> = self
            .module
            .conformances
            .values()
            .map(|conformance| {
                let (trait_key, type_key) = conformance_key(self.module, conformance);
                (
                    trait_key,
                    type_key,
                    conformance.trait_name.clone(),
                    conformance.type_name.clone(),
                )
            })
            .collect();
        for (trait_key, type_key, trait_name, written_type) in conformances {
            let Some(entry) = self.trait_entry(&trait_key) else {
                continue;
            };
            let type_name = written_type;
            let trait_decl = entry.decl.clone();
            for method in &trait_decl.methods {
                let key = (type_key.clone(), method.name.node.clone());
                let Some(found) = self.methods.get(&key).cloned() else {
                    continue;
                };
                let Some(declared) = self.traits[&trait_key].get(&method.name.node).cloned() else {
                    continue;
                };
                let Some(reason) = signature_difference(&declared, &found) else {
                    continue;
                };
                let span = self.module.methods[&(type_name.clone(), method.name.node.clone())]
                    .decl
                    .name
                    .span;
                self.diagnostics.push(
                    Diagnostic::error(
                        CONFORMANCE_SIGNATURE,
                        format!(
                            "`{type_name}.{}` does not match the signature `{trait_name}` declares: {reason}",
                            method.name.node
                        ),
                    )
                    .at(span)
                    .label(
                        method.name.span,
                        format!("`{trait_name}` declares {}", trait_signature(&declared, &method.name.node)),
                    )
                    .rule("A conformance's method has exactly the signature its trait declares, because a call through a bound or through `dyn Trait` is checked against the trait and dispatched to the conformance.")
                    .help(format!(
                        "write `{}`",
                        trait_signature(&declared, &method.name.node)
                    )),
                );
            }
        }
    }

    /// Checks one function's or method's body against its declared return
    /// type.
    ///
    /// A function with no `->` returns `Unit`, so its body's value must be
    /// `Unit` too.
    fn check_body(&mut self, decl: &FnDecl, sig: &FnSig) {
        self.record_signature(decl, sig);
        self.type_params = sig.generics.clone();
        self.bounds = sig.bounds.clone();
        self.ret = sig.ret.clone();
        self.ret_span = sig.ret_span;
        self.ret_stated = true;
        self.scopes.push(BTreeMap::new());
        if let Some(receiver) = &sig.receiver {
            // `var self` is the one receiver a body may write through, and
            // it is written at the declaration exactly as a `var` parameter
            // is.
            let is_var = decl.receiver.is_some_and(|receiver| receiver.is_var);
            self.declare("self", receiver.clone(), is_var);
        }
        for param in &sig.params {
            let ty = if param.variadic {
                Ty::Array(Box::new(param.ty.clone()))
            } else {
                param.ty.clone()
            };
            self.declare(&param.name, ty, param.is_var);
        }
        for (param, declared) in decl.params.iter().zip(&sig.params) {
            if let Some(default) = &param.default {
                let expected = Expected::new(
                    declared.ty.clone(),
                    param.name.span,
                    format!("this parameter is `{}`", declared.ty),
                );
                self.expr(default, Some(&expected));
            }
        }
        let expected = Expected::new(
            sig.ret.clone(),
            sig.ret_span,
            if decl.return_type.is_some() {
                format!("the declared return type is `{}`", sig.ret)
            } else {
                "this function declares no return type, so it returns `()`".to_string()
            },
        );
        self.block(&decl.body, Some(&expected));
        self.scopes.pop();
        self.type_params.clear();
        self.bounds.clear();
    }

    // ------------------------------------------------------ declarations

    fn struct_sig(&mut self, decl: &StructDecl, opaque: bool) -> StructSig {
        let outer = std::mem::take(&mut self.type_params);
        self.reject_bounds(&decl.generics, "struct");
        let generics = self.enter_generics(&decl.generics);
        let fields = decl
            .fields
            .iter()
            .map(|field| ParamSig {
                name: field.name.node.clone(),
                ty: self.resolve(&field.ty),
                variadic: false,
                has_default: false,
                is_var: false,
                span: field.name.span,
            })
            .collect();
        self.type_params = outer;
        StructSig {
            generics,
            fields,
            opaque,
        }
    }

    fn enum_sig(&mut self, decl: &EnumDecl) -> EnumSig {
        let outer = std::mem::take(&mut self.type_params);
        self.reject_bounds(&decl.generics, "enum");
        let generics = self.enter_generics(&decl.generics);
        let cases = decl
            .cases
            .iter()
            .map(|case| CaseSig {
                name: case.name.node.clone(),
                payload: case.payload.iter().map(|ty| self.resolve(ty)).collect(),
                span: case.name.span,
            })
            .collect();
        self.type_params = outer;
        EnumSig { generics, cases }
    }

    /// The signature of a free function, or of a method of `receiver_type`.
    ///
    /// A method's type parameters are the ones its type declares. Resolution
    /// does not record an `impl` block's own parameter list, so an `impl`
    /// that renames its type's parameters is not supported.
    fn fn_sig(&mut self, decl: &FnDecl, receiver_type: Option<&str>) -> FnSig {
        let mut type_generics: Vec<GenericParam> = Vec::new();
        if let Some(type_name) = receiver_type {
            // The type may be one this module imported, when the method
            // comes from a conformance declared here for another module's
            // type, so its declaration is looked up where it lives.
            if let Some(owner) = self.declaring_module(type_name) {
                if let Some(entry) = owner.structs.get(type_name) {
                    type_generics.extend(entry.decl.generics.iter().cloned());
                } else if let Some(entry) = owner.enums.get(type_name) {
                    type_generics.extend(entry.decl.generics.iter().cloned());
                }
            }
        }
        let mut names = type_generics.clone();
        names.extend(decl.generics.iter().cloned());
        let outer = self.type_params.clone();
        let generics = self.enter_generics(&names);
        let bounds = self.bounds_of(&decl.generics);
        let owner_arity = type_generics.len();

        // ADR 0004: a declaration's parameters are written, not inferred —
        // unlike a lambda's, which shares this same `Param` node but takes
        // its types from the expected type at its call site, a declaration
        // has no call site to infer from. `param_sig` still maps the missing
        // type to `Ty::recovery()` below, so this is one error rather than a
        // cascade through every call the parameter appears in.
        for param in &decl.params {
            if param.ty.is_none() {
                self.diagnostics.push(
                    Diagnostic::error(
                        MISSING_PARAMETER_TYPE,
                        format!("parameter `{}` has no declared type", param.name.node),
                    )
                    .at(param.span)
                    .rule("A declaration's parameters are written: only a lambda's infer, from the expected type at its call site.")
                    .help(format!("write `{}: <type>`", param.name.node)),
                );
            }
        }
        self.check_variadic_shape(&decl.params);

        let params = decl
            .params
            .iter()
            .map(|param| self.param_sig(param))
            .collect::<Vec<_>>();
        let ret = match &decl.return_type {
            Some(ty) => self.resolve(ty),
            None => Ty::Unit,
        };
        let ret_span = match &decl.return_type {
            Some(ty) => ty.span,
            None => decl.name.span,
        };
        let receiver = receiver_type
            .filter(|_| decl.receiver.is_some())
            .map(|name| {
                let args: Vec<Ty> = generics
                    .iter()
                    .take(owner_arity)
                    .map(|p| Ty::Param(p.clone()))
                    .collect();
                self.nominal(name, args)
            });
        self.type_params = outer;
        FnSig {
            generics,
            bounds,
            params,
            ret,
            ret_span,
            is_async: decl.is_async,
            receiver,
            receiver_is_var: decl.receiver.is_some_and(|receiver| receiver.is_var),
        }
    }

    /// A parameter's declared type. Its default value is checked with the
    /// body, in [`Checker::check_body`], where every signature in the module
    /// is known and the other parameters are in scope.
    fn param_sig(&mut self, param: &Param) -> ParamSig {
        let ty = match &param.ty {
            Some(ty) => self.resolve(ty),
            None => Ty::recovery(),
        };
        ParamSig {
            name: param.name.node.clone(),
            ty,
            variadic: param.variadic,
            has_default: param.default.is_some(),
            // A variadic parameter is an immutable `Array<T>` inside the
            // body whatever stands in front of it, which is the
            // `Place::binding(_, false)` `bind_params` builds for one.
            is_var: param.is_var && !param.variadic,
            span: param.span,
        }
    }

    /// Refuses the two shapes a variadic parameter can be written in that
    /// nothing gave a meaning to.
    ///
    /// Where a parameter stands in the list, and whether it was written with
    /// a default, are read off the parameter list this pass already walks to
    /// build a [`ParamSig`] — which is what [ADR 0021] says makes them the
    /// checker's to decide rather than a backend's.
    ///
    /// **Standing anywhere but last.** A variadic parameter is the `Array<T>`
    /// of the arguments no earlier parameter took, so a parameter after it
    /// could only be filled by an argument it had already collected. The two
    /// evaluators disagreed about which: `Interpreter::assign_labels`
    /// gathers the left-over arguments only when the *last* parameter is
    /// variadic, while `bind_params` wraps *any* variadic slot in an
    /// `Array`, so `fn f(items: Int..., tail: String)` bound `items` to an
    /// array of at most one element. Neither reading was chosen, and the
    /// rule that makes the least language is that there is nothing here to
    /// read.
    ///
    /// **Written with a default.** A variadic parameter given no arguments
    /// is the empty `Array<T>`, which is already the whole answer to what
    /// omitting it means. A default would be a second answer to that
    /// question, and it is one nothing can reach: `bind_params` tests
    /// `variadic` before `default` and `continue`s, and
    /// [`Checker::match_arguments`] does the same, so the expression was
    /// checked, could carry side effects a reader expects, and was
    /// unreachable by construction.
    ///
    /// [ADR 0021]: https://github.com/myuon/cove/blob/main/docs/adr/0021-places-are-a-static-fact.md
    fn check_variadic_shape(&mut self, params: &[Param]) {
        for (index, param) in params.iter().enumerate() {
            if !param.variadic {
                continue;
            }
            if index + 1 != params.len() {
                self.diagnostics.push(
                    Diagnostic::error(
                        VARIADIC_POSITION,
                        format!(
                            "parameter `{}` is variadic, so it must be the last one",
                            param.name.node
                        ),
                    )
                    .at(param.span)
                    .rule("A variadic parameter is the last one its declaration writes: it collects every argument the parameters before it did not take.")
                    .help(format!(
                        "move `{}` to the end of the parameter list",
                        param.name.node
                    )),
                );
            }
            if param.default.is_some() {
                self.diagnostics.push(
                    Diagnostic::error(
                        VARIADIC_DEFAULT,
                        format!(
                            "parameter `{}` is variadic, so it cannot have a default",
                            param.name.node
                        ),
                    )
                    .at(param.span)
                    .rule("A variadic parameter given no arguments is an empty `Array<T>`, so there is nothing left for a default to answer.")
                    .help(format!(
                        "remove the `= ...`; a call that passes nothing already gives `{}` an empty array",
                        param.name.node
                    )),
                );
            }
        }
    }

    /// Brings `params` into scope as type parameters, on top of whatever is
    /// already in scope, and returns just the ones it added. Every caller
    /// restores the previous list when the declaration ends.
    fn enter_generics(&mut self, params: &[GenericParam]) -> Vec<Arc<str>> {
        let generics: Vec<Arc<str>> = params.iter().map(|p| p.name.node.as_str().into()).collect();
        self.type_params.extend(generics.iter().cloned());
        generics
    }

    /// The traits each of `params` is bounded by, with every bound checked to
    /// name a trait this module declares.
    fn bounds_of(&mut self, params: &[GenericParam]) -> BTreeMap<Arc<str>, Vec<TraitBound>> {
        let mut bounds: BTreeMap<Arc<str>, Vec<TraitBound>> = BTreeMap::new();
        for param in params {
            let mut named: Vec<TraitBound> = Vec::new();
            for bound in &param.bounds {
                let Some(key) = self.trait_key(&bound.node) else {
                    self.diagnostics
                        .push(unknown_trait(&bound.node, bound.span));
                    continue;
                };
                if named.iter().any(|b| *b.name == *key) {
                    continue;
                }
                named.push(TraitBound {
                    name: key.as_str().into(),
                    span: bound.span,
                });
            }
            if !named.is_empty() {
                bounds.insert(param.name.node.as_str().into(), named);
            }
        }
        bounds
    }

    /// Reports a bound written on a declaration whose type parameters the MVP
    /// never checks a bound against.
    ///
    /// A bound is checked where a type parameter is instantiated, and only a
    /// call site instantiates one today. A `struct`, `enum`, or `type` writes
    /// its arguments in a type, which this pass does not check bounds for, so
    /// a bound written there would be silently ignored.
    fn reject_bounds(&mut self, params: &[GenericParam], what: &str) {
        for param in params {
            for bound in &param.bounds {
                self.diagnostics.push(
                    Diagnostic::error(
                        UNSUPPORTED_BOUND,
                        format!(
                            "a bound on a {what}'s type parameter is not checked in the MVP"
                        ),
                    )
                    .at(bound.span)
                    .rule("A bound is checked where its type parameter is instantiated, and only a call site instantiates one; a `struct`, `enum`, or `type` binds its arguments in a type instead.")
                    .help(format!(
                        "write `{}` here, and bound the type parameter of the functions that operate on it",
                        param.name.node
                    )),
                );
            }
        }
    }

    /// A struct or enum this module declares, or `Unknown` when it declares
    /// neither.
    fn nominal(&self, name: &str, args: Vec<Ty>) -> Ty {
        let Some(owner) = self.declaring_module(name) else {
            return Ty::recovery();
        };
        let key = self.key(name);
        if owner.structs.contains_key(name) {
            Ty::Struct(key.into(), args)
        } else if owner.enums.contains_key(name) {
            Ty::Enum(key.into(), args)
        } else {
            Ty::recovery()
        }
    }

    /// The resolved module a name as written belongs to: this one when it
    /// declares the name, and the module a `use` imported it from otherwise.
    fn declaring_module(&self, name: &str) -> Option<&'a ResolvedModule> {
        match self.module.imports.get(name) {
            Some(owner) if self.module.owner_of(name) != Some(&self.module.name) => {
                self.program.modules.get(owner)
            }
            _ => Some(self.module),
        }
    }

    /// The trait a canonical key names, wherever it is declared.
    fn trait_entry(&self, key: &str) -> Option<&'a TraitEntry> {
        match key.rsplit_once('.') {
            Some((owner, name)) => self.program.modules.get(owner)?.traits.get(name),
            None => self.module.traits.get(key),
        }
    }

    /// The canonical key of the trait `name` refers to here, when this
    /// module declares or imports one.
    fn trait_key(&self, name: &str) -> Option<String> {
        let key = self.key(name);
        self.traits.contains_key(&key).then_some(key)
    }

    // ---------------------------------------------------- written types

    /// Resolves a written type against the builtins, this module's
    /// declarations, and the type parameters in scope.
    fn resolve(&mut self, ty: &Type) -> Ty {
        match &ty.kind {
            TypeKind::Unit => Ty::Unit,
            TypeKind::Fn {
                is_async,
                params,
                return_type,
            } => {
                let params = params
                    .iter()
                    .map(|param| match &param.ty {
                        Some(ty) => self.resolve(ty),
                        // The parser gives every parameter of a written
                        // function type a type, named or bare, so there is
                        // no such thing as a missing one here.
                        None => Ty::placeholder(),
                    })
                    .collect();
                let ret = match return_type {
                    Some(ty) => self.resolve(ty),
                    None => Ty::Unit,
                };
                Ty::func(*is_async, params, ret)
            }
            TypeKind::Named { path, args } => self.resolve_named(path, args, ty.span),
            TypeKind::Dyn(name) => {
                // `dyn` names a trait with a bare name, so an imported trait
                // is reached through a `use` of the trait itself; a module
                // imported whole cannot qualify one.
                let Some(key) = self.trait_key(&name.node) else {
                    self.diagnostics.push(unknown_trait(&name.node, name.span));
                    return Ty::recovery();
                };
                Ty::Dyn(key.as_str().into())
            }
        }
    }

    fn resolve_named(&mut self, path: &[Ident], args: &[Type], span: Span) -> Ty {
        let arguments: Vec<Ty> = args.iter().map(|arg| self.resolve(arg)).collect();
        if path.len() > 1 {
            let head = &path[0].node;
            // A module imported whole makes its exported types writable
            // qualified, exactly as a `use` of the type would make them
            // writable bare.
            if path.len() == 2 && self.module.module_imports.contains_key(head.as_str()) {
                let Some(key) = self.qualified_key(head, &path[1].node, span) else {
                    return Ty::recovery();
                };
                return self.foreign_type(&key, arguments, span);
            }
            if self.module.host_uses.contains(head.as_str()) {
                if path.len() == 2 {
                    return self.host_named_type(head, &path[1].node, arguments.len(), span);
                }
                // A host type is written `<module>.<Name>` and nothing
                // longer, so a deeper path reaches past anything a schema
                // could describe and is left to the boundary.
                self.diagnostics
                    .push(unchecked_host_type(&join_path(path), span));
                return Ty::dynamic_boundary();
            }
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_TYPE,
                    format!("`{}` names no type this module can see", join_path(path)),
                )
                .at(span)
                .rule("A qualified type name reaches a host module, or a module of this package imported with `use`.")
                .help(format!(
                    "add `use {}` if `{}` is a host module or a module of this package, or declare the type in this module",
                    path[0].node,
                    path[0].node
                )),
            );
            return Ty::recovery();
        }

        let name = path[0].node.as_str();
        if let Some(param) = self.type_params.iter().find(|p| &***p == name).cloned() {
            self.check_type_arity(name, 0, arguments.len(), span);
            return Ty::Param(param);
        }
        if let Some(ty) = self.builtin_type(name, &arguments, span) {
            return ty;
        }
        if let Some(entry) = self.module.structs.get(name) {
            let declared = entry.decl.generics.len();
            self.check_type_arity(name, declared, arguments.len(), span);
            return Ty::Struct(name.into(), fit(arguments, declared));
        }
        if let Some(entry) = self.module.enums.get(name) {
            let declared = entry.decl.generics.len();
            self.check_type_arity(name, declared, arguments.len(), span);
            return Ty::Enum(name.into(), fit(arguments, declared));
        }
        if self.module.aliases.contains_key(name) {
            let (generics, ty) = self.alias(name);
            self.check_type_arity(name, generics.len(), arguments.len(), span);
            return expand_alias(generics, ty, arguments);
        }
        if self.is_imported(name) {
            let key = self.key(name);
            return self.foreign_type(&key, arguments, span);
        }
        self.diagnostics.push(
            Diagnostic::error(
                UNKNOWN_TYPE,
                format!("`{name}` names no type this module can see"),
            )
            .at(span)
            .rule("A module sees its own declarations, what it imports with `use`, and the builtins.")
            .help(format!(
                "declare `struct {name}`, `enum {name}`, or `type {name} = ...` in this module, or `use <module>.{name}` to import it; a type only a host knows is written `<module>.{name}` after a `use` of that module"
            )),
        );
        Ty::recovery()
    }

    /// A type another module declares, named by its canonical key.
    ///
    /// The key is enough: the module's whole environment was brought in
    /// before this one was prepared, so the declaration's own signature is
    /// already resolved.
    fn foreign_type(&mut self, key: &str, arguments: Vec<Ty>, span: Span) -> Ty {
        let written = key.rsplit('.').next().unwrap_or(key).to_string();
        if let Some(sig) = self.structs.get(key) {
            let declared = sig.generics.len();
            self.check_type_arity(&written, declared, arguments.len(), span);
            return Ty::Struct(key.into(), fit(arguments, declared));
        }
        if let Some(sig) = self.enums.get(key) {
            let declared = sig.generics.len();
            self.check_type_arity(&written, declared, arguments.len(), span);
            return Ty::Enum(key.into(), fit(arguments, declared));
        }
        if let Some((generics, ty)) = self.aliases.get(key).cloned() {
            self.check_type_arity(&written, generics.len(), arguments.len(), span);
            return expand_alias(generics, ty, arguments);
        }
        // The name resolves to a declaration that is not a type, such as an
        // imported function. Writing one where a type belongs is a mistake
        // with a name, not a gap in what the checker knows.
        self.diagnostics.push(
            Diagnostic::error(UNKNOWN_TYPE, format!("`{written}` is not a type"))
                .at(span)
                .rule("A type is a struct, an enum, a type alias, a type parameter, or a builtin.")
                .help(format!(
                    "`{written}` names something else the module exports; name a type instead"
                )),
        );
        Ty::recovery()
    }

    /// The builtin named `name`, with its arity checked.
    ///
    /// How many type arguments each builtin takes is the number of
    /// parameters `cove_schema::builtins` declares it binds, so `Map<K, V>`
    /// takes two here because it takes two there. What `Ty` each name is
    /// stays this crate's, since `Ty` is this crate's representation.
    fn builtin_type(&mut self, name: &str, args: &[Ty], span: Span) -> Option<Ty> {
        // `Scope` is the one builtin whose type a program never writes: a
        // task scope is reached through `scope name { ... }`, so naming it is
        // an undeclared name rather than a builtin with the wrong arity.
        if name == SCOPE.name {
            return None;
        }
        let arity = cove_schema::builtin(name)?.parameters.len();
        self.check_type_arity(name, arity, args.len(), span);
        let first = args.first().cloned().unwrap_or(Ty::recovery());
        let second = args.get(1).cloned().unwrap_or(Ty::recovery());
        Some(match name {
            "Unit" => Ty::Unit,
            "Bool" => Ty::Bool,
            "Int" => Ty::Int,
            "Float" => Ty::Float,
            "String" => Ty::Str,
            "Duration" => Ty::Duration,
            "Error" => Ty::Error,
            "Range" => Ty::Range,
            "Array" => Ty::Array(Box::new(first)),
            "Vector" => Ty::Vector(Box::new(first)),
            "Set" => Ty::Set(Box::new(first)),
            "Option" => Ty::Option(Box::new(first)),
            "Task" => Ty::Task(Box::new(first)),
            "Shared" => Ty::Shared(Box::new(self.task_safe_argument(first, span))),
            "Map" => Ty::Map(Box::new(first), Box::new(second)),
            "MapEntry" => Ty::MapEntry(Box::new(first), Box::new(second)),
            _ => Ty::Result(Box::new(first), Box::new(second)),
        })
    }

    fn check_type_arity(&mut self, name: &str, expected: usize, found: usize, span: Span) {
        if expected == found {
            return;
        }
        self.diagnostics.push(
            Diagnostic::error(
                TYPE_ARGUMENTS,
                format!("`{name}` takes {expected} type argument(s), but {found} were written"),
            )
            .at(span)
            .rule("A generic type is written with exactly the arguments its declaration binds.")
            .help(if expected == 0 {
                format!("write `{name}`")
            } else {
                format!(
                    "write `{name}<{}>`",
                    (0..expected).map(|_| "_").collect::<Vec<_>>().join(", ")
                )
            }),
        );
    }

    /// Checks the argument of a `Shared<T>` and returns it, reporting the
    /// first part of it that may not cross a task boundary.
    ///
    /// A `Shared` is reachable from every task it was given to, so what it
    /// wraps must be able to cross a boundary itself. The Language Card names
    /// `Shared` in the sentence that keeps a vector out of a task, and a
    /// `Shared<Vector<T>>` would be exactly the reach that sentence forbids.
    ///
    /// This is the static half of the rule, and a type is all it sees, so it
    /// answers for the type arguments a program writes. A struct whose
    /// *field* holds a vector is refused too, by the walk over the value
    /// itself in `cove_runtime::task`, which is where the whole rule lives.
    fn task_safe_argument(&mut self, ty: Ty, span: Span) -> Ty {
        if let Some(offending) = not_task_safe(&ty) {
            let offending = offending.to_string();
            let message = if offending == ty.to_string() {
                format!("`Shared` cannot wrap a `{offending}`, which cannot cross a task boundary")
            } else {
                format!(
                    "`Shared` cannot wrap `{ty}`: the `{offending}` in it cannot cross a task boundary"
                )
            };
            self.diagnostics.push(
                Diagnostic::error(TASK_SAFETY, message)
                .at(span)
                .rule(TASK_SAFETY_RULE)
                .help(if offending.starts_with("Vector") {
                    "wrap an `Array` instead, or finish the vector with `freeze()` before wrapping it"
                        .to_string()
                } else {
                    format!("wrap a value that may cross a task boundary; a `{offending}` may not")
                }),
            );
        }
        ty
    }

    /// Expands a type alias, once per module.
    fn alias(&mut self, name: &str) -> (Vec<Arc<str>>, Ty) {
        if let Some(cached) = self.aliases.get(name) {
            return cached.clone();
        }
        // Only a name the module declares as an alias is expanded, which
        // every caller has already established.
        let Some(entry) = self.module.aliases.get(name) else {
            return (Vec::new(), Ty::placeholder());
        };
        let decl = entry.decl.clone();
        if self.expanding.iter().any(|n| n == name) {
            self.diagnostics.push(
                Diagnostic::error(ALIAS_CYCLE, format!("`{name}` expands to itself"))
                    .at(decl.name.span)
                    .rule("A type alias names an existing type; it cannot be defined in terms of itself.")
                    .help(format!(
                        "declare `struct {name}` or `enum {name}` instead, which may refer to itself through a field"
                    )),
            );
            return (Vec::new(), Ty::recovery());
        }
        self.expanding.push(name.to_string());
        let outer = std::mem::take(&mut self.type_params);
        self.reject_bounds(&decl.generics, "type alias");
        let generics = self.enter_generics(&decl.generics);
        let ty = self.resolve(&decl.ty);
        self.type_params = outer;
        self.expanding.pop();
        let resolved = (generics, ty);
        // Expanding an alias can report — a bound written on its parameters
        // is refused here — and the cache would then answer the second walk
        // without reporting again. A probe's diagnostics are discarded, so
        // caching from inside one would discard the diagnostic for good.
        if !self.probing {
            self.aliases.insert(name.to_string(), resolved.clone());
        }
        resolved
    }

    // ------------------------------------------------------------ scopes

    /// Brings `name` into scope, saying whether source may write the place
    /// it binds.
    ///
    /// Mutability is a parameter rather than a default because a default is
    /// how the two halves of this rule came apart in the first place: every
    /// site that binds a name knows which kind it is binding, and a site
    /// that had to be *remembered* to mark is a site that will one day be
    /// forgotten.
    fn declare(&mut self, name: &str, ty: Ty, mutable: bool) {
        // The other half of the placeholder invariant `Checker::expr`
        // asserts: a binding's type is read by every later use of the name,
        // so a placeholder reaching one is a site that was wrong about
        // itself.
        debug_assert!(
            !ty.holds_placeholder(),
            "a placeholder unknown escaped into the type of `{name}`: `{ty}`"
        );
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), Binding { ty, mutable });
        }
    }

    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    /// Whether `name` reaches a binding source may write.
    ///
    /// A name is writable when the binding it reaches is a `var` *and* that
    /// binding belongs to the function value being checked. A capture is
    /// read-only whatever it was declared as — see [`Checker::capture_floor`]
    /// — so the depth the name was found at is part of the answer and not
    /// bookkeeping.
    fn writable(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(depth, scope)| scope.get(name).map(|binding| (depth, binding)))
            .is_some_and(|(depth, binding)| binding.mutable && depth >= self.capture_floor)
    }

    /// Whether `expr` is a place, and whether source may write it.
    ///
    /// **This is the definition.** A place is a name a body bound, or a
    /// field of a place — nothing else, and no deeper analysis. The two
    /// readings of it that used to exist are readings of this one:
    /// `Interpreter::resolve_place_opt` asks it of an `Env` at run time, and
    /// `cove_ir::lower`'s `Body::place_mutability` asked it of a slot table
    /// while lowering. Both answer what a scope stack already knows, which
    /// is why the question belongs here.
    ///
    /// `None` is not a place at all: a call's result, a literal, an operator
    /// applied to anything, or a name this body did not bind. `Some` is a
    /// place, `true` where source may write it and `false` where `let` made
    /// it read-only. A field does not ask a question of its own — it
    /// inherits its root's answer, exactly as `Place::field` copies
    /// `mutable` down from the base.
    ///
    /// A name this body did not bind answers `None` rather than a refusal:
    /// it is a module's declaration, or a name resolution already reported,
    /// and neither is this rule's to speak about. [`Checker::not_a_place`]
    /// is what decides that abstention, and says why it is safe.
    fn place_mutability(&self, expr: &Expr) -> Option<bool> {
        match &expr.kind {
            ExprKind::Ident(name) => self.lookup(name).map(|_| self.writable(name)),
            ExprKind::Field { base, .. } => self.place_mutability(base),
            _ => None,
        }
    }

    /// The `var` arguments of a call, checked against the place rule.
    ///
    /// `Interpreter::eval_args` resolves a `var` argument to a place before
    /// it knows which declaration the call reaches, and refuses one that is
    /// not a place or is a read-only one; this is the same question asked in
    /// the same position, so it covers a `var` written at a call to anything
    /// at all.
    ///
    /// What is *not* asked here is whether the callee declared that
    /// parameter `var`. That is a fact about the two markings agreeing
    /// rather than about places, a function type carries no marking for a
    /// call through a value to be checked against, and the interpreter still
    /// answers it — see the module docs under "What the runtime keeps".
    fn var_arguments(&mut self, args: &[Arg]) {
        for arg in args.iter().filter(|arg| arg.is_var) {
            match self.place_mutability(&arg.value) {
                Some(true) => {}
                Some(false) => {
                    let place = place_text(&arg.value);
                    self.diagnostics.push(
                        Diagnostic::error(
                            READ_ONLY_PLACE,
                            format!(
                                "`{place}` is a read-only place, so it cannot be passed as `var`"
                            ),
                        )
                        .at(arg.span)
                        .rule("`let` creates a read-only place; `var` creates a mutable place.")
                        .help(format!("declare it with `var {place}`")),
                    );
                }
                None if Checker::not_a_place(&arg.value) => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            NOT_A_PLACE,
                            "this expression is not a place, so it cannot be assigned or aliased",
                        )
                        .at(arg.value.span)
                        .rule("Only variables and their struct fields are places.")
                        .help("bind it with `var` first, then pass that binding"),
                    );
                }
                None => {}
            }
        }
    }

    /// A method that writes through its receiver, checked against the place
    /// rule.
    ///
    /// The receiver of a `var self` method is the caller's own place, so it
    /// must be one and must be writable — `Interpreter::eval_method_call`
    /// asks both before it makes the alias. `freeze` is the one mutating
    /// builtin that tolerates a receiver which is no place at all: a
    /// temporary holds the only handle to its own storage, so freezing it
    /// answers from the temporary rather than writing anywhere, which is
    /// what `builtins::call_method`'s own arm does.
    fn mutating_receiver(&mut self, receiver: &Ty, method: &Ident, base: &Expr, span: Span) {
        let Some(needs_a_place) = self.mutating_method(receiver, &method.node) else {
            return;
        };
        match self.place_mutability(base) {
            Some(true) => {}
            Some(false) => {
                let place = place_text(base);
                self.diagnostics.push(
                    Diagnostic::error(
                        READ_ONLY_PLACE,
                        format!(
                            "`{}` takes a `var self` receiver, but `{place}` is a read-only place",
                            method.node
                        ),
                    )
                    .at(span)
                    .rule("`let` creates a read-only place; `var` creates a mutable place.")
                    .help(format!("declare it with `var {place}`")),
                );
            }
            None if needs_a_place && Checker::not_a_place(base) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        NOT_A_PLACE,
                        format!(
                            "`{}` takes a `var self` receiver, but `{}` is not a place",
                            method.node,
                            place_text(base)
                        ),
                    )
                    .at(span)
                    .rule("A mutating receiver declares `var self` and mutates the caller's place.")
                    .help("bind the value with `var` first, then call the method on that binding"),
                );
            }
            None => {}
        }
    }

    /// Whether `method` called on a receiver of type `ty` writes through it,
    /// and whether it needs a place even where the receiver is a temporary.
    ///
    /// `None` is *not mutating, or this pass will not say*. A receiver whose
    /// type is an unknown, a `Never`, or a host type is the second: the
    /// interpreter reaches a host resource's own operations before it
    /// reaches any of this, and an unknown receiver could be one.
    fn mutating_method(&self, ty: &Ty, method: &str) -> Option<bool> {
        match ty {
            Ty::Unknown(_) | Ty::Never | Ty::Host(_) => None,
            Ty::Struct(name, _) | Ty::Enum(name, _) => self
                .methods
                .get(&(name.to_string(), method.to_string()))
                .and_then(|sig| sig.receiver_is_var.then_some(true)),
            // A method reached through a bound or through `dyn` dispatches
            // on the concrete value's own type at run time, so the receiver
            // is the caller's place there exactly as it is for a direct
            // call.
            Ty::Param(param) => self
                .bound_method(param, method)
                .and_then(|(trait_name, _)| {
                    self.mutating_trait_method(&trait_name, method)
                        .then_some(true)
                }),
            Ty::Dyn(trait_name) => self
                .mutating_trait_method(trait_name, method)
                .then_some(true),
            // A builtin type: the shared table says which of its methods
            // write through their receiver, and `freeze` is the one that
            // does not need a place to write to — it takes the storage
            // rather than changing it, so a temporary holding the only
            // handle can be frozen. The rest need somewhere for the change
            // to land.
            //
            // The receiver's own entry is asked and not the name alone,
            // because a mutating name belongs to a type: `pop` is a
            // `Vector`'s and no method of an `Array` at all, so
            // `items.pop()` on an array is told it has no such method rather
            // than told to find a place for a receiver it would never need.
            // `Interpreter::call_builtin_method`'s guard asks the same
            // question of the value it is holding.
            _ => cove_schema::builtins::builtin(&builtin_name(ty))
                .and_then(|entry| entry.method(method))
                .filter(|declared| declared.mutating)
                .map(|_| method != "freeze"),
        }
    }

    /// Whether an expression that is not a place should be reported as one.
    ///
    /// An `Ident`, or a field path rooted at one, that this body did not
    /// bind is left alone. It names a declaration the module holds — which
    /// the interpreter refuses with `cannot find` from its own environment,
    /// a resolution answer rather than a place one — or a name that failed
    /// to resolve and has been reported already. Either way a second
    /// diagnostic here would say less than the first.
    ///
    /// Everything else — a call's result, a literal, an operator — is
    /// decidedly not a place, from the shape of the expression alone.
    fn not_a_place(expr: &Expr) -> bool {
        !matches!(expr.kind, ExprKind::Ident(_) | ExprKind::Field { .. })
    }

    // ------------------------------------------------------- expressions

    /// Checks a block and returns the type of its value, which is its tail
    /// expression's type or `Unit` when it has none.
    fn block(&mut self, block: &Block, expected: Option<&Expected>) -> Ty {
        self.scopes.push(BTreeMap::new());
        for stmt in &block.statements {
            self.stmt(stmt);
        }
        let ty = match &block.tail {
            Some(tail) => self.expr(tail, expected),
            None => {
                let ty = Ty::Unit;
                if let Some(expected) = expected {
                    self.expect(&ty, expected, block.span);
                }
                ty
            }
        };
        self.scopes.pop();
        ty
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let {
                is_var,
                name,
                ty,
                value,
            } => {
                let bound = match ty {
                    Some(written) => {
                        let declared = self.resolve(written);
                        let expected = Expected::new(
                            declared.clone(),
                            written.span,
                            format!("the declared type is `{declared}`"),
                        );
                        self.expr(value, Some(&expected));
                        declared
                    }
                    None => {
                        let inferred = self.expr(value, None);
                        // A binding whose initializer never produces a value
                        // has no type to infer; stop rather than guess. Every
                        // use of the name is in code the initializer's
                        // `return` or `break` made unreachable, so there is
                        // nothing left to say about any of them.
                        if inferred == Ty::Never {
                            Ty::recovery()
                        } else {
                            inferred
                        }
                    }
                };
                self.declare(&name.node, bound, *is_var);
            }
            StmtKind::Expr(expr) => {
                self.expr(expr, None);
            }
            StmtKind::Item(item) => {
                // A local `fn` is an ordinary closure the body can call.
                if let ItemKind::Fn(decl) = &item.kind {
                    let outer_params = self.type_params.clone();
                    let sig = self.fn_sig(decl, None);
                    self.declare(&decl.name.node, sig.as_value(), false);
                    let outer_ret = std::mem::replace(&mut self.ret, sig.ret.clone());
                    let outer_span = std::mem::replace(&mut self.ret_span, sig.ret_span);
                    let outer_stated = std::mem::replace(&mut self.ret_stated, true);
                    self.type_params.extend(sig.generics.iter().cloned());
                    let outer_bounds = self.bounds.clone();
                    self.bounds.extend(
                        sig.bounds
                            .iter()
                            .map(|(name, bounds)| (name.clone(), bounds.clone())),
                    );
                    // A local `fn` is built as a closure, so the names
                    // around it are captures and a capture is read-only.
                    let outer_floor = std::mem::replace(&mut self.capture_floor, self.scopes.len());
                    self.scopes.push(BTreeMap::new());
                    for param in &sig.params {
                        self.declare(&param.name, param.ty.clone(), param.is_var);
                    }
                    let expected = Expected::new(
                        sig.ret.clone(),
                        sig.ret_span,
                        format!("the declared return type is `{}`", sig.ret),
                    );
                    self.block(&decl.body, Some(&expected));
                    self.scopes.pop();
                    self.capture_floor = outer_floor;
                    self.bounds = outer_bounds;
                    self.type_params = outer_params;
                    self.ret_span = outer_span;
                    self.ret = outer_ret;
                    self.ret_stated = outer_stated;
                }
            }
        }
    }

    /// Checks an expression, against `expected` when the surrounding form
    /// imposes one, and returns its type.
    ///
    /// An expression's type is the most observable thing this pass produces:
    /// it is what the next form is checked against and what a binding holding
    /// the value keeps. [`Unknown::Placeholder`] claims to reach neither, so
    /// one arriving here means a construction site was wrong about itself,
    /// and the assertion names it in the test suite rather than letting the
    /// unknown validate whatever comes next.
    /// Recording happens here, at the one point every expression passes
    /// through, so no form can be added later that forgets to. It happens
    /// after the type is settled and nothing in the walk reads it back, so
    /// what is recorded cannot change what is reported.
    ///
    /// An expression walked twice records twice, and the later record wins.
    /// A [`Checker::probe`] is always the earlier of the two, so the answer
    /// left behind is the one the real walk reached.
    fn expr(&mut self, expr: &Expr, expected: Option<&Expected>) -> Ty {
        let ty = self.expr_type(expr, expected);
        debug_assert!(
            !ty.holds_placeholder(),
            "a placeholder unknown escaped into the type of an expression at {:?}: `{ty}`",
            expr.span
        );
        self.facts.record_ty(expr.span.file, expr.id, &ty);
        ty
    }

    fn expr_type(&mut self, expr: &Expr, expected: Option<&Expected>) -> Ty {
        let span = expr.span;
        let ty = match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Duration(_) => Ty::Duration,
            ExprKind::Unit => Ty::Unit,
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Interpolation(inner) = part {
                        // Interpolation renders any value, so an interpolated
                        // expression is checked but not constrained.
                        self.expr(inner, None);
                    }
                }
                Ty::Str
            }
            ExprKind::Ident(name) => self.ident(name, span, expected),
            ExprKind::ArrayLit(items) => self.array_literal(items, span, expected),
            ExprKind::Field { base, name } => self.field(base, name, span),
            ExprKind::Call {
                callee,
                generics,
                args,
                trailing,
            } => self.call(
                expr.id,
                callee,
                generics,
                args,
                trailing.as_deref(),
                span,
                expected,
            ),
            ExprKind::Unary { op, operand } => self.unary(*op, operand, span),
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, span),
            ExprKind::Assign { op, target, value } => self.assign(*op, target, value, span),
            ExprKind::Try(inner) => self.try_expr(inner, span),
            ExprKind::Await(inner) => self.await_expr(inner, span),
            ExprKind::Block(block) => return self.block(block, expected),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                return self.if_expr(
                    condition,
                    then_branch,
                    else_branch.as_deref(),
                    span,
                    expected,
                )
            }
            ExprKind::Match { scrutinee, arms } => {
                return self.match_expr(scrutinee, arms, span, expected)
            }
            ExprKind::For {
                binding,
                iterable,
                body,
            } => self.for_expr(binding, iterable, body),
            ExprKind::While { condition, body } => {
                self.condition(condition);
                self.block(body, None);
                Ty::Unit
            }
            ExprKind::Return(value) => {
                if self.ret_stated {
                    let expected = Expected::new(
                        self.ret.clone(),
                        self.ret_span,
                        format!("the declared return type is `{}`", self.ret),
                    );
                    match value {
                        Some(value) => {
                            self.expr(value, Some(&expected));
                        }
                        None => self.expect(&Ty::Unit, &expected, span),
                    }
                } else {
                    // A function value nothing expects takes its result from
                    // its body's value. An early `return` produces one
                    // somewhere the body's value is not, so nothing written
                    // anywhere says what the two have to agree on — and the
                    // function's own type would be read off a body that no
                    // longer decides it.
                    self.diagnostics.push(
                        Diagnostic::error(
                            LAMBDA_RETURN,
                            "this function value uses `return`, but nothing says what it produces",
                        )
                        .at(span)
                        .rule("A `return` is checked against a stated result type: a declaration writes one, and a function value takes one from the place that holds it.")
                        .help("give this function value to a place that declares its type, as in `let handle: fn(Int) -> String = fn(n) { ... }`, or end the body with the value instead of returning it"),
                    );
                    if let Some(value) = value {
                        self.expr(value, None);
                    }
                }
                Ty::Never
            }
            // A loop's value comes from `break`, so a `break` operand is
            // checked against the loop's expected type rather than the
            // function's. Neither form produces a value of its own.
            ExprKind::Break(value) => {
                // The operand is checked on its own, against no expectation,
                // and its value is discarded: the loop it leaves produces
                // `()` however it leaves. See issue #87.
                if let Some(value) = value {
                    self.expr(value, None);
                }
                Ty::Never
            }
            ExprKind::Continue => Ty::Never,
            ExprKind::Lambda {
                is_async,
                params,
                body,
            } => return self.lambda(*is_async, params, body, span, expected),
            ExprKind::Scope { name, body } => {
                self.scopes.push(BTreeMap::new());
                self.declare(&name.node, Ty::Scope, false);
                let ty = self.block(body, expected);
                self.scopes.pop();
                return ty;
            }
            ExprKind::Range {
                start,
                end,
                inclusive_end: _,
            } => {
                let bound = Expected::new(Ty::Int, span, "a range runs between two `Int`s");
                self.expr(start, Some(&bound));
                self.expr(end, Some(&bound));
                Ty::Range
            }
        };
        if let Some(expected) = expected {
            self.expect(&ty, expected, span);
        }
        ty
    }

    /// Whether the place a value is being given to has already been
    /// accounted for, so a form that finds nothing there should stay quiet.
    ///
    /// The `UNCONSTRAINED` warnings all say the same thing — nothing written
    /// settles this type — and that is only worth saying when the silence is
    /// the program's. An expectation the checker itself could not state
    /// carries its own explanation already: an argument of a call whose
    /// callee was just rejected, a block whose type a schema declared `Any`,
    /// a call into a host module no schema describes. Repeating the
    /// gap underneath one of those turns one fact into two diagnostics, which
    /// is what the recovery classification exists to prevent.
    ///
    /// See `Unknown::is_accounted_for` for which unknowns qualify.
    fn accounted_for(expected: Option<&Expected>) -> bool {
        expected.is_some_and(|e| e.ty.is_accounted_for())
    }

    /// The unknown standing for a place this pass abstained about, or a
    /// recovery unknown when the place has a type but not a usable one.
    ///
    /// Both are silent; keeping the kind is what lets a later reader of the
    /// type say which silence it came from.
    fn abstention_of(expected: Option<&Expected>) -> Ty {
        match expected.map(|e| &e.ty) {
            Some(Ty::Unknown(kind)) if kind.is_accounted_for() => Ty::Unknown(*kind),
            _ => Ty::recovery(),
        }
    }

    /// Reports a type that does not match what the surrounding form asked
    /// for, pointing at the expression and labelling what imposed it.
    fn expect(&mut self, found: &Ty, expected: &Expected, span: Span) {
        if found.matches(&expected.ty) || coerces(found, &expected.ty, &self.view()) {
            return;
        }
        // The one implicit conversion the language has is to `dyn Trait`, so
        // a value rejected there is rejected for a reason of its own: it does
        // not conform.
        let mut diagnostic = match &expected.ty {
            Ty::Dyn(trait_name) if !matches!(found, Ty::Dyn(_)) => Diagnostic::error(
                MISMATCH,
                format!("`{found}` does not conform to `{trait_name}`, so it is not a `{}`", expected.ty),
            )
            .at(span)
            .rule("A concrete value becomes a `dyn Trait` value where one is expected, and that is the only implicit conversion in the language; it requires an explicit conformance.")
            .help(format!("write `impl {trait_name} for {found} {{ ... }}`")),
            _ => {
                let mut diagnostic = Diagnostic::error(
                    MISMATCH,
                    format!("expected `{}`, found `{found}`", expected.ty),
                )
                .at(span)
                .rule("Types are nominal and the only implicit conversion is to `dyn Trait`: a value must otherwise already have the type its place asks for.");
                if let Some(help) = conversion_help(&expected.ty, found) {
                    diagnostic = diagnostic.help(help);
                }
                diagnostic
            }
        };
        if let Some(origin) = &expected.origin {
            diagnostic = diagnostic.label(origin.span, origin.label.clone());
        }
        self.diagnostics.push(diagnostic);
    }

    /// A bare name: a local, a module function, a constructor, a host item,
    /// or a name only the host can explain.
    fn ident(&mut self, name: &str, span: Span, expected: Option<&Expected>) -> Ty {
        if let Some(binding) = self.lookup(name) {
            return binding.ty.clone();
        }
        if name == NONE_CASE.name {
            return match expected.map(|e| &e.ty) {
                Some(Ty::Option(inner)) => Ty::Option(inner.clone()),
                // `None` carries nothing, so it is the only value whose own
                // type its own text cannot settle.
                _ => {
                    if !Checker::accounted_for(expected) {
                        self.diagnostics.push(unconstrained(
                            "nothing says what this `None` is an `Option` of".to_string(),
                            format!("write the type on the place that holds it, as in `let value: Option<Int> = {name}`"),
                            span,
                        ));
                    }
                    Ty::Option(Box::new(Ty::unconstrained()))
                }
            };
        }
        if let Some(sig) = self.functions.get(&self.key(name)) {
            return sig.as_value();
        }
        // A host operation is a value. The interpreter has always bound one
        // and called it later — `Value::HostFn` is exactly that — and the
        // schema says what it takes and what it answers, so this is a place
        // where reading the schema turns something that used to be unknown
        // into an ordinary function type rather than into a refusal.
        if let Some(module) = self.module.host_items.get(name).cloned() {
            return self.host_operation_value(&module, name, span);
        }
        // A type or a module used as a value has no type in this system; the
        // forms that give it meaning (`Vector.of`, `MapEntry(key:, value:)`,
        // `Booking(id: 1)`, `lib.create(...)`) are understood at the call
        // itself. Writing one bare is a mistake with a name, so it is named
        // rather than turned into an unknown that would let whatever was
        // done with it check.
        if let Some(what) = self.namespace(name) {
            self.diagnostics.push(not_a_value(name, what, span));
            return Ty::recovery();
        }
        self.unresolved_name(name, span)
    }

    /// `console.println` written where a value belongs, or a bare `println`
    /// that `use console.println` brought into scope.
    ///
    /// A host module's members are its operations and its types. An operation
    /// is a value — the interpreter binds one as `Value::HostFn` and calls it
    /// later — so it is given the function type its schema declares, which
    /// checks a call made through the value exactly as a direct call is
    /// checked. A type is not a value, for the same reason a bare `Vector` is
    /// not.
    fn host_operation_value(&mut self, module: &str, name: &str, span: Span) -> Ty {
        let shown = format!("{module}.{name}");
        let Some(schema) = self.host_schema(module) else {
            // No schema to read: what the host exposes under this name is
            // between it and the boundary, and the `use` that named the
            // module is where that is answered (#74).
            return Ty::dynamic_boundary();
        };
        let Some(operation) = schema.operation(name) else {
            if schema.declared_type(name).is_some() || schema.resource(name).is_some() {
                self.diagnostics
                    .push(not_a_value(&shown, Namespace::HostType, span));
                return Ty::recovery();
            }
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_HOST_OPERATION,
                    format!("host module `{module}` has no operation `{name}`"),
                )
                .at(span)
                .rule(HOST_SCHEMA_RULE)
                .help(format!(
                    "`{module}` exposes {}",
                    list(&operation_names(schema.operations))
                )),
            );
            return Ty::recovery();
        };
        if operation.variadic {
            // A variadic operation has no function type in this language, so
            // the value cannot be given one. Cove has no variadic `fn` type
            // to write, which makes this the language's own gap rather than
            // the program's: the call through the value still runs, and the
            // boundary still counts the arguments. Said out loud as a note,
            // for the same reason a schema's `Any` is one.
            self.diagnostics.push(
                Diagnostic::note(
                    VARIADIC_AS_VALUE,
                    format!(
                        "`{shown}` is variadic, so this value has no function type here"
                    ),
                )
                .at(span)
                .rule("A function type in Cove names a fixed list of parameters; a Host API operation may declare a variadic one, which no `fn` type can be written for.")
                .help(format!(
                    "calling `{shown}` directly is checked against its schema; a call made through this value is checked by the boundary and by nothing here, so write `fn(value: {}) {{ {shown}(value) }}` to have one that is",
                    operation
                        .params
                        .first()
                        .map(host_ty)
                        .unwrap_or(Ty::Unit)
                )),
            );
            return Ty::unconstrained();
        }
        Ty::func(
            false,
            operation.params.iter().map(host_ty).collect(),
            host_ty(&operation.result),
        )
    }

    /// The type of a name nothing in scope explains.
    ///
    /// Both cases are errors, and the case of the first letter only changes
    /// the correction. A capitalized name used to be assumed to come from
    /// the host and warned about instead; but a host reaches this module
    /// through `use` like everything else, so that assumption never named a
    /// real way for the name to arrive — it only let an unknown through to
    /// validate whatever the program then did with it.
    fn unresolved_name(&mut self, name: &str, span: Span) -> Ty {
        let (code, help) = if starts_uppercase(name) {
            (
                UNRESOLVED_NAME,
                format!(
                    "declare `struct {name}` or `enum {name}` in this module, `use <module>.{name}` to import it, or `use <host>` and write `<host>.{name}`"
                ),
            )
        } else {
            (
                UNKNOWN_NAME,
                format!(
                    "declare `let {name} = ...` before this expression, or `use <host>.{name}`"
                ),
            )
        };
        self.diagnostics.push(
            Diagnostic::error(code, format!("cannot find `{name}` in this scope"))
                .at(span)
                .rule("A name must be a local binding, a parameter, a declaration of this module, or something `use` imports.")
                .help(help),
        );
        Ty::recovery()
    }

    /// What `name` names, when it names something values are reached
    /// *through* rather than something that is one.
    ///
    /// The order follows `Checker::ident`'s: a local binding and a
    /// declared function are values and have already answered by the time
    /// this is asked.
    fn namespace(&self, name: &str) -> Option<Namespace> {
        if self.module.structs.contains_key(name) {
            Some(Namespace::Struct)
        } else if self.module.enums.contains_key(name) {
            Some(Namespace::Enum)
        } else if self.is_imported(name) {
            Some(self.declared_shape(&self.key(name)))
        } else if cove_schema::is_builtin_type(name) || name == MAP_ENTRY.name {
            Some(Namespace::BuiltinType)
        } else if self.module.host_uses.contains(name) {
            Some(Namespace::HostModule)
        } else if self.module.module_imports.contains_key(name) {
            Some(Namespace::Module)
        } else {
            None
        }
    }

    /// Whether the declaration `key` names is a struct, an enum, or something
    /// whose shape decides nothing about how a value of it is written.
    ///
    /// A trait and a type alias reach the last of these: neither is
    /// constructed and neither has cases, so the correction can only point at
    /// the associated functions.
    fn declared_shape(&self, key: &str) -> Namespace {
        if self.structs.contains_key(key) {
            Namespace::Struct
        } else if self.enums.contains_key(key) {
            Namespace::Enum
        } else {
            Namespace::Type
        }
    }

    fn array_literal(&mut self, items: &[Expr], span: Span, expected: Option<&Expected>) -> Ty {
        let mut element_hint = match expected.map(|e| &e.ty) {
            Some(Ty::Array(inner)) => Some((**inner).clone()),
            _ => None,
        };
        // With no expected element type a sibling may still settle one, and
        // which sibling is not known until every one has been walked:
        // `[[], [1]]` is an `Array<Array<Int>>` and nothing in it was left
        // unproved. So the items are walked once with nothing reported, to
        // find the element type out, and then again for real against it.
        if element_hint.is_none() && items.len() > 1 && !self.probing {
            let found = self.probe(|checker| {
                items.iter().fold(Ty::recovery(), |element, item| {
                    let ty = checker.expr(item, None);
                    element.join(&ty)
                })
            });
            if !found.is_wild() {
                element_hint = Some(found);
            }
        }
        if items.is_empty() && element_hint.is_none() && !Checker::accounted_for(expected) {
            // An empty literal has no element to read a type off and no
            // expected type to be given one, so `Array<_>` is as far as the
            // checker gets and every element-typed operation on it after
            // this point is unchecked.
            self.diagnostics.push(unconstrained(
                "nothing says what this empty array holds".to_string(),
                "write the type on the place that holds it, as in `let items: Array<Int> = []`"
                    .to_string(),
                span,
            ));
        }
        let mut element = element_hint
            .clone()
            .unwrap_or_else(|| match items.is_empty() {
                true => Ty::unconstrained(),
                false => Ty::recovery(),
            });
        for item in items {
            let hint = element_hint
                .clone()
                .map(|ty| {
                    let label = format!("the array's element type is `{ty}`");
                    Expected::new(ty, span, label)
                })
                .or_else(|| {
                    (!element.is_wild()).then(|| {
                        Expected::new(
                            element.clone(),
                            span,
                            format!("the first element is `{element}`"),
                        )
                    })
                });
            let ty = self.expr(item, hint.as_ref());
            element = element.join(&ty);
        }
        Ty::Array(Box::new(element))
    }

    /// `base.name`: an enum case, a host operation, an imported module's
    /// declaration, or a struct field.
    fn field(&mut self, base: &Expr, name: &Ident, span: Span) -> Ty {
        // `http.Method.Get` is three segments, so it reaches here as a field
        // of a field. A host module's enum has no other way to be written.
        if let ExprKind::Field {
            base: module,
            name: declared,
        } = &base.kind
        {
            if let ExprKind::Ident(head) = &module.kind {
                if self.lookup(head).is_none() && self.module.host_uses.contains(head.as_str()) {
                    if let Some(ty) = self.host_enum_case(head, &declared.node, name, span) {
                        return ty;
                    }
                }
            }
        }
        if let ExprKind::Ident(head) = &base.kind {
            if self.lookup(head).is_none() {
                let key = self.key(head);
                if self.enums.contains_key(&key) {
                    return self.enum_case(&key, name, &[], span);
                }
                if self.module.host_uses.contains(head.as_str()) {
                    // A host module's members are its operations and its
                    // types. An operation is a value and the schema says
                    // which one; a type is not, exactly as a bare `Vector`
                    // is not. A build with no schema for the module can tell
                    // neither, and leaves both to the boundary.
                    return self.host_operation_value(head, &name.node, span);
                }
                if self.module.module_imports.contains_key(head.as_str()) {
                    let Some(key) = self.qualified_key(head, &name.node, span) else {
                        return Ty::recovery();
                    };
                    // A function reached through its module is an ordinary
                    // value; a type is not, exactly as a bare type name is
                    // not.
                    return match self.functions.get(&key) {
                        Some(sig) => sig.as_value(),
                        None => {
                            let shape = self.declared_shape(&key);
                            self.diagnostics.push(not_a_value(
                                &format!("{head}.{}", name.node),
                                shape,
                                span,
                            ));
                            Ty::recovery()
                        }
                    };
                }
            }
        }
        let base_ty = self.expr(base, None);
        self.field_of(&base_ty, name, span)
    }

    fn field_of(&mut self, base_ty: &Ty, name: &Ident, span: Span) -> Ty {
        match base_ty {
            Ty::Unknown(_) => Ty::recovery(),
            Ty::Struct(struct_name, args) => {
                // A `Ty::Struct` is only ever built from a key this table
                // answers, so there is no reachable program without one.
                let Some(sig) = self.structs.get(struct_name.as_ref()) else {
                    return Ty::placeholder();
                };
                let sig = sig.clone();
                let subst = substitution(&sig.generics, args);
                let usage = if self.assigned_place == Some(span) {
                    FieldUse::Write
                } else {
                    FieldUse::Read
                };
                if self.reject_opaque_field(struct_name, &sig, &name.node, usage, span) {
                    return Ty::recovery();
                }
                match sig.fields.iter().find(|f| f.name == name.node) {
                    Some(field) => field.ty.substitute(&subst),
                    None => {
                        let known: Vec<String> =
                            sig.fields.iter().map(|f| f.name.clone()).collect();
                        self.diagnostics.push(
                            Diagnostic::error(
                                UNKNOWN_FIELD,
                                format!("`{struct_name}` has no field `{}`", name.node),
                            )
                            .at(span)
                            .rule("A struct's fields are exactly the ones its declaration lists.")
                            .help(format!("`{struct_name}` declares {}", list(&known))),
                        );
                        Ty::recovery()
                    }
                }
            }
            Ty::Host(declared) => {
                let declared = declared.clone();
                self.host_field(&declared, name, span)
            }
            // The two builtin structs. Their fields are declared in
            // `cove_schema::builtins`, which is also where the runtime reads
            // what to build, so `error.message` and `entry.key` are one
            // description rather than a checker's and an interpreter's.
            Ty::MapEntry(_, _) | Ty::Error => self.builtin_field(base_ty, name, span),
            // A type parameter and a trait object both stand for some type
            // the checker cannot see, so neither has fields: only the traits
            // in play say what can be done with the value.
            abstract_ty @ (Ty::Param(_) | Ty::Dyn(_)) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        UNKNOWN_FIELD,
                        format!("`{abstract_ty}` has no field `{}`", name.node),
                    )
                    .at(span)
                    .rule("A trait declares methods, not fields, so a value reached only through a trait has no fields; conformance is explicit and never structural.")
                    .help(format!(
                        "declare `fn {}(self) -> ...` in the trait and call `{}()`",
                        name.node, name.node
                    )),
                );
                Ty::recovery()
            }
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        UNKNOWN_FIELD,
                        format!("`{other}` has no field `{}`", name.node),
                    )
                    .at(span)
                    .rule("Only a struct has fields.")
                    .help(format!(
                        "`{other}` is not a struct; call a method such as `{}()` instead, if one exists",
                        name.node
                    )),
                );
                Ty::recovery()
            }
        }
    }

    /// `Enum.Case` or `Enum.Case(payload...)`.
    fn enum_case(&mut self, enum_name: &str, case: &Ident, args: &[Arg], span: Span) -> Ty {
        // As in `field_of`: the key was read off a resolved type, so the
        // table answers it.
        let Some(sig) = self.enums.get(enum_name).cloned() else {
            return Ty::placeholder();
        };
        let ty = Ty::Enum(
            enum_name.into(),
            sig.generics.iter().cloned().map(Ty::Param).collect(),
        );
        let Some(found) = sig.cases.iter().find(|c| c.name == case.node) else {
            // An associated function is only reached through a call, which is
            // handled before this; a bare `Enum.name` that is not a case is a
            // case name that does not exist.
            let known: Vec<String> = sig.cases.iter().map(|c| c.name.clone()).collect();
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_CASE,
                    format!("`{enum_name}` has no case `{}`", case.node),
                )
                .at(span)
                .rule("An enum's cases are exactly the ones its declaration lists.")
                .help(format!("`{enum_name}` declares {}", list(&known))),
            );
            return ty;
        };
        if found.payload.len() != args.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    PAYLOAD_ARITY,
                    format!(
                        "`{enum_name}.{}` carries {} value(s), but {} were given",
                        case.node,
                        found.payload.len(),
                        args.len()
                    ),
                )
                .at(span)
                .label(found.span, "declared here")
                .rule("An enum case carries exactly the payload its declaration writes.")
                .help(if found.payload.is_empty() {
                    format!("write `{enum_name}.{}`", case.node)
                } else {
                    format!(
                        "write `{enum_name}.{}({})`",
                        case.node,
                        found
                            .payload
                            .iter()
                            .map(Ty::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }),
            );
        }
        // A generic enum's arguments are decided by the payload it is given,
        // exactly as a generic function's are decided by its arguments.
        let generic_set: BTreeSet<Arc<str>> = sig.generics.iter().cloned().collect();
        let mut subst: BTreeMap<Arc<str>, Ty> = BTreeMap::new();
        for (arg, payload) in args.iter().zip(&found.payload) {
            let hint = self.open(payload, &sig.generics, &subst);
            let expected = Expected::new(
                hint.clone(),
                found.span,
                format!("this case carries a `{hint}`"),
            );
            let found_ty = self.expr(&arg.value, Some(&expected));
            unify(payload, &found_ty, &generic_set, &mut subst, &self.view());
        }
        for arg in args.iter().skip(found.payload.len()) {
            self.expr(&arg.value, None);
        }
        self.open(&ty, &sig.generics, &subst)
    }

    fn unary(&mut self, op: UnaryOp, operand: &Expr, span: Span) -> Ty {
        let ty = self.expr(operand, None);
        if ty.is_wild() {
            return ty;
        }
        match (op, &ty) {
            (UnaryOp::Not, Ty::Bool) => Ty::Bool,
            (UnaryOp::Neg, Ty::Int) => Ty::Int,
            (UnaryOp::Neg, Ty::Float) => Ty::Float,
            (UnaryOp::Neg, Ty::Duration) => Ty::Duration,
            _ => {
                let symbol = match op {
                    UnaryOp::Not => "!",
                    UnaryOp::Neg => "-",
                };
                self.diagnostics.push(
                    Diagnostic::error(OPERATOR, format!("`{symbol}` is not defined for `{ty}`"))
                        .at(span)
                        .rule("There are no implicit numeric, string, or boolean conversions.")
                        .help(match op {
                            UnaryOp::Not => {
                                "`!` negates a `Bool`; compare instead, as in `x == 0`".to_string()
                            }
                            UnaryOp::Neg => {
                                "`-` negates an `Int`, a `Float`, or a `Duration`".to_string()
                            }
                        }),
                );
                Ty::recovery()
            }
        }
    }

    fn binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, span: Span) -> Ty {
        let left = self.expr(lhs, None);
        let right = self.expr(rhs, None);
        self.binary_result(op, &left, &right, span)
    }

    /// The type of `left op right`, mirroring the operators the runtime
    /// defines. Mixed operands are rejected: the card allows no implicit
    /// numeric, string, or boolean conversions, and this is that rule made
    /// static.
    fn binary_result(&mut self, op: BinaryOp, left: &Ty, right: &Ty, span: Span) -> Ty {
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let mut ok = true;
                for ty in [left, right] {
                    if !ty.is_wild() && *ty != Ty::Bool {
                        ok = false;
                    }
                }
                if !ok {
                    self.operator_error(op, left, right, span, "`&&` and `||` combine two `Bool`s");
                }
                Ty::Bool
            }
            BinaryOp::Eq | BinaryOp::Ne => {
                if !left.matches(right) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            OPERATOR,
                            format!("cannot compare `{left}` with `{right}`"),
                        )
                        .at(span)
                        .rule("`==` means value equality between values of the same type.")
                        .help(format!(
                            "convert one side explicitly so both are `{left}`, or compare values that already share a type"
                        )),
                    );
                }
                Ty::Bool
            }
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                if left.is_wild() || right.is_wild() {
                    return left.join(right);
                }
                if left != right {
                    self.operator_error(
                        op,
                        left,
                        right,
                        span,
                        "arithmetic combines two values of the same type",
                    );
                    return Ty::recovery();
                }
                match left {
                    Ty::Int | Ty::Float => left.clone(),
                    Ty::Duration if matches!(op, BinaryOp::Add | BinaryOp::Sub) => Ty::Duration,
                    Ty::Str if op == BinaryOp::Add => {
                        self.diagnostics.push(
                            Diagnostic::error(OPERATOR, "`+` is not defined for `String`")
                                .at(span)
                                .rule("There are no implicit string conversions.")
                                .help("use string interpolation, such as \"{left}{right}\""),
                        );
                        Ty::recovery()
                    }
                    _ => {
                        self.operator_error(
                            op,
                            left,
                            right,
                            span,
                            "arithmetic is defined for `Int`, `Float`, and (for `+` and `-`) `Duration`",
                        );
                        Ty::recovery()
                    }
                }
            }
            // `is` asks a narrower question than `==`: whether two operands
            // are the same shared storage, which only a handful of types
            // (today, only `Vector`) even have. A type mismatch is rejected
            // exactly like `==`; a same-typed operand that is not one of
            // those types is rejected too, since the Language Card says
            // identity is explicit "when available" — it is not silently
            // `false` for a type that has none.
            BinaryOp::Is => {
                if !left.matches(right) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            OPERATOR,
                            format!("cannot compare the identity of `{left}` with `{right}`"),
                        )
                        .at(span)
                        .rule("`is` compares identity between values of the same type.")
                        .help(format!(
                            "convert one side explicitly so both are `{left}`, or compare values that already share a type"
                        )),
                    );
                    return Ty::Bool;
                }
                if left.is_wild() || matches!(left, Ty::Vector(_)) {
                    return Ty::Bool;
                }
                self.diagnostics.push(
                    Diagnostic::error(
                        OPERATOR,
                        format!("identity is not available for `{left}`"),
                    )
                    .at(span)
                    .rule("`==` means value equality. Identity, when available, is explicit.")
                    .help(
                        "`is` is defined for `Vector`; compare other values with `==`, or call `toArray()` for an independent copy",
                    ),
                );
                Ty::Bool
            }
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                if left.is_wild() || right.is_wild() {
                    return Ty::Bool;
                }
                if left != right {
                    self.operator_error(
                        op,
                        left,
                        right,
                        span,
                        "an ordering compares two values of the same type",
                    );
                } else if !matches!(left, Ty::Int | Ty::Float | Ty::Duration | Ty::Str) {
                    self.operator_error(
                        op,
                        left,
                        right,
                        span,
                        "`<`, `<=`, `>`, and `>=` are defined for `Int`, `Float`, `Duration`, and `String`",
                    );
                }
                Ty::Bool
            }
        }
    }

    fn operator_error(&mut self, op: BinaryOp, left: &Ty, right: &Ty, span: Span, help: &str) {
        let symbol = operator_symbol(op);
        self.diagnostics.push(
            Diagnostic::error(
                OPERATOR,
                format!("`{symbol}` is not defined for `{left}` and `{right}`"),
            )
            .at(span)
            .rule("There are no implicit numeric, string, or boolean conversions.")
            .help(help.to_string()),
        );
    }

    fn assign(&mut self, op: Option<BinaryOp>, target: &Expr, value: &Expr, span: Span) -> Ty {
        if !matches!(target.kind, ExprKind::Ident(_) | ExprKind::Field { .. }) {
            self.diagnostics.push(
                Diagnostic::error(
                    NOT_A_PLACE,
                    "this expression is not a place, so it cannot be assigned",
                )
                .at(target.span)
                .rule("Only a binding or a field of one is a place.")
                .help("assign to a `var` binding, or to a field of one"),
            );
            self.expr(value, None);
            return Ty::Unit;
        }
        // The target is a place; whether it is a *writable* one is decided
        // from the binding it is rooted at. Reported before the types are
        // checked and the walk carries on afterwards, because an assignment
        // whose value is also the wrong type is two mistakes and both are
        // worth saying.
        if self.place_mutability(target) == Some(false) {
            let place = place_text(target);
            self.diagnostics.push(
                Diagnostic::error(
                    READ_ONLY_PLACE,
                    format!("cannot assign to `{place}`, which is a read-only place"),
                )
                .at(span)
                .rule("`let` creates a read-only place; `var` creates a mutable place.")
                .help(format!(
                    "declare it with `var {place}` to make it assignable"
                )),
            );
        }
        // The target is checked as the place it is, so a field refused
        // across an opaque boundary is refused as a write rather than as a
        // read. Only the target itself is the place: the value, and any
        // field read on the way to the place, are checked as ordinary
        // expressions.
        let outer = std::mem::replace(
            &mut self.assigned_place,
            matches!(target.kind, ExprKind::Field { .. }).then_some(target.span),
        );
        let target_ty = self.expr(target, None);
        self.assigned_place = outer;
        match op {
            None => {
                let expected = Expected::new(
                    target_ty.clone(),
                    target.span,
                    format!("the assigned place is `{target_ty}`"),
                );
                self.expr(value, Some(&expected));
            }
            Some(op) => {
                let value_ty = self.expr(value, None);
                let result = self.binary_result(op, &target_ty, &value_ty, span);
                let expected = Expected::new(
                    target_ty.clone(),
                    target.span,
                    format!("the assigned place is `{target_ty}`"),
                );
                self.expect(&result, &expected, span);
            }
        }
        Ty::Unit
    }

    /// `expr?`, which returns the failure from the current function.
    fn try_expr(&mut self, inner: &Expr, span: Span) -> Ty {
        let ty = self.expr(inner, None);
        match &ty {
            Ty::Unknown(_) | Ty::Never => Ty::recovery(),
            Ty::Result(ok, error) => {
                let (ok, error) = ((**ok).clone(), (**error).clone());
                match self.ret.clone() {
                    Ty::Unknown(_) => {}
                    Ty::Result(_, ret_error) if error.matches(&ret_error) => {}
                    Ty::Result(_, ret_error) => self.diagnostics.push(
                        Diagnostic::error(
                            TRY_RETURN,
                            format!(
                                "`?` propagates `{error}`, but this function returns `{ret_error}` as its failure"
                            ),
                        )
                        .at(span)
                        .label(self.ret_span, format!("the declared failure type is `{ret_error}`"))
                        .rule("`expr?` returns the error from the current function, so the two failure types must be the same.")
                        .help(format!(
                            "map the failure first, as in `expr.mapError {{ ... }}?`, or declare this function `-> Result<_, {error}>`"
                        )),
                    ),
                    other => self.diagnostics.push(
                        Diagnostic::error(
                            TRY_RETURN,
                            format!("`?` needs a function that returns a `Result`, but this one returns `{other}`"),
                        )
                        .at(span)
                        .label(self.ret_span, format!("the declared return type is `{other}`"))
                        .rule("`expr?` returns the error from the current function.")
                        .help(format!("declare this function `-> Result<{other}, {error}>`, or handle the `Err` with `unwrapOr`")),
                    ),
                }
                ok
            }
            Ty::Option(inner_ty) => {
                let inner_ty = (**inner_ty).clone();
                match self.ret.clone() {
                    Ty::Unknown(_) | Ty::Option(_) => {}
                    other => self.diagnostics.push(
                        Diagnostic::error(
                            TRY_RETURN,
                            format!("`?` on an `Option` needs a function that returns an `Option`, but this one returns `{other}`"),
                        )
                        .at(span)
                        .label(self.ret_span, format!("the declared return type is `{other}`"))
                        .rule("`expr?` returns the missing value from the current function.")
                        .help(format!("declare this function `-> Option<{other}>`, or handle the `None` with `unwrapOr`")),
                    ),
                }
                inner_ty
            }
            Ty::Task(inner_ty) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        TRY_OPERAND,
                        format!(
                            "`?` needs a `Result` or an `Option`, but found `Task<{inner_ty}>`"
                        ),
                    )
                    .at(span)
                    .rule("`expr?` returns the error from the current function.")
                    .help("settle the task first, as in `task.await()?`"),
                );
                Ty::recovery()
            }
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        TRY_OPERAND,
                        format!("`?` needs a `Result` or an `Option`, but found `{other}`"),
                    )
                    .at(span)
                    .rule("`expr?` returns the error from the current function.")
                    .help(format!("`{other}` cannot fail, so drop the `?`")),
                );
                Ty::recovery()
            }
        }
    }

    fn await_expr(&mut self, inner: &Expr, span: Span) -> Ty {
        let ty = self.expr(inner, None);
        match &ty {
            Ty::Unknown(_) | Ty::Never => Ty::recovery(),
            Ty::Task(inner_ty) => (**inner_ty).clone(),
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        AWAIT_OPERAND,
                        format!("`await` needs a task, but found `{other}`"),
                    )
                    .at(span)
                    .rule("`await` settles a task. Only a task spawned into a scope, or one returned by an `async fn`, has a value to settle.")
                    .help("call an `async fn`, or spawn the work into a task scope, and await that handle"),
                );
                Ty::recovery()
            }
        }
    }

    fn condition(&mut self, condition: &Expr) -> Ty {
        let ty = self.expr(condition, None);
        if !ty.matches(&Ty::Bool) {
            self.diagnostics.push(
                Diagnostic::error(
                    CONDITION,
                    format!("a condition must be a `Bool`, but found `{ty}`"),
                )
                .at(condition.span)
                .rule("There are no implicit boolean conversions.")
                .help(condition_help(&ty)),
            );
        }
        Ty::Bool
    }

    /// An `if` with an `else` is an expression whose branches must agree.
    ///
    /// An `if` with no `else` is a statement: its type is `()` and the value
    /// of its branch is discarded, because there is no second branch to give
    /// the missing case a value.
    fn if_expr(
        &mut self,
        condition: &Expr,
        then_branch: &Block,
        else_branch: Option<&Expr>,
        span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        self.condition(condition);
        let Some(else_branch) = else_branch else {
            self.block(then_branch, None);
            if let Some(expected) = expected {
                self.expect(&Ty::Unit, expected, span);
            }
            return Ty::Unit;
        };
        // With no expectation the branches are only answerable to each other,
        // and either of them may be the one that says what the value is:
        // `if c { None } else { Some(1) }` is an `Option<Int>` and nothing in
        // it was left unproved. Which branch says so is not known until both
        // have been walked, so with nothing expected they are walked once
        // with nothing reported and then again against what that found.
        let settled = match expected {
            Some(_) => None,
            None if self.probing => None,
            None => self
                .probe(|checker| {
                    let then_ty = checker.block(then_branch, None);
                    let else_ty = checker.expr(else_branch, None);
                    then_ty
                        .matches(&else_ty)
                        .then(|| then_ty.join(&else_ty))
                        .filter(|ty| !ty.is_wild())
                })
                .map(|ty| {
                    let label = format!("both branches produce `{ty}`");
                    Expected::new(ty, span, label)
                }),
        };
        let hint = expected.or(settled.as_ref());
        let then_ty = self.block(then_branch, hint);
        let else_ty = self.expr(else_branch, hint);
        // With an expectation, both branches were already checked against it
        // and a disagreement was reported there; without one, the branches
        // are only answerable to each other.
        if expected.is_none() && !then_ty.matches(&else_ty) {
            self.branches_disagree(then_branch.span, else_branch.span, &then_ty, &else_ty);
        }
        then_ty.join(&else_ty)
    }

    fn branches_disagree(&mut self, first: Span, second: Span, first_ty: &Ty, second_ty: &Ty) {
        self.diagnostics.push(
            Diagnostic::error(
                BRANCHES,
                format!("this branch produces `{second_ty}`, but the other produces `{first_ty}`"),
            )
            .at(second)
            .label(first, format!("this branch produces `{first_ty}`"))
            .rule(
                "Every branch of an `if` or `match` used as an expression produces the same type.",
            )
            .help(format!(
                "make both branches produce `{first_ty}`, or bind them separately"
            )),
        );
    }

    fn match_expr(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        let scrutinee_ty = self.expr(scrutinee, None);
        let mut result: Option<(Ty, Span)> = None;
        for arm in arms {
            self.scopes.push(BTreeMap::new());
            self.pattern(&arm.pattern, &scrutinee_ty);
            let ty = self.expr(&arm.body, expected);
            self.scopes.pop();
            result = Some(match result {
                None => (ty, arm.body.span),
                Some((previous, previous_span)) => {
                    if expected.is_none() && !previous.matches(&ty) {
                        self.branches_disagree(previous_span, arm.body.span, &previous, &ty);
                    }
                    (previous.join(&ty), previous_span)
                }
            });
        }
        let _ = span;
        match result {
            Some((ty, _)) => ty,
            // A `match` with no arms produces nothing; resolution already
            // reports it as non-exhaustive.
            None => Ty::Never,
        }
    }

    /// Checks a pattern against the scrutinee's type and binds its names.
    ///
    /// Case names and exhaustiveness belong to resolution, which reports them
    /// from the arms alone; this adds what only a type can say — that the
    /// pattern's enum is the scrutinee's enum, and that a payload has the
    /// arity and types the case declares.
    fn pattern(&mut self, pattern: &Pattern, scrutinee: &Ty) {
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => self.declare(name, scrutinee.clone(), false),
            PatternKind::Literal(expr) => {
                let ty = self.expr(expr, None);
                if !ty.matches(scrutinee) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            PATTERN,
                            format!(
                                "this pattern matches `{ty}`, but the scrutinee is `{scrutinee}`"
                            ),
                        )
                        .at(pattern.span)
                        .rule("A pattern matches values of the scrutinee's type.")
                        .help(format!(
                            "write a `{scrutinee}` literal, or a binding such as `other`"
                        )),
                    );
                }
            }
            PatternKind::Variant { path, payload } => {
                self.variant_pattern(pattern.span, path, payload, scrutinee)
            }
        }
    }

    fn variant_pattern(&mut self, span: Span, path: &[Ident], payload: &[Pattern], scrutinee: &Ty) {
        let case = path.last().expect("a variant path is never empty");
        let payload_types: Option<Vec<Ty>> = match scrutinee {
            Ty::Unknown(_) | Ty::Never => None,
            // The language's own enums declare their cases in
            // `cove_schema::builtins`, and a case's payload is written in the
            // parameters the scrutinee binds: `Some` carries a `T`, so
            // `Some(n)` against an `Option<Int>` binds an `Int`. A case the
            // schema does not declare answers `None` here, because resolution
            // is what reports an arm that names one.
            Ty::Option(_) | Ty::Result(_, _) => builtin_case_payload(scrutinee, &case.node),
            Ty::Enum(name, args) => {
                if let [qualifier, _] = path {
                    if self.key(&qualifier.node) != **name {
                        self.diagnostics.push(
                            Diagnostic::error(
                                PATTERN,
                                format!(
                                    "this pattern matches `{}`, but the scrutinee is `{name}`",
                                    qualifier.node
                                ),
                            )
                            .at(span)
                            .rule("A pattern matches values of the scrutinee's type.")
                            .help(format!(
                                "write a `{name}` case, such as `{name}.{}`",
                                first_case_of(self.enums.get(name.as_ref()))
                            )),
                        );
                        None
                    } else {
                        self.case_payload(name, &case.node, args)
                    }
                } else {
                    self.case_payload(name, &case.node, args)
                }
            }
            // A host module's enum has cases and nothing inside them: the
            // schema writes `cases: &["Get", "Post"]` and gives them no
            // payload to bind.
            Ty::Host(declared) => match self.host_declared_type(declared) {
                Some(schema) if schema.cases.contains(&case.node.as_str()) => Some(Vec::new()),
                Some(schema) if schema.is_enum() => {
                    let known: Vec<String> =
                        schema.cases.iter().map(|c| (*c).to_string()).collect();
                    self.diagnostics.push(
                        Diagnostic::error(
                            UNKNOWN_CASE,
                            format!("`{declared}` has no case `{}`", case.node),
                        )
                        .at(span)
                        .rule(HOST_SCHEMA_RULE)
                        .help(format!("`{declared}` declares {}", list(&known))),
                    );
                    None
                }
                _ => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            PATTERN,
                            format!(
                                "`{declared}` has no cases, so it cannot be matched by `{}`",
                                case.node
                            ),
                        )
                        .at(span)
                        .rule(HOST_SCHEMA_RULE)
                        .help(format!(
                            "match a `{declared}` with a binding, or read one of its fields"
                        )),
                    );
                    None
                }
            },
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        PATTERN,
                        format!(
                            "`{other}` has no cases, so it cannot be matched by `{}`",
                            case.node
                        ),
                    )
                    .at(span)
                    .rule("A variant pattern matches an enum case.")
                    .help(format!(
                        "match a literal `{other}`, or bind the value with a name"
                    )),
                );
                None
            }
        };

        let Some(types) = payload_types else {
            for sub in payload {
                self.pattern(sub, &Ty::recovery());
            }
            return;
        };
        if types.len() != payload.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    PAYLOAD_ARITY,
                    format!(
                        "`{}` carries {} value(s), but this pattern binds {}",
                        case.node,
                        types.len(),
                        payload.len()
                    ),
                )
                .at(span)
                .rule("A pattern binds exactly the payload its case declares.")
                .help(if types.is_empty() {
                    format!("write `{}`", case.node)
                } else {
                    format!(
                        "write `{}({})`",
                        case.node,
                        types.iter().map(|_| "value").collect::<Vec<_>>().join(", ")
                    )
                }),
            );
        }
        for (sub, ty) in payload.iter().zip(types.iter()) {
            self.pattern(sub, ty);
        }
        for sub in payload.iter().skip(types.len()) {
            self.pattern(sub, &Ty::recovery());
        }
    }

    /// The payload types of `case` on the enum `name`, substituted with the
    /// scrutinee's type arguments, or `None` when the enum has no such case —
    /// resolution reports that.
    fn case_payload(&mut self, name: &str, case: &str, args: &[Ty]) -> Option<Vec<Ty>> {
        let sig = self.enums.get(name)?;
        let subst = substitution(&sig.generics, args);
        let found = sig.cases.iter().find(|c| c.name == case)?;
        Some(
            found
                .payload
                .iter()
                .map(|ty| ty.substitute(&subst))
                .collect(),
        )
    }

    fn for_expr(&mut self, binding: &Ident, iterable: &Expr, body: &Block) -> Ty {
        let ty = self.expr(iterable, None);
        let element = match &ty {
            Ty::Unknown(_) | Ty::Never => Ty::recovery(),
            Ty::Array(inner) | Ty::Vector(inner) | Ty::Set(inner) => (**inner).clone(),
            Ty::Range => Ty::Int,
            // A `Map` iterates in ascending key order, binding each pair as
            // the same `MapEntry` shape `Map.of` accepts.
            Ty::Map(key, value) => Ty::MapEntry(key.clone(), value.clone()),
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        ITERABLE,
                        format!(
                            "`for` iterates an `Array`, a `Vector`, a `Range`, a `Set`, or a `Map`, but found `{other}`"
                        ),
                    )
                    .at(iterable.span)
                    .rule("`for` iterates a sequence; iteration order is defined by each collection type.")
                    .help(iterable_help(other)),
                );
                Ty::recovery()
            }
        };
        self.scopes.push(BTreeMap::new());
        self.declare(&binding.node, element, false);
        self.block(body, None);
        self.scopes.pop();
        Ty::Unit
    }

    /// A lambda takes its parameter types from the expected type at the call
    /// site, as ADR 0004 decides; a parameter it writes for itself is used as
    /// written.
    ///
    /// One shape it may not write is a variadic parameter
    /// ([`VARIADIC_LAMBDA`]), at any position. See the comment at the
    /// refusal for why, and for what it leaves undecided.
    fn lambda(
        &mut self,
        is_async: bool,
        params: &[Param],
        body: &Block,
        span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        let hint = match expected.map(|e| &e.ty) {
            Some(Ty::Fn(func)) => Some(func.clone()),
            _ => None,
        };
        // Whether anything at all says what this function value is. A
        // written function type says it exactly. An expected type the
        // checker has already abstained about — a host with no schema, a
        // schema's `Any` — says that nothing here is being stated, which is
        // an answer of its own and one reported where the abstention was
        // made. No expected type at all is the language gap, and it is the
        // only case this pass has to name.
        let stated = expected.is_some();
        // What the place holding this value says it *produces*, which is a
        // narrower question than what it says about the value.
        let stated_ret: Option<Ty> = match (hint.as_ref(), expected) {
            // A written or schema-declared result type, or one this pass
            // abstained about, which is an answer as well.
            (Some(func), _) if !func.ret.holds_placeholder() => Some(func.ret.clone()),
            // An expected function type whose own result this pass left
            // open: `Result.mapError`'s callback produces whatever its body
            // produces, and the expectation states its parameters only. So
            // nothing anywhere says what an early `return` in it has to
            // agree with, which is the gap `LAMBDA_RETURN` names.
            (Some(_), _) => None,
            // Not a function type at all. An expectation this pass abstained
            // about answers for the whole value, its result included; any
            // other one is a mismatch reported where the value is given, and
            // saying a second time that the result is unstated would be the
            // same mistake twice.
            (None, Some(_)) => Some(Checker::abstention_of(expected)),
            (None, None) => None,
        };
        if let Some(func) = &hint {
            if func.params.len() != params.len() {
                self.diagnostics.push(
                    Diagnostic::error(
                        ARITY,
                        format!(
                            "this function takes {} parameter(s), but {} were expected here",
                            params.len(),
                            func.params.len()
                        ),
                    )
                    .at(span)
                    .rule("A function value has exactly the parameters the place that holds it declares.")
                    .help(format!("write `fn({}) {{ ... }}`", (0..func.params.len()).map(|i| format!("p{i}")).collect::<Vec<_>>().join(", "))),
                );
            }
        }

        let mut param_types = Vec::with_capacity(params.len());
        // Everything outside this scope is a capture from here on, and
        // `Env::declare_capture` binds a capture read-only. See
        // `Checker::capture_floor`.
        let outer_floor = std::mem::replace(&mut self.capture_floor, self.scopes.len());
        self.scopes.push(BTreeMap::new());
        for (index, param) in params.iter().enumerate() {
            let ty = match &param.ty {
                Some(written) => self.resolve(written),
                // A lambda's parameters are the one kind the language
                // infers, and the only thing they are inferred from is the
                // expected type at the place the value is given to. With no
                // such place there is nothing to infer from, and the body is
                // then checked against nothing wherever it uses the
                // parameter.
                None => match hint.as_ref().and_then(|f| f.params.get(index)) {
                    Some(ty) => ty.clone(),
                    None if stated => Checker::abstention_of(expected),
                    None => {
                        self.diagnostics.push(unconstrained(
                            format!("nothing says what `{}` is", param.name.node),
                            format!(
                                "write the type, as in `{}: <type>`, or give this function value to a place that declares one",
                                param.name.node
                            ),
                            param.span,
                        ));
                        Ty::recovery()
                    }
                },
            };
            // A function value's parameters are its function type's, and
            // ADR 0016 says a function type in Cove names a fixed list of
            // them — the same rule `cove::type::variadic_as_value` states
            // from the other side, where a variadic host operation is used
            // as a value and no `fn` type can be written for it. A `...`
            // here asks for a parameter list that is a run-time fact: how
            // many arguments it gathers is decided at the call, and a call
            // through a value has no declaration in reach to gather
            // against.
            //
            // Which is to say this leans toward "a closure's parameter list
            // is fixed", because that is what the rest of the language
            // already says and it is the smaller of the two languages. What
            // a variadic parameter on a function value should *mean* is not
            // settled here, and the other reading — teach a function type to
            // carry variadicity, so that a call through a value gathers — is
            // still open; it needs ADR 0016's decision revisited, and
            // nothing below stands in its way. Issue #168 is where both are
            // written down.
            //
            // The parameter binds a recovery unknown rather than what was
            // written, so that this is one diagnostic and not a cascade
            // through every use of the name in the body — the same reason
            // `MISSING_PARAMETER_TYPE` does it.
            let ty = if param.variadic {
                self.diagnostics.push(
                    Diagnostic::error(
                        VARIADIC_LAMBDA,
                        format!(
                            "parameter `{}` is variadic, so it cannot be written on a function value",
                            param.name.node
                        ),
                    )
                    .at(param.span)
                    .rule("A variadic parameter is written on a declaration: a function value has exactly the parameters its function type names, and a function type names a fixed list of them.")
                    .help(format!(
                        "remove the `...` and give `{}` an `Array` type, passing one at the call; or declare an `fn`, which a call reaches by name and can gather arguments for",
                        param.name.node
                    )),
                );
                Ty::recovery()
            } else {
                ty
            };
            param_types.push(ty.clone());
            self.declare(&param.name.node, ty, param.is_var);
        }

        // The expected type decides the result only when it has one to give;
        // otherwise the body does.
        let declared_ret = stated_ret.clone().filter(|ty| !ty.is_wild());
        let outer_ret = std::mem::replace(
            &mut self.ret,
            stated_ret.clone().unwrap_or_else(Ty::placeholder),
        );
        let outer_span = std::mem::replace(&mut self.ret_span, span);
        let outer_stated = std::mem::replace(&mut self.ret_stated, stated_ret.is_some());
        let expected_body = stated_ret.clone().map(|ty| match ty.is_wild() {
            // An abstention passed on rather than dropped: a body given to a
            // place this pass said nothing about is not asked to state a type
            // nothing outside it stated either.
            true => Expected::abstained(ty),
            false => {
                let label = format!("this function value produces `{ty}`");
                Expected::new(ty, span, label)
            }
        });
        let body_ty = self.block(body, expected_body.as_ref());
        self.ret = outer_ret;
        self.ret_span = outer_span;
        self.ret_stated = outer_stated;
        self.scopes.pop();
        self.capture_floor = outer_floor;

        let value = Ty::func(is_async, param_types, declared_ret.unwrap_or(body_ty));
        // A function value given to a place that is not a function type is a
        // mismatch like any other, and this is where it is reported.
        // `Checker::expr` hands the expectation to this method rather than
        // checking the result against it afterwards, because a lambda *reads*
        // the expectation to type its parameters; so the check that skips is
        // made here, for exactly the expectations a lambda could not read. An
        // expected function type is left alone — a disagreeing one has
        // already been reported against the parameters and the body, which
        // says where it disagrees — and so is an unknown, which agrees with
        // everything by construction.
        if let Some(expected) = expected {
            if !matches!(expected.ty, Ty::Fn(_)) && !expected.ty.is_wild() {
                self.expect(&value, expected, span);
            }
        }
        value
    }

    // -------------------------------------------------------------- calls

    /// A call, resolved the way the interpreter resolves one: a local
    /// binding, then a declaration of this module, then a host item, then a
    /// builtin.
    ///
    /// `id` names the call expression itself rather than any of its parts,
    /// because that is what a resolved target is recorded against: a
    /// consumer holding the call is asking which declaration it reaches.
    #[allow(clippy::too_many_arguments)]
    fn call(
        &mut self,
        id: ExprId,
        callee: &Expr,
        generics: &[Type],
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        // Before the callee is resolved, exactly as `Interpreter::eval_args`
        // aliases a `var` argument before it knows what it is calling.
        self.var_arguments(args);
        match &callee.kind {
            ExprKind::Ident(name) if self.lookup(name).is_none() => {
                self.call_named(name, generics, args, trailing, span, callee.span, expected)
            }
            ExprKind::Field { base, name } => {
                if let ExprKind::Ident(head) = &base.kind {
                    if self.lookup(head).is_none() {
                        if let Some(ty) =
                            self.call_qualified(id, head, name, args, trailing, span, expected)
                        {
                            return ty;
                        }
                    }
                }
                let receiver = self.expr(base, None);
                self.mutating_receiver(&receiver, name, base, span);
                self.method_call(id, &receiver, name, args, trailing, span)
            }
            _ => {
                let callee_ty = self.expr(callee, None);
                self.call_value(&callee_ty, args, trailing, span, callee.span)
            }
        }
    }

    /// `name(...)` where `name` is not a local binding.
    #[allow(clippy::too_many_arguments)]
    fn call_named(
        &mut self,
        name: &str,
        generics: &[Type],
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        callee_span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        let key = self.key(name);
        if let Some(sig) = self.functions.get(&key).cloned() {
            let explicit = generics.iter().map(|ty| self.resolve(ty)).collect();
            return self.call_signature(&sig, &format!("`{name}`"), explicit, args, trailing, span);
        }
        if let Some(sig) = self.structs.get(&key).cloned() {
            return self.struct_init(&key, &sig, args, trailing, span, expected);
        }
        if self.enums.contains_key(&key) {
            let cases = first_case_of(self.enums.get(&key));
            self.diagnostics.push(
                Diagnostic::error(NOT_CALLABLE, format!("`{name}` is an enum, not a function"))
                    .at(callee_span)
                    .rule("An enum value is one of its cases; the enum itself is not callable.")
                    .help(format!("name a case, such as `{name}.{cases}`")),
            );
            self.check_args_freely(args, trailing);
            return Ty::recovery();
        }
        // `use console.println` makes `println(...)` the same call as
        // `console.println(...)`, so it is checked against the same schema
        // entry.
        if let Some(module) = self.module.host_items.get(name).cloned() {
            return self.host_call(&module, name, args, trailing, span);
        }
        if name == MAP_ENTRY.name {
            return self.map_entry(args, trailing, span);
        }
        if let Some(ty) = self.assertion(name, args, trailing, span) {
            return ty;
        }
        if let Some(ty) = self.constructor(name, args, trailing, span, expected) {
            return ty;
        }
        if name == NONE_CASE.name {
            self.diagnostics.push(
                Diagnostic::error(NOT_CALLABLE, "`None` is a value, not a call")
                    .at(callee_span)
                    .rule("`None` is the empty case of `Option`, which carries nothing.")
                    .help("write `None`"),
            );
            self.check_args_freely(args, trailing);
            return Ty::Option(Box::new(Ty::recovery()));
        }
        self.check_args_freely(args, trailing);
        self.unresolved_name(name, callee_span)
    }

    /// `assert(condition)` and `assertEqual(actual, expected)`.
    ///
    /// These are builtins rather than a library because a failure message
    /// names the source text of the condition, which only the compiler has.
    /// Both report failure as an ordinary `Err`, so `?` works on them inside
    /// a test and a failing assertion is an expected failure rather than a
    /// panic.
    ///
    /// The signature comes from `cove_schema::builtins::FREE_BUILTINS`, which
    /// the runtime dispatches out of as well, so an assertion cannot take one
    /// number of arguments here and another there.
    fn assertion(
        &mut self,
        name: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Option<Ty> {
        let schema = free_builtin(name, FreeBuiltinKind::Assertion)?;
        let supplied: Vec<&Expr> = args.iter().map(|arg| &arg.value).chain(trailing).collect();
        let mut bindings = FreeBindings::new(schema);
        if supplied.len() == schema.arity() {
            self.free_arguments(schema, &supplied, &mut bindings, span);
        } else {
            // An assertion given the wrong number of arguments is told that
            // and nothing else: which argument was meant to be which is no
            // longer a question with an answer.
            self.diagnostics.push(
                free_arity(schema, supplied.len(), span)
                    .rule(
                        "`assert` checks one condition; `assertEqual` compares one pair of values.",
                    )
                    .help(format!(
                        "write `{name}({})`",
                        schema
                            .params
                            .iter()
                            .map(|param| param.name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
            );
            self.check_args_freely(args, trailing);
        }
        Some(bindings.open(&schema.result))
    }

    /// `Ok(v)`, `Err(e)`, `Some(v)`, `Error("message")`, and `Shared(value)`.
    ///
    /// Which names these are, what each carries, and what each produces are
    /// the shared table's; what a call site adds is the type it expects,
    /// which is the only thing that can say what the `E` of an `Ok` is.
    fn constructor(
        &mut self,
        name: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        expected: Option<&Expected>,
    ) -> Option<Ty> {
        let schema = free_builtin(name, FreeBuiltinKind::Constructor)?;
        let mut bindings = FreeBindings::new(schema);
        if let Some(hint) = expected.map(|e| &e.ty) {
            bindings.read_off(&schema.result, hint, span);
        }
        let mut supplied: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
        if let Some(trailing) = trailing {
            supplied.push(trailing);
        }
        if supplied.len() != schema.arity() {
            self.diagnostics.push(
                free_arity(schema, supplied.len(), span)
                    .rule("A constructor carries exactly one value.")
                    .help(format!("write `{name}(value)`")),
            );
        }
        // Unlike an assertion, a constructor still checks the payload it was
        // given: there is only one parameter, so a wrong count says nothing
        // about which value was meant for it.
        self.free_arguments(schema, &supplied, &mut bindings, span);
        Some(bindings.open(&schema.result))
    }

    /// Checks a free builtin's arguments against the parameters it declares.
    ///
    /// A parameter whose type is already settled — by the type the call site
    /// expects, or by an argument that came before it — is what its argument
    /// is checked against. One that is not is settled *by* its argument,
    /// which is how `Ok(1)` decides it makes a `Result<Int, _>` and how
    /// `assertEqual`'s first argument decides what its second one must be.
    fn free_arguments(
        &mut self,
        schema: &'static FreeBuiltinSchema,
        supplied: &[&Expr],
        bindings: &mut FreeBindings,
        span: Span,
    ) {
        for (index, value) in supplied.iter().enumerate() {
            let Some(param) = schema.params.get(index) else {
                // An argument the signature has no parameter for is still
                // checked, so a mistake inside it is reported next to the
                // arity rather than after it is fixed.
                self.expr(value, None);
                continue;
            };
            let declared = bindings.open(&param.ty);
            if declared.is_wild() {
                let found = self.expr(value, None);
                // What a `Shared` wraps must be task-safe, so the payload is
                // checked here as well as where a `Shared<T>` is written as a
                // type.
                let found = if matches!(schema.result, BuiltinType::Shared(_)) {
                    self.task_safe_argument(found, span)
                } else {
                    found
                };
                bindings.bind(&param.ty, found, value.span);
            } else {
                let reason = free_builtin_reason(schema, param, &declared);
                let expected = Expected::new(declared, bindings.origin(&param.ty, span), reason);
                self.expr(value, Some(&expected));
            }
        }
    }

    /// `head.name(...)` where `head` is not a local binding: a host
    /// operation, an enum case, an associated function, or a method reached
    /// through its type's name.
    #[allow(clippy::too_many_arguments)]
    fn call_qualified(
        &mut self,
        id: ExprId,
        head: &str,
        name: &Ident,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        expected: Option<&Expected>,
    ) -> Option<Ty> {
        if self.module.host_uses.contains(head) {
            return Some(self.host_call(head, &name.node, args, trailing, span));
        }
        // A module imported whole answers a qualified call with whatever it
        // exports under that name: a function to call, or a struct to
        // initialize.
        if self.module.module_imports.contains_key(head) {
            let Some(key) = self.qualified_key(head, &name.node, span) else {
                self.check_args_freely(args, trailing);
                return Some(Ty::recovery());
            };
            if let Some(sig) = self.functions.get(&key).cloned() {
                return Some(self.call_signature(
                    &sig,
                    &format!("`{head}.{}`", name.node),
                    Vec::new(),
                    args,
                    trailing,
                    span,
                ));
            }
            if let Some(sig) = self.structs.get(&key).cloned() {
                return Some(self.struct_init(&key, &sig, args, trailing, span, expected));
            }
            // The module exports the name, but as something no call reaches:
            // an enum, a trait, or an alias.
            self.diagnostics.push(
                Diagnostic::error(
                    NOT_CALLABLE,
                    format!("`{head}.{}` is not a function", name.node),
                )
                .at(span)
                .rule("A qualified call reaches a function the named module exports, or a struct it declares.")
                .help(format!(
                    "`{head}` exports `{}` as something else; name a case or a method of it instead",
                    name.node
                )),
            );
            self.check_args_freely(args, trailing);
            return Some(Ty::recovery());
        }
        let key = self.key(head);
        if let Some(sig) = self.enums.get(&key).cloned() {
            let is_case = sig.cases.iter().any(|c| c.name == name.node);
            if !is_case {
                if let Some(sig) = self.methods.get(&(key.clone(), name.node.clone())).cloned() {
                    self.record_target(id, span.file, &key, &name.node);
                    self.check_receiver(&sig, &key, &name.node, span, false);
                    return Some(self.call_signature(
                        &sig,
                        &format!("`{head}.{}`", name.node),
                        Vec::new(),
                        args,
                        trailing,
                        span,
                    ));
                }
            }
            return Some(self.enum_case(&key, name, args, span));
        }
        if self.structs.contains_key(&key) {
            if let Some(sig) = self.methods.get(&(key.clone(), name.node.clone())).cloned() {
                self.record_target(id, span.file, &key, &name.node);
                self.check_receiver(&sig, &key, &name.node, span, false);
                return Some(self.call_signature(
                    &sig,
                    &format!("`{head}.{}`", name.node),
                    Vec::new(),
                    args,
                    trailing,
                    span,
                ));
            }
            let known = self.known_members(&key);
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_ASSOCIATED,
                    format!("`{head}` has no associated function `{}`", name.node),
                )
                .at(span)
                .rule("An associated function is declared in the type's `impl` block.")
                .help(format!("`{head}` declares {known}")),
            );
            self.check_args_freely(args, trailing);
            return Some(Ty::recovery());
        }
        if cove_schema::is_builtin_type(head) {
            return Some(self.builtin_associated(head, name, args, trailing, span));
        }
        None
    }

    // --------------------------------------------------------- host calls
    //
    // ADR 0001 asks for one description of each Host API operation's
    // argument, result, and error types, "shared by the compiler, runtime,
    // and CLI". `cove-schema` is that description and this is the compiler's
    // half of reading it: a call reaching a host module is checked against
    // the same entry `HostRegistry::dispatch` will check it against, except
    // that here the mistake still has a span to point at.
    //
    // What the checker cannot do is see a host it does not ship. An
    // embedding registers its modules at run time, so a module named in no
    // shipped schema is left exactly where it was — unchecked, and said to be
    // — and the boundary is what holds such a host to its word.

    /// `http.fetch(...)`, `http.Route(...)`, or `println(...)` reached
    /// through `use console.println`.
    fn host_call(
        &mut self,
        module: &str,
        name: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let Some(schema) = self.host_schema(module) else {
            // A module no schema describes — neither a shipped one nor one
            // the embedder handed over. Its operations are between the host
            // that registered it and the boundary, which is the one thing
            // `cove check` cannot do for a program.
            //
            // Nothing is reported *here*. The fact is about the `use` that
            // named the module and about the compilation that was not shown
            // it, not about this call: no edit to `module.name` can fix it,
            // and the remedy — handing the module's `ModuleSchema` to the
            // compiler — is one thing to say however many calls a program
            // makes. `cove::resolve::unchecked_host` puts that warning at
            // the `use`, where the remedy is.
            //
            // What the arguments are given is the abstention itself, so a
            // callback into such a host — the shape an embedding is written
            // in — is not asked to state a type nothing on this side stated.
            self.check_args_abstained(args, trailing, Ty::dynamic_boundary());
            return Ty::dynamic_boundary();
        };
        if let Some(operation) = schema.operation(name) {
            return self.call_host_operation(
                operation,
                &format!("{module}.{name}"),
                args,
                trailing,
                span,
            );
        }
        if let Some(declared) = schema.declared_type(name) {
            if !declared.is_enum() {
                return self.host_type_init(module, declared, args, trailing, span);
            }
            self.diagnostics.push(
                Diagnostic::error(
                    NOT_CALLABLE,
                    format!("`{module}.{name}` is a host enum, not a function"),
                )
                .at(span)
                .rule(HOST_SCHEMA_RULE)
                .help(format!(
                    "name a case, such as `{module}.{name}.{}`",
                    declared.cases.first().copied().unwrap_or("Case")
                )),
            );
            self.check_args_freely(args, trailing);
            return Ty::Host(format!("{module}.{name}").into());
        }
        if schema.resource(name).is_some() {
            self.diagnostics.push(
                Diagnostic::error(
                    NOT_CALLABLE,
                    format!("`{module}.{name}` is a host resource, not a function"),
                )
                .at(span)
                .rule("A host resource is opened by an operation of its module, which hands back a handle to it.")
                .help(format!(
                    "call the operation that opens one, such as {}",
                    list(&operation_names(schema.operations))
                )),
            );
            self.check_args_freely(args, trailing);
            return Ty::recovery();
        }
        self.diagnostics.push(
            Diagnostic::error(
                UNKNOWN_HOST_OPERATION,
                format!("host module `{module}` has no operation `{name}`"),
            )
            .at(span)
            .rule(HOST_SCHEMA_RULE)
            .help(format!(
                "`{module}` exposes {}",
                list(&operation_names(schema.operations))
            )),
        );
        self.check_args_freely(args, trailing);
        Ty::recovery()
    }

    /// Checks a call against one operation's declared signature.
    ///
    /// A host operation's parameters have types and no names, so they are
    /// named by position: the diagnostic for the second one reads "argument
    /// `#2`", and the runtime's own message for the same mistake counts the
    /// same way.
    fn call_host_operation(
        &mut self,
        operation: &'static OperationSchema,
        shown: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let supplied = args.len() + usize::from(trailing.is_some());
        // A spread argument stands for a sequence whose length is not known
        // here, so it is the one call whose arity cannot be counted; the
        // boundary counts it when the values exist.
        let spread = args.iter().any(|arg| arg.spread);
        if !spread && !operation.accepts(supplied) {
            self.diagnostics.push(
                Diagnostic::error(
                    ARITY,
                    format!(
                        "`{shown}` takes {}, but {supplied} were given",
                        operation.expected_arity()
                    ),
                )
                .at(span)
                .rule(HOST_SCHEMA_RULE)
                .help(declared_signature(shown, operation)),
            );
            self.check_args_freely(args, trailing);
            // The call was just rejected, so it produces nothing to say
            // anything about: noting that its result is unconstrained would
            // be a second diagnostic about a call that will never run.
            return host_ty(&operation.result);
        }
        let last = operation.params.len().saturating_sub(1);
        let params: Vec<ParamSig> = operation
            .params
            .iter()
            .enumerate()
            .map(|(index, declared)| {
                // A variadic parameter answers for every argument from its
                // own position onwards, so it is named the way the signature
                // writes it: `#1...`, not `#1`.
                let variadic = operation.variadic && index == last;
                ParamSig {
                    name: format!("#{}{}", index + 1, if variadic { "..." } else { "" }),
                    ty: host_ty(declared),
                    variadic,
                    has_default: false,
                    is_var: false,
                    span,
                }
            })
            .collect();
        self.match_arguments(
            &params,
            &[],
            BTreeMap::new(),
            args,
            trailing,
            span,
            &format!("`{shown}`"),
            "argument",
        );
        self.host_result(operation, shown, span)
    }

    /// The type a host operation's result is here, saying so where the
    /// schema declared it `Any`.
    ///
    /// `Any` in a parameter costs nothing: the operation accepts every
    /// value, so there was no check to skip. `Any` in a result is the other
    /// half of the same promise, and it does cost something — from the call
    /// onwards the program holds a value whose type no schema stated — so
    /// the call says which of the two it is rather than leaving a silent
    /// unknown to spread.
    fn host_result(&mut self, operation: &'static OperationSchema, shown: &str, span: Span) -> Ty {
        if contains_any(&operation.result) {
            self.diagnostics.push(
                Diagnostic::note(
                    UNCONSTRAINED_RESULT,
                    format!(
                        "`{shown}` declares its result `{}`, so nothing here says what this call produced",
                        operation.result
                    ),
                )
                .at(span)
                .rule("A Host API operation declares `Any` where its meaning does not depend on the type of a value: the schema promises to carry the value, not to describe it.")
                .help(format!(
                    "whatever the program does with the result of `{shown}` is checked at run time and by nothing here; {}",
                    declared_signature(shown, operation)
                )),
            );
        }
        host_ty(&operation.result)
    }

    /// `http.Route(method: ..., path: ..., handler: ...)`: a host type
    /// initialized from Cove source, exactly as a struct is.
    ///
    /// This is the one place a host type's *fields* are checked. The boundary
    /// checks a declared type by name only — ADR 0013's amendment says why —
    /// so what is checked here is that the program built the value the schema
    /// describes, not that the host did.
    fn host_type_init(
        &mut self,
        module: &str,
        declared: &'static TypeSchema,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let params: Vec<ParamSig> = declared
            .fields
            .iter()
            .map(|field| ParamSig {
                name: field.name.to_string(),
                ty: host_ty(&field.ty),
                variadic: false,
                has_default: false,
                is_var: false,
                span,
            })
            .collect();
        self.match_arguments(
            &params,
            &[],
            BTreeMap::new(),
            args,
            trailing,
            span,
            &format!("`{module}.{}`", declared.name),
            "the field",
        );
        Ty::Host(format!("{module}.{}", declared.name).into())
    }

    /// `http.Request` written as a type.
    fn host_named_type(&mut self, module: &str, name: &str, arguments: usize, span: Span) -> Ty {
        let qualified = format!("{module}.{name}");
        let Some(schema) = self.host_schema(module) else {
            self.diagnostics.push(unchecked_host_type(&qualified, span));
            return Ty::dynamic_boundary();
        };
        if !schema.declares_type(name) {
            let mut known: Vec<String> = schema
                .types
                .iter()
                .map(|declared| declared.name.to_string())
                .collect();
            known.extend(
                schema
                    .resources
                    .iter()
                    .map(|resource| resource.name.to_string()),
            );
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_HOST_TYPE,
                    format!("host module `{module}` declares no type `{name}`"),
                )
                .at(span)
                .rule(HOST_SCHEMA_RULE)
                .help(if known.is_empty() {
                    format!("`{module}` declares no types of its own")
                } else {
                    format!("`{module}` declares {}", list(&known))
                }),
            );
            return Ty::recovery();
        }
        // A host type takes no arguments, because the schema has none to
        // give it.
        self.check_type_arity(&qualified, 0, arguments, span);
        Ty::Host(qualified.into())
    }

    /// `http.Method.Get`, if `module` is a host module whose schema declares
    /// `declared` as an enum. `None` leaves the expression to be read the way
    /// it was before.
    fn host_enum_case(
        &mut self,
        module: &str,
        declared: &str,
        case: &Ident,
        span: Span,
    ) -> Option<Ty> {
        let schema = self.host_schema(module)?.declared_type(declared)?;
        if !schema.is_enum() {
            return None;
        }
        let qualified: Arc<str> = format!("{module}.{declared}").into();
        if !schema.cases.contains(&case.node.as_str()) {
            let known: Vec<String> = schema.cases.iter().map(|c| (*c).to_string()).collect();
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_CASE,
                    format!("`{qualified}` has no case `{}`", case.node),
                )
                .at(span)
                .rule(HOST_SCHEMA_RULE)
                .help(format!("`{qualified}` declares {}", list(&known))),
            );
        }
        Some(Ty::Host(qualified))
    }

    /// `error.message` and `entry.key`: a field of a builtin struct, typed
    /// from the shared table.
    ///
    /// The runtime builds both of these as ordinary struct values and has
    /// always served a read of their fields. What it had no way to tell the
    /// checker was what those fields are called, so `Error` was opaque here
    /// and answered that it had no `message` at all; declaring the fields in
    /// `cove_schema::builtins` is what closed that.
    fn builtin_field(&mut self, base_ty: &Ty, name: &Ident, span: Span) -> Ty {
        // Only `MapEntry` and `Error` reach here, and the table declares
        // both.
        let Some(schema) = builtin_schema_of(base_ty) else {
            return Ty::placeholder();
        };
        let bound = receiver_binding(schema, base_ty);
        match schema.field(&name.node) {
            Some(field) => builtin_ty(&field.ty, &bound, Some(base_ty)),
            None => {
                let known: Vec<String> = schema.fields.iter().map(|f| f.name.to_string()).collect();
                self.diagnostics.push(
                    Diagnostic::error(
                        UNKNOWN_FIELD,
                        format!("`{}` has no field `{}`", schema.name, name.node),
                    )
                    .at(span)
                    .rule("A builtin struct's fields are exactly the ones the language defines.")
                    .help(format!(
                        "`{}` declares {}",
                        schema.name,
                        list(&known)
                    )),
                );
                Ty::recovery()
            }
        }
    }

    /// `request.path`: a field of a host type, typed from the schema.
    fn host_field(&mut self, declared: &str, name: &Ident, span: Span) -> Ty {
        let Some(schema) = self.host_declared_type(declared) else {
            // A resource keeps its state on the far side of the boundary, so
            // there is nothing in it to read: everything it answers, it
            // answers as an operation.
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_FIELD,
                    format!("`{declared}` has no field `{}`", name.node),
                )
                .at(span)
                .rule("A host resource is a name for something the host keeps, so it has operations rather than fields.")
                .help(format!("call an operation on it, such as `{}()`", name.node)),
            );
            return Ty::recovery();
        };
        match schema.fields.iter().find(|f| f.name == name.node) {
            Some(field) => {
                // The other end of the `Any` promise. A schema may declare a
                // *field* `Any` as readily as a result — `http.Route.handler`
                // is one — and reading it leaves the program holding a value
                // no schema described, exactly as calling an `Any`-result
                // operation does. Same fact, same note.
                if contains_any(&field.ty) {
                    self.diagnostics.push(
                        Diagnostic::note(
                            UNCONSTRAINED_FIELD,
                            format!(
                                "`{declared}` declares `{}` as `{}`, so nothing here says what this field holds",
                                name.node, field.ty
                            ),
                        )
                        .at(span)
                        .rule("A Host API schema declares `Any` where its meaning does not depend on the type of a value: the schema promises to carry the value, not to describe it.")
                        .help(format!(
                            "whatever the program does with `{}.{}` is checked at run time and by nothing here",
                            declared, name.node
                        )),
                    );
                }
                host_ty(&field.ty)
            }
            None => {
                let known: Vec<String> = schema.fields.iter().map(|f| f.name.to_string()).collect();
                self.diagnostics.push(
                    Diagnostic::error(
                        UNKNOWN_FIELD,
                        format!("`{declared}` has no field `{}`", name.node),
                    )
                    .at(span)
                    .rule(HOST_SCHEMA_RULE)
                    .help(if known.is_empty() {
                        format!("`{declared}` carries no fields")
                    } else {
                        format!("`{declared}` declares {}", list(&known))
                    }),
                );
                Ty::recovery()
            }
        }
    }

    /// `server.handle(routes)`: an operation called on a host resource
    /// handle, checked against the same schema a module's operation is.
    fn host_method_call(
        &mut self,
        declared: &str,
        name: &Ident,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        if let Some(resource) = self.host_resource(declared) {
            if let Some(operation) = resource.operation(&name.node) {
                return self.call_host_operation(
                    operation,
                    &format!("{declared}.{}", name.node),
                    args,
                    trailing,
                    span,
                );
            }
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_HOST_OPERATION,
                    format!("`{declared}` has no operation `{}`", name.node),
                )
                .at(span)
                .rule(HOST_SCHEMA_RULE)
                .help(format!(
                    "`{declared}` answers {}",
                    list(&operation_names(resource.operations))
                )),
            );
            self.check_args_freely(args, trailing);
            return Ty::recovery();
        }
        self.diagnostics.push(
            Diagnostic::error(
                UNKNOWN_HOST_OPERATION,
                format!("`{declared}` has no operation `{}`", name.node),
            )
            .at(span)
            .rule("A host type that is plain data has fields; only a host resource answers operations.")
            .help(format!("read a field, such as `.{}`", name.node)),
        );
        self.check_args_freely(args, trailing);
        Ty::recovery()
    }

    /// `Vector.of(...)`, `Map.of(...)`, `Set.of(...)`, `Int.parse(...)`.
    ///
    /// The signatures come from [`cove_schema::builtins`], the same table the
    /// runtime's `call_associated` dispatches out of. A spread argument is
    /// left to the runtime, which rejects one in any of these calls.
    fn builtin_associated(
        &mut self,
        type_name: &str,
        name: &Ident,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        // An associated function is called on the type, so nothing binds the
        // receiver's parameters: the `T` of `Vector.of(items: T...)` is the
        // signature's own, unified at the call site.
        let declared = cove_schema::builtin(type_name)
            .and_then(|schema| schema.associated_function(&name.node));
        let Some(declared) = declared else {
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_ASSOCIATED,
                    format!("`{type_name}` has no associated function `{}`", name.node),
                )
                .at(span)
                .rule(format!(
                    "A builtin type's associated functions are {}.",
                    builtin_associated_functions()
                ))
                .help(format!(
                    "`{type_name}` has no `{}`; construct the value another way",
                    name.node
                )),
            );
            self.check_args_freely(args, trailing);
            return Ty::recovery();
        };
        let sig = builtin_sig(declared, &[], &[], None);
        let what = format!("`{type_name}.{}`", name.node);
        self.call_builtin(&sig, &what, args, trailing, span)
    }

    /// `MapEntry(key: ..., value: ...)`: a synthesized labeled call, exactly
    /// like a declared struct's initializer, that exists so `Map.of` has a
    /// call-shaped way to write the pairs it collects.
    ///
    /// Its labels are the fields [`MAP_ENTRY`] declares, which is also what
    /// the interpreter assigns the arguments to, so a call and the value it
    /// builds cannot come apart.
    fn map_entry(&mut self, args: &[Arg], trailing: Option<&Expr>, span: Span) -> Ty {
        // There is no receiver here, so the entry's own `K` and `V` are what
        // the call site settles, the way a generic function's are.
        let bound = BTreeMap::new();
        let sig = BuiltinSig {
            generics: MAP_ENTRY
                .parameters
                .iter()
                .map(|name| Arc::from(*name))
                .collect(),
            params: MAP_ENTRY
                .fields
                .iter()
                .map(|field| (field.name, builtin_ty(&field.ty, &bound, None)))
                .collect(),
            variadic: false,
            ret: Ty::MapEntry(
                Box::new(Ty::Param("K".into())),
                Box::new(Ty::Param("V".into())),
            ),
        };
        self.call_builtin(&sig, "`MapEntry`", args, trailing, span)
    }

    /// The type arguments the place holding this value states, when it
    /// states any: `let t: Tagged<String> = Tagged(n: 1)` settles `T` even
    /// though no field mentions it.
    fn expected_arguments(name: &str, expected: Option<&Expected>) -> Vec<Ty> {
        match expected.map(|e| &e.ty) {
            Some(Ty::Struct(other, args)) if other.as_ref() == name => args.clone(),
            _ => Vec::new(),
        }
    }

    /// `Type(field: value, ...)`, the synthesized labeled call the card
    /// describes.
    #[allow(clippy::too_many_arguments)]
    fn struct_init(
        &mut self,
        name: &str,
        sig: &StructSig,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        // A refusal ends the diagnosis. Matching the arguments against
        // fields this module may not name would answer the question it was
        // just refused — the labels it guessed wrong would come back as
        // `known labels: raw, count`, and a field it left out would be
        // reported against the declaring module's source. The arguments
        // themselves are still checked, since a mistake inside one is a
        // mistake either way, and the result is still this struct.
        if self.reject_opaque_construction(name, sig, span) {
            self.check_args_freely(args, trailing);
            return Ty::Struct(name.into(), vec![Ty::recovery(); sig.generics.len()]);
        }
        let generics: Vec<Arc<str>> = sig.generics.clone();
        let stated = Checker::expected_arguments(name, expected);
        let subst = self.match_arguments(
            &sig.fields,
            &generics,
            BTreeMap::new(),
            args,
            trailing,
            span,
            &format!("`{name}`"),
            "the field",
        );
        let arguments = generics
            .iter()
            .enumerate()
            .map(|(index, g)| {
                // The fields were just checked, which is what settles the
                // parameters they mention; one no field mentions can only be
                // settled by the place the value is given to.
                if let Some(ty) = subst.get(g) {
                    return ty.clone();
                }
                if let Some(ty) = stated.get(index).filter(|ty| !ty.is_wild()) {
                    return ty.clone();
                }
                // Nothing settles it, and the value carries it anyway: every
                // later use of this binding at that parameter's position is
                // checked against nothing. That is the same gap an empty
                // array literal and a bare `None` leave, and it is named the
                // same way rather than left to spread as a silent unknown.
                if !Checker::accounted_for(expected) {
                    self.diagnostics.push(unconstrained(
                        format!("nothing says what `{g}` is in `{name}<{g}>`"),
                        format!(
                            "write the type on the place that holds it, as in `let value: {name}<Int> = ...`, or give the value to a place that declares one"
                        ),
                        span,
                    ));
                }
                Ty::unconstrained()
            })
            .collect();
        Ty::Struct(name.into(), arguments)
    }

    /// Checks a call against a declared signature, unifying the callee's type
    /// parameters at the call site and substituting them into the result.
    fn call_signature(
        &mut self,
        sig: &FnSig,
        what: &str,
        explicit: Vec<Ty>,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let mut subst: BTreeMap<Arc<str>, Ty> = BTreeMap::new();
        for (param, ty) in sig.generics.iter().zip(explicit) {
            subst.insert(param.clone(), ty);
        }
        let subst = self.match_arguments(
            &sig.params,
            &sig.generics,
            subst,
            args,
            trailing,
            span,
            what,
            "the parameter",
        );
        self.check_bounds(sig, &subst, what, span);
        let ret = self.open(&sig.ret, &sig.generics, &subst);
        if sig.is_async {
            // An `async fn` is called like any other function and produces a
            // task; its value is reachable only through `await`.
            Ty::Task(Box::new(ret))
        } else {
            ret
        }
    }

    /// Calls a value of function type.
    fn call_value(
        &mut self,
        callee: &Ty,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        callee_span: Span,
    ) -> Ty {
        match callee {
            Ty::Unknown(_) | Ty::Never => {
                self.check_args_freely(args, trailing);
                Ty::recovery()
            }
            Ty::Fn(func) => {
                let params: Vec<ParamSig> = func
                    .params
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| ParamSig {
                        name: format!("#{index}"),
                        ty: ty.clone(),
                        variadic: false,
                        has_default: false,
                        // A function type has no marking to read. See
                        // `Checker::var_arguments` for what this pass does
                        // and does not decide about a call through a value.
                        is_var: false,
                        span: callee_span,
                    })
                    .collect();
                self.match_arguments(
                    &params,
                    &[],
                    BTreeMap::new(),
                    args,
                    trailing,
                    span,
                    "this function value",
                    "the parameter",
                );
                if func.is_async {
                    Ty::Task(Box::new(func.ret.clone()))
                } else {
                    func.ret.clone()
                }
            }
            other => {
                self.diagnostics.push(
                    Diagnostic::error(NOT_CALLABLE, format!("`{other}` is not a function"))
                        .at(callee_span)
                        .rule("Only a function value can be called.")
                        .help(format!(
                            "`{other}` is a value, not a function; remove the argument list"
                        )),
                );
                self.check_args_freely(args, trailing);
                Ty::recovery()
            }
        }
    }

    /// Matches call-site arguments to parameters by position and by label,
    /// checking each against the parameter's type and binding the callee's
    /// type parameters as it goes.
    ///
    /// Labels are parameter names, so a labeled argument goes to the
    /// parameter it names. Their *order* is the runtime's rule, not this
    /// one's.
    #[allow(clippy::too_many_arguments)]
    fn match_arguments(
        &mut self,
        params: &[ParamSig],
        generics: &[Arc<str>],
        mut subst: BTreeMap<Arc<str>, Ty>,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        what: &str,
        role: &str,
    ) -> BTreeMap<Arc<str>, Ty> {
        let variadic_last = params.last().is_some_and(|p| p.variadic);
        let mut slots: Vec<Option<&Arg>> = vec![None; params.len()];
        let mut rest: Vec<&Arg> = Vec::new();
        let mut next = 0usize;
        let mut labeled = false;
        // One mistake, one diagnostic: a label that names no parameter has
        // already been reported, so the parameter it failed to fill is not
        // reported as missing too.
        let mut mislabeled = false;
        let generic_set: BTreeSet<Arc<str>> = generics.iter().cloned().collect();

        for arg in args {
            match &arg.label {
                Some(label) => {
                    labeled = true;
                    match params.iter().position(|p| p.name == label.node) {
                        Some(index) => {
                            // A label whose parameter stands before one an
                            // earlier argument already filled. The same
                            // label twice lands here too, and is left to the
                            // missing-argument report the parameter it never
                            // filled already earns: one mistake, one
                            // diagnostic.
                            if index < next && slots[index].is_none() {
                                let order: Vec<&str> =
                                    params.iter().map(|p| p.name.as_str()).collect();
                                self.diagnostics.push(
                                    Diagnostic::error(
                                        LABEL_ORDER,
                                        format!(
                                            "{what} was given the label `{}` out of declaration order",
                                            label.node
                                        ),
                                    )
                                    .at(arg.span)
                                    .rule("Labeled arguments appear in declaration order, so argument order matches parameter order.")
                                    .help(format!(
                                        "write the arguments in this order: {}",
                                        order.join(", ")
                                    )),
                                );
                            }
                            slots[index] = Some(arg);
                            next = index + 1;
                        }
                        None => {
                            let known: Vec<String> =
                                params.iter().map(|p| p.name.clone()).collect();
                            self.diagnostics.push(
                                Diagnostic::error(
                                    UNKNOWN_LABEL,
                                    format!("{what} has no parameter labeled `{}`", label.node),
                                )
                                .at(arg.span)
                                .rule("Argument labels are parameter names and part of the API contract.")
                                .help(format!("known labels: {}", list(&known))),
                            );
                            self.expr(&arg.value, None);
                            mislabeled = true;
                        }
                    }
                }
                // The runtime rejects a positional argument after a labeled
                // one; there is no parameter to check this one against.
                None if labeled => {
                    self.expr(&arg.value, None);
                }
                None if variadic_last && next + 1 >= params.len() => rest.push(arg),
                None if next < params.len() => {
                    slots[next] = Some(arg);
                    next += 1;
                }
                None => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            ARITY,
                            format!(
                                "{what} takes {} argument(s), but more were given",
                                params.len()
                            ),
                        )
                        .at(arg.span)
                        .rule("A call passes exactly the arguments the declaration binds.")
                        .help(format!(
                            "{what} declares {}",
                            list(&params.iter().map(|p| p.name.clone()).collect::<Vec<_>>())
                        )),
                    );
                    self.expr(&arg.value, None);
                }
            }
        }

        // A trailing closure fills the first parameter still empty, which is
        // the variadic one when the signature ends in `...`.
        let trailing_slot = trailing.and_then(|_| {
            if variadic_last {
                None
            } else {
                slots.iter().position(Option::is_none)
            }
        });
        if let Some(trailing) = trailing {
            match trailing_slot.and_then(|index| params.get(index).map(|p| (index, p))) {
                Some((index, param)) => {
                    let param = param.clone();
                    let expected = param.ty.substitute(&subst);
                    let hint = self.open(&expected, generics, &subst);
                    let found = self.trailing_type(trailing, Some(&hint));
                    self.check_argument(
                        &found,
                        &hint,
                        &expected,
                        trailing.span,
                        &param,
                        &generic_set,
                        &mut subst,
                        role,
                    );
                    slots[index] = None;
                }
                None => {
                    let ty = self.trailing_type(trailing, None);
                    if variadic_last {
                        if let Some(param) = params.last() {
                            let param = param.clone();
                            let element = param.ty.substitute(&subst);
                            let hint = self.open(&element, generics, &subst);
                            self.check_argument(
                                &ty,
                                &hint,
                                &element,
                                trailing.span,
                                &param,
                                &generic_set,
                                &mut subst,
                                role,
                            );
                        }
                    } else {
                        self.diagnostics.push(
                            Diagnostic::error(
                                ARITY,
                                format!(
                                    "{what} takes {} argument(s), but a trailing closure was given too",
                                    params.len()
                                ),
                            )
                            .at(trailing.span)
                            .rule("A trailing closure is the call's last argument.")
                            .help("remove the trailing closure, or pass it in place of an argument"),
                        );
                    }
                }
            }
        }

        for (index, param) in params.iter().enumerate() {
            if param.variadic {
                let element = param.ty.substitute(&subst);
                let mut supplied: Vec<&Arg> = rest.clone();
                if let Some(arg) = slots[index] {
                    supplied.insert(0, arg);
                }
                for arg in supplied {
                    self.variadic_argument(
                        arg,
                        &element,
                        param,
                        generics,
                        &generic_set,
                        &mut subst,
                        role,
                    );
                }
                continue;
            }
            let Some(arg) = slots[index] else {
                if !param.has_default && trailing_slot != Some(index) && !mislabeled {
                    self.diagnostics.push(
                        Diagnostic::error(
                            MISSING_ARGUMENT,
                            format!("{what} needs {role} `{}`", param.name),
                        )
                        .at(span)
                        .label(param.span, format!("`{}` is `{}`", param.name, param.ty))
                        .rule("A call passes every parameter that has no default.")
                        .help(format!("pass `{}: <{}>`", param.name, param.ty)),
                    );
                }
                continue;
            };
            let expected = param.ty.substitute(&subst);
            let hint = self.open(&expected, generics, &subst);
            let hint_expected = Expected::new(
                hint.clone(),
                param.span,
                format!("{role} `{}` is `{}`", param.name, param.ty),
            );
            let found = self.expr(&arg.value, Some(&hint_expected));
            self.check_argument(
                &found,
                &hint,
                &expected,
                arg.span,
                param,
                &generic_set,
                &mut subst,
                role,
            );
        }
        subst
    }

    /// Binds the callee's type parameters from one argument, and reports the
    /// mismatch the expectation could not: an expectation is checked against
    /// `hint`, in which every unbound type parameter is `Unknown`, so only
    /// unification can tell that two uses of the same parameter disagree.
    #[allow(clippy::too_many_arguments)]
    fn check_argument(
        &mut self,
        found: &Ty,
        hint: &Ty,
        expected: &Ty,
        span: Span,
        param: &ParamSig,
        generics: &BTreeSet<Arc<str>>,
        subst: &mut BTreeMap<Arc<str>, Ty>,
        role: &str,
    ) {
        let unified = unify(expected, found, generics, subst, &self.view());
        if !unified && found.matches(hint) {
            let expected = expected.substitute(subst);
            self.report_argument(found, &expected, span, param, role);
        }
    }

    /// A signature type with every type parameter still unbound replaced by
    /// an unconstrained unknown, so it can be used as an expectation without
    /// pretending the call site has decided what the parameter is.
    ///
    /// The unknown is [`Ty::unconstrained`] rather than a placeholder because
    /// it *is* read: an argument is checked against it, and a lambda takes
    /// its parameter types from it. What it says is exactly what
    /// "unconstrained" means — nothing read so far states this type — and
    /// saying so is what keeps a form given to such a place from being asked
    /// to explain a silence that is not its own.
    fn open(&self, ty: &Ty, generics: &[Arc<str>], subst: &BTreeMap<Arc<str>, Ty>) -> Ty {
        if generics.is_empty() {
            return ty.clone();
        }
        let map: BTreeMap<Arc<str>, Ty> = generics
            .iter()
            .map(|g| {
                (
                    g.clone(),
                    subst.get(g).cloned().unwrap_or(Ty::unconstrained()),
                )
            })
            .collect();
        ty.substitute(&map)
    }

    /// One argument passed to a variadic parameter, which is an `Array<T>`
    /// inside the callee: each ordinary argument is a `T`, and a spread
    /// argument is a sequence of them.
    #[allow(clippy::too_many_arguments)]
    fn variadic_argument(
        &mut self,
        arg: &Arg,
        element: &Ty,
        param: &ParamSig,
        generics: &[Arc<str>],
        generic_set: &BTreeSet<Arc<str>>,
        subst: &mut BTreeMap<Arc<str>, Ty>,
        role: &str,
    ) {
        if arg.spread {
            let ty = self.expr(&arg.value, None);
            let spread_element = match &ty {
                Ty::Array(inner) | Ty::Vector(inner) => (**inner).clone(),
                Ty::Unknown(_) | Ty::Never => Ty::recovery(),
                other => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            MISMATCH,
                            format!("`...` spreads an `Array` or a `Vector`, but found `{other}`"),
                        )
                        .at(arg.span)
                        .label(param.span, format!("`{}` is variadic", param.name))
                        .rule("A variadic parameter is an `Array<T>`, so a spread argument must be a sequence of `T`.")
                        .help(format!("pass the value directly, as in `f(<{other}>)`")),
                    );
                    return;
                }
            };
            if !unify(element, &spread_element, generic_set, subst, &self.view()) {
                self.diagnostics.push(
                    Diagnostic::error(
                        MISMATCH,
                        format!("expected `{element}`, found `{spread_element}`"),
                    )
                    .at(arg.span)
                    .label(param.span, format!("`{}` is `{element}...`", param.name))
                    .rule("A variadic parameter is an `Array<T>`; every spread element is a `T`.")
                    .help(format!("spread a sequence of `{element}`")),
                );
            }
            return;
        }
        let hint = self.open(element, generics, subst);
        let hint_expected = Expected::new(
            hint.clone(),
            param.span,
            format!("{role} `{}` is `{}`", param.name, param.ty),
        );
        let found = self.expr(&arg.value, Some(&hint_expected));
        self.check_argument(
            &found,
            &hint,
            element,
            arg.span,
            param,
            generic_set,
            subst,
            role,
        );
    }

    fn report_argument(
        &mut self,
        found: &Ty,
        expected: &Ty,
        span: Span,
        param: &ParamSig,
        role: &str,
    ) {
        if found.matches(expected) {
            return;
        }
        let mut diagnostic = Diagnostic::error(
            MISMATCH,
            format!("expected `{expected}`, found `{found}`"),
        )
        .at(span)
        .label(param.span, format!("{role} `{}` is `{}`", param.name, param.ty))
        .rule("Types are nominal and there are no implicit conversions: an argument must already have the parameter's type.");
        if let Some(help) = conversion_help(expected, found) {
            diagnostic = diagnostic.help(help);
        }
        self.diagnostics.push(diagnostic);
    }

    /// A trailing block is a closure argument: `mapError { ... }` is a
    /// function of no parameters.
    fn trailing_type(&mut self, trailing: &Expr, expected: Option<&Ty>) -> Ty {
        match &trailing.kind {
            ExprKind::Block(block) => {
                let ret = match expected {
                    Some(Ty::Fn(func)) => Some(func.ret.clone()),
                    // A block given to a place this pass abstained about is
                    // a body with an explanation of its own, made where the
                    // abstention was. Passing it down is what keeps an empty
                    // array or a bare `None` inside `clock.timeout(1s) { .. }`
                    // from being asked to state a type nothing outside it
                    // stated either.
                    Some(ty) if ty.is_accounted_for() => Some(ty.clone()),
                    _ => None,
                };
                let hint = ret.clone().map(|ty| match ty.is_wild() {
                    true => Expected::abstained(ty),
                    false => {
                        let label = format!("the trailing closure produces `{ty}`");
                        Expected::new(ty, trailing.span, label)
                    }
                });
                let ty = self.block(block, hint.as_ref());
                Ty::func(
                    false,
                    Vec::new(),
                    ret.filter(|ty| !ty.is_wild()).unwrap_or(ty),
                )
            }
            _ => {
                let hint = expected.cloned().map(|ty| match ty.is_accounted_for() {
                    true => Expected::abstained(ty),
                    false => {
                        let label = format!("the trailing argument is `{ty}`");
                        Expected::new(ty, trailing.span, label)
                    }
                });
                self.expr(trailing, hint.as_ref())
            }
        }
    }

    /// Checks every argument of a call whose callee has no signature, so an
    /// error inside one is still reported.
    fn check_args_freely(&mut self, args: &[Arg], trailing: Option<&Expr>) {
        self.check_args_abstained(args, trailing, Ty::recovery());
    }

    /// Walks every argument of a call this pass has stopped checking,
    /// against an unknown that says why.
    ///
    /// Each argument is still walked, because a mistake inside one is a
    /// mistake wherever the call went wrong. What it is *not* is walked
    /// against nothing: a place typed by an unknown the checker has already
    /// accounted for is a place with an explanation, and giving the argument
    /// that explanation is what stops one rejected call from also reporting
    /// the empty array, the bare `None`, and the unannotated lambda
    /// parameter it happened to be written with.
    ///
    /// The two callers are the two reasons a call stops being checked: an
    /// error already reported about it ([`Ty::recovery`]), and a host module
    /// no schema describes ([`Ty::dynamic_boundary`]).
    fn check_args_abstained(&mut self, args: &[Arg], trailing: Option<&Expr>, why: Ty) {
        let expected = Expected::abstained(why.clone());
        for arg in args {
            self.expr(&arg.value, Some(&expected));
        }
        if let Some(trailing) = trailing {
            self.trailing_type(trailing, Some(&why));
        }
    }

    // ----------------------------------------------------------- traits

    /// What a conformance question needs to be answered: the module's
    /// declared conformances, plus the bounds of the type parameters in
    /// scope, since a bounded parameter conforms to the traits it is bounded
    /// by.
    fn view(&self) -> ConformanceView<'_> {
        ConformanceView {
            declared: &self.conformances,
            bounds: &self.bounds,
        }
    }

    /// Checks every bound of `sig` against the types its call site chose.
    ///
    /// A bound is checked here, at the call, because that is where a type
    /// parameter is instantiated; inside the body the parameter is rigid and
    /// its bound is a fact rather than an obligation.
    fn check_bounds(
        &mut self,
        sig: &FnSig,
        subst: &BTreeMap<Arc<str>, Ty>,
        what: &str,
        span: Span,
    ) {
        for (param, bounds) in &sig.bounds {
            let Some(ty) = subst.get(param) else {
                continue;
            };
            if ty.is_wild() {
                continue;
            }
            for bound in bounds {
                if conforms(ty, &bound.name, &self.view()) {
                    continue;
                }
                // `dyn Trait` is a type, not a type parameter, so it never
                // stands in for one even when it names the very same trait.
                let (message, help) = if let Ty::Dyn(trait_name) = ty {
                    (
                        format!("`dyn {trait_name}` cannot be used as a type argument"),
                        format!(
                            "pass a concrete value that conforms to `{}`, or declare the parameter as `dyn {}` instead of `{param}`",
                            bound.name, bound.name
                        ),
                    )
                } else {
                    (
                        format!("`{ty}` does not conform to `{}`", bound.name),
                        format!("write `impl {} for {ty} {{ ... }}`", bound.name),
                    )
                };
                self.diagnostics.push(
                    Diagnostic::error(UNSATISFIED_BOUND, message)
                        .at(span)
                        .label(
                            bound.span,
                            format!("{what} requires `{param}: {}`", bound.name),
                        )
                        .rule("A type argument must conform to every trait its type parameter is bounded by, and conformance is explicit: only an `impl Trait for Type` block declares one.")
                        .help(help),
                );
            }
        }
    }

    /// Whether the trait method named `method` declares a `var self`
    /// receiver.
    fn mutating_trait_method(&self, trait_name: &str, method: &str) -> bool {
        self.trait_entry(trait_name)
            .and_then(|entry| entry.method(method))
            .and_then(|method| method.receiver)
            .is_some_and(|receiver| receiver.is_var)
    }

    /// The trait among `T`'s bounds that declares `method`, with its
    /// signature.
    fn bound_method(&self, param: &str, method: &str) -> Option<(Arc<str>, FnSig)> {
        for bound in self.bounds.get(param)? {
            if let Some(sig) = self.traits.get(&*bound.name).and_then(|m| m.get(method)) {
                return Some((bound.name.clone(), sig.clone()));
            }
        }
        None
    }

    /// A method call on a value whose type is the type parameter `param`.
    ///
    /// Resolution goes through the parameter's bounds: a parameter with no
    /// bound has no operations at all, which is the whole reason a bound is
    /// written.
    fn param_method_call(
        &mut self,
        param: &Arc<str>,
        name: &Ident,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        if let Some((trait_name, sig)) = self.bound_method(param, &name.node) {
            return self.call_signature(
                &sig,
                &format!("`{trait_name}.{}`", name.node),
                Vec::new(),
                args,
                trailing,
                span,
            );
        }
        let diagnostic = match self.bounds.get(param) {
            None => Diagnostic::error(
                UNBOUNDED_PARAMETER,
                format!("`{param}` has no bound, so it has no method `{}`", name.node),
            )
            .rule("A method call on a type parameter resolves through the parameter's bounds; an unbounded parameter's values can only be moved, not inspected.")
            .help(format!(
                "bound the parameter, as in `<{param}: SomeTrait>`, and declare `{}` in that trait",
                name.node
            )),
            Some(bounds) => {
                let names: Vec<String> = bounds.iter().map(|b| b.name.to_string()).collect();
                Diagnostic::error(
                    UNKNOWN_METHOD,
                    format!(
                        "no trait `{param}` is bounded by declares a method `{}`",
                        name.node
                    ),
                )
                .rule("A method call on a type parameter resolves through the parameter's bounds.")
                .help(format!(
                    "`{param}` is bounded by {}; declare `{}` in one of them, or add another bound",
                    list(&names),
                    name.node
                ))
            }
        };
        self.diagnostics.push(diagnostic.at(span));
        self.check_args_freely(args, trailing);
        Ty::recovery()
    }

    /// A method call on a `dyn Trait` value.
    ///
    /// Only the trait's `self`-taking methods are reachable: an associated
    /// function has no receiver to dispatch on, so a trait object cannot
    /// find an implementation for it.
    fn dyn_method_call(
        &mut self,
        trait_name: &Arc<str>,
        name: &Ident,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let sig = self
            .traits
            .get(&**trait_name)
            .and_then(|methods| methods.get(&name.node))
            .cloned();
        let Some(sig) = sig else {
            let known: Vec<String> = self
                .traits
                .get(&**trait_name)
                .map(|methods| methods.keys().cloned().collect())
                .unwrap_or_default();
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_METHOD,
                    format!("`{trait_name}` has no method `{}`", name.node),
                )
                .at(span)
                .rule("A call on a `dyn Trait` value reaches the trait's methods and nothing else: the concrete type is not known here.")
                .help(if known.is_empty() {
                    format!("`{trait_name}` declares no methods")
                } else {
                    format!("`{trait_name}` declares {}", list(&known))
                }),
            );
            self.check_args_freely(args, trailing);
            return Ty::recovery();
        };
        if sig.receiver.is_none() {
            self.diagnostics.push(
                Diagnostic::error(
                    DYN_ASSOCIATED,
                    format!(
                        "`{trait_name}.{}` takes no `self`, so it cannot be called through `dyn {trait_name}`",
                        name.node
                    ),
                )
                .at(span)
                .rule("Only a trait method whose first parameter is `self` may be called through `dyn Trait`: an associated function has no receiver to dispatch on.")
                .help(format!(
                    "call it on a concrete type, as in `SomeType.{}(...)`, or give it a `self` parameter",
                    name.node
                )),
            );
            self.check_args_freely(args, trailing);
            return Ty::recovery();
        }
        // Conversion to `dyn Trait` produces a value, exactly as assignment
        // and argument passing do, so a mutation made through the trait
        // object could not be observed by whatever the value came from.
        if self.mutating_trait_method(trait_name, &name.node) {
            self.diagnostics.push(
                Diagnostic::error(
                    DYN_MUTATING,
                    format!(
                        "`{trait_name}.{}` takes `var self`, so it cannot be called through `dyn {trait_name}`",
                        name.node
                    ),
                )
                .at(span)
                .rule("A concrete value becomes a `dyn Trait` value by conversion, and a conversion produces a value; a mutating receiver needs the caller's own place, which a converted value is not.")
                .help(format!(
                    "call `{}` on the concrete value before converting it, or declare the method with `self`",
                    name.node
                )),
            );
            self.check_args_freely(args, trailing);
            return Ty::recovery();
        }
        self.call_signature(
            &sig,
            &format!("`{trait_name}.{}`", name.node),
            Vec::new(),
            args,
            trailing,
            span,
        )
    }

    // ------------------------------------------------------------ methods

    /// Records that the call `id` reaches the method `method` declared on
    /// the type `key` names.
    ///
    /// `key` is a canonical name — bare for a type this module declares, and
    /// `module.Name` for one it meets through an import — so splitting it is
    /// what turns "the name this checker files it under" into "the
    /// declaration", which is what a consumer needs and what is the same
    /// answer read from anywhere in the package.
    ///
    /// The split is at the **last** dot, because a module's name may hold one
    /// and a type's name may not: `module_opaque.account.Account` is the type
    /// `Account` of the module `module_opaque.account`, and splitting at the
    /// first dot would file it under `module_opaque` as a type called
    /// `account.Account`, which nothing declares.
    /// Records the boundary of the declaration whose body is about to be
    /// checked.
    ///
    /// The types are `sig`'s own — the ones this checker resolved for *this*
    /// declaration and is about to check the body against — rather than a
    /// second reading of `decl`'s annotations. That is the whole point:
    /// [`crate::facts`] exists so that a consumer and the checker cannot
    /// disagree, and a re-derivation here would be exactly the disagreement
    /// it prevents.
    ///
    /// A variadic parameter is recorded as what it was written as rather
    /// than as the `Array<T>` the body sees, because a call supplies the
    /// element and the array is what the callee makes of them. Which of the
    /// two questions [`Signature::params`] answers is stated on the field
    /// itself, where a consumer reads it, rather than only here.
    ///
    /// Recording is not deciding: this is called before the walk and read by
    /// nothing during it, so no diagnostic depends on it.
    fn record_signature(&mut self, decl: &FnDecl, sig: &FnSig) {
        self.facts.record_signature(
            decl.span.file,
            decl.span,
            Signature {
                receiver: sig.receiver.clone(),
                params: sig.params.iter().map(|param| param.ty.clone()).collect(),
                ret: sig.ret.clone(),
            },
        );
    }

    /// Records the boundary of a struct declaration's initializer.
    ///
    /// `Point(x: 0.0, y: 1.0)` is a call, and the thing it calls is a
    /// signature this checker synthesizes out of the declaration's fields —
    /// which is why [`ParamSig`] documents itself as covering one. Recording
    /// it here is what publishes a struct's *resolved* field types, in
    /// declaration order, to anything downstream that holds a value to the
    /// declaration rather than reading a field out of one.
    ///
    /// The types are the declaration's own, so a generic struct's field is
    /// recorded as the `Ty::Param` it was written as; a consumer holding a
    /// use completes it with [`Ty::instantiate`]. Recording is not deciding,
    /// exactly as for [`Checker::record_signature`].
    fn record_struct_signature(&mut self, decl: &StructDecl, sig: &StructSig) {
        let ret = Ty::Struct(
            self.key(&decl.name.node).into(),
            sig.generics.iter().cloned().map(Ty::Param).collect(),
        );
        self.facts.record_signature(
            decl.span.file,
            decl.span,
            Signature {
                receiver: None,
                params: sig.fields.iter().map(|field| field.ty.clone()).collect(),
                ret,
            },
        );
    }

    /// The same for each of an enum's cases, whose payload types a case
    /// expression is checked against.
    ///
    /// One record per case rather than one per enum, because a case is what a
    /// program names and a value carries: `Verdict.Drop(reason)` is the call,
    /// and the case's own span is what a consumer holding the declaration
    /// already has to key by.
    fn record_case_signatures(&mut self, decl: &EnumDecl, sig: &EnumSig) {
        let ret = Ty::Enum(
            self.key(&decl.name.node).into(),
            sig.generics.iter().cloned().map(Ty::Param).collect(),
        );
        for (case, declared) in decl.cases.iter().zip(&sig.cases) {
            self.facts.record_signature(
                case.span.file,
                case.span,
                Signature {
                    receiver: None,
                    params: declared.payload.clone(),
                    ret: ret.clone(),
                },
            );
        }
    }

    fn record_target(&mut self, id: ExprId, file: FileId, key: &str, method: &str) {
        let (module, type_name) = match key.rsplit_once('.') {
            Some((module, name)) => (module.to_string(), name.to_string()),
            None => (self.module.name.clone(), key.to_string()),
        };
        self.facts.record_target(
            file,
            id,
            MethodTarget {
                module,
                type_name,
                method: method.to_string(),
            },
        );
    }

    fn method_call(
        &mut self,
        id: ExprId,
        receiver: &Ty,
        name: &Ident,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        match receiver {
            Ty::Unknown(_) | Ty::Never => {
                self.check_args_freely(args, trailing);
                return Ty::recovery();
            }
            Ty::Struct(type_name, type_args) | Ty::Enum(type_name, type_args) => {
                let key = (type_name.to_string(), name.node.clone());
                if let Some(sig) = self.methods.get(&key).cloned() {
                    self.record_target(id, span.file, type_name, &name.node);
                    self.check_receiver(&sig, type_name, &name.node, span, true);
                    let generics = self.declared_generics(type_name);
                    let subst = substitution(&generics, type_args);
                    let sig = FnSig {
                        generics: sig
                            .generics
                            .iter()
                            .filter(|g| !generics.contains(g))
                            .cloned()
                            .collect(),
                        params: sig
                            .params
                            .iter()
                            .map(|p| ParamSig {
                                ty: p.ty.substitute(&subst),
                                ..p.clone()
                            })
                            .collect(),
                        ret: sig.ret.substitute(&subst),
                        ..sig
                    };
                    return self.call_signature(
                        &sig,
                        &format!("`{type_name}.{}`", name.node),
                        Vec::new(),
                        args,
                        trailing,
                        span,
                    );
                }
                let known = self.known_members(type_name);
                self.diagnostics.push(
                    Diagnostic::error(
                        UNKNOWN_METHOD,
                        format!("`{type_name}` has no method `{}`", name.node),
                    )
                    .at(span)
                    .rule("A method is declared in its type's `impl` block.")
                    .help(format!("`{type_name}` declares {known}")),
                );
                self.check_args_freely(args, trailing);
                return Ty::recovery();
            }
            Ty::Param(param) => {
                let param = param.clone();
                return self.param_method_call(&param, name, args, trailing, span);
            }
            Ty::Dyn(trait_name) => {
                let trait_name = trait_name.clone();
                return self.dyn_method_call(&trait_name, name, args, trailing, span);
            }
            Ty::Host(declared) => {
                let declared = declared.clone();
                return self.host_method_call(&declared, name, args, trailing, span);
            }
            _ => {}
        }

        if let (Ty::Result(ok, error), "mapError") = (receiver, name.node.as_str()) {
            return self.map_error(ok, error, args, trailing, span);
        }

        // `Snapshot` is the one trait a closure, a task, a task scope, and a
        // synchronized value never conform to: none has an independent
        // mutable graph this side of a lock to copy. `builtin_method` would
        // otherwise report this as an ordinary unknown method, which does not
        // say why.
        if name.node == "snapshot"
            && matches!(
                receiver,
                Ty::Fn(_) | Ty::Task(_) | Ty::Scope | Ty::Shared(_)
            )
        {
            self.diagnostics
                .push(no_snapshot_conformance(receiver, span));
            self.check_args_freely(args, trailing);
            return Ty::recovery();
        }

        match builtin_method(receiver, &name.node) {
            Some(sig) => {
                let what = format!("`{}.{}`", builtin_name(receiver), name.node);
                self.call_builtin(&sig, &what, args, trailing, span)
            }
            None => {
                self.diagnostics
                    .push(unknown_builtin_method(receiver, &name.node, span));
                self.check_args_freely(args, trailing);
                Ty::recovery()
            }
        }
    }

    /// `result.mapError { ... }`, which replaces a `Result`'s failure with
    /// whatever its callback produces.
    ///
    /// The Language Card writes the callback as a trailing closure that may
    /// ignore the error it replaces, so a callback of no parameters and one
    /// of a single `E` parameter are both accepted — exactly the two forms
    /// the runtime dispatches.
    fn map_error(
        &mut self,
        ok: &Ty,
        error: &Ty,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let callback: Option<&Expr> = match (args.first(), trailing) {
            (Some(arg), None) => Some(&arg.value),
            (None, Some(trailing)) => Some(trailing),
            _ => None,
        };
        let count = args.len() + usize::from(trailing.is_some());
        if count != 1 {
            self.diagnostics.push(
                Diagnostic::error(
                    ARITY,
                    format!("`Result.mapError` takes 1 argument, but {count} were given"),
                )
                .at(span)
                .rule("`mapError` replaces a failure with the value its one callback produces.")
                .help("write `result.mapError { ... }`"),
            );
        }
        let Some(callback) = callback else {
            self.check_args_freely(args, trailing);
            return Ty::Result(Box::new(ok.clone()), Box::new(Ty::recovery()));
        };
        let takes_error =
            matches!(&callback.kind, ExprKind::Lambda { params, .. } if params.len() == 1);
        let expected = Ty::func(
            false,
            if takes_error {
                vec![error.clone()]
            } else {
                Vec::new()
            },
            // The callback's own result is what replaces the failure type,
            // so the expectation states the parameters and leaves the result
            // to the body.
            Ty::placeholder(),
        );
        let found = self.trailing_type(callback, Some(&expected));
        let replacement = match &found {
            Ty::Fn(func) => func.ret.clone(),
            _ => Ty::recovery(),
        };
        Ty::Result(Box::new(ok.clone()), Box::new(replacement))
    }

    /// Checks a call against a builtin signature, which binds no generics of
    /// its own beyond those already substituted into it.
    fn call_builtin(
        &mut self,
        sig: &BuiltinSig,
        what: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let last = sig.params.len().saturating_sub(1);
        let params: Vec<ParamSig> = sig
            .params
            .iter()
            .enumerate()
            .map(|(index, (name, ty))| ParamSig {
                name: (*name).to_string(),
                ty: ty.clone(),
                variadic: sig.variadic && index == last,
                has_default: false,
                is_var: false,
                span,
            })
            .collect();
        let subst = self.match_arguments(
            &params,
            &sig.generics,
            BTreeMap::new(),
            args,
            trailing,
            span,
            what,
            "the parameter",
        );
        self.open(&sig.ret, &sig.generics, &subst)
    }

    /// Reports a method called as an associated function, or an associated
    /// function called on a value.
    ///
    /// A method's first parameter is its receiver, written `self` or `var
    /// self`; an associated function has none. Which one a declaration is,
    /// is part of its type.
    fn check_receiver(
        &mut self,
        sig: &FnSig,
        type_name: &str,
        name: &str,
        span: Span,
        given: bool,
    ) {
        match (sig.receiver.is_some(), given) {
            (true, false) => self.diagnostics.push(
                Diagnostic::error(
                    RECEIVER,
                    format!("`{type_name}.{name}` is a method and needs a receiver"),
                )
                .at(span)
                .rule("A method is called on a value; only an associated function is called on its type.")
                .help(format!(
                    "call it on a value, as in `value.{name}(...)`, or declare `fn {name}()` without `self`"
                )),
            ),
            (false, true) => self.diagnostics.push(
                Diagnostic::error(
                    RECEIVER,
                    format!("`{type_name}.{name}` takes no receiver"),
                )
                .at(span)
                .rule("An associated function is called on its type; only a method is called on a value.")
                .help(format!("write `{type_name}.{name}(...)`")),
            ),
            _ => {}
        }
    }

    /// The type parameters a struct or enum declares.
    fn declared_generics(&self, name: &str) -> Vec<Arc<str>> {
        if let Some(sig) = self.structs.get(name) {
            return sig.generics.clone();
        }
        if let Some(sig) = self.enums.get(name) {
            return sig.generics.clone();
        }
        Vec::new()
    }

    /// The methods and cases a diagnostic can suggest for `type_name`.
    fn known_members(&self, type_name: &str) -> String {
        let mut names: Vec<String> = self
            .methods
            .keys()
            .filter(|(owner, _)| owner == type_name)
            .map(|(_, name)| name.clone())
            .collect();
        if let Some(sig) = self.enums.get(type_name) {
            names.extend(sig.cases.iter().map(|c| c.name.clone()));
        }
        if names.is_empty() {
            format!("no methods; declare one in `impl {type_name}`")
        } else {
            list(&names)
        }
    }
}

// ------------------------------------------------------------- entry shape

/// Checks every `[run.<name>]` entry against the shape the host boundary
/// calls: no parameters or one `Array<String>` of process arguments, and a
/// value the host can report.
fn check_entries(
    package: &Package,
    checked: &BTreeMap<&str, Checker<'_>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for run in package.config.runs.values() {
        let Some((module_name, entry)) = run.entry_parts() else {
            continue;
        };
        let Some(checker) = checked.get(module_name) else {
            continue;
        };
        let Some(sig) = checker.functions.get(entry) else {
            continue;
        };
        let Some(function) = checker.module.functions.get(entry) else {
            continue;
        };
        let name_span = function.decl.name.span;

        if sig.params.len() > 1 {
            diagnostics.push(
                Diagnostic::error(
                    ENTRY,
                    format!(
                        "entry `{}` declares {} parameters",
                        run.entry,
                        sig.params.len()
                    ),
                )
                .at(name_span)
                .rule("An entry function takes either no parameters or one `Array<String>` of process arguments.")
                .help(format!(
                    "write `fn {entry}()` or `fn {entry}(args: Array<String>)`"
                )),
            );
        } else if let Some(param) = sig.params.first() {
            let expected = Ty::Array(Box::new(Ty::Str));
            if !param.ty.matches(&expected) {
                diagnostics.push(
                    Diagnostic::error(
                        ENTRY,
                        format!(
                            "entry `{}` takes `{}`, but the host passes `Array<String>`",
                            run.entry, param.ty
                        ),
                    )
                    .at(param.span)
                    .rule("An entry function's one parameter is the process arguments, an `Array<String>`.")
                    .help(format!("write `fn {entry}(args: Array<String>)`")),
                );
            }
        }

        if !matches!(sig.ret, Ty::Unit | Ty::Result(_, _) | Ty::Unknown(_)) {
            diagnostics.push(
                Diagnostic::error(
                    ENTRY,
                    format!(
                        "entry `{}` returns `{}`, which the host cannot report",
                        run.entry, sig.ret
                    ),
                )
                .at(sig.ret_span)
                .rule("The host reports an entry's failure through its `Err`, so an entry returns `()` or a `Result`.")
                .help(format!(
                    "write `fn {entry}(...) -> Result<{}, Error>`",
                    sig.ret
                )),
            );
        }
    }
}

// -------------------------------------------------------------- unification

/// Unifies a parameter type with an argument type, binding the callee's own
/// type parameters in `subst`.
///
/// This is the whole of ADR 0004's "unify at the call site, substitute into
/// the signature": a type parameter binds to the first type it meets and
/// must match every later one, and nothing else is inferred.
fn unify(
    param: &Ty,
    arg: &Ty,
    generics: &BTreeSet<Arc<str>>,
    subst: &mut BTreeMap<Arc<str>, Ty>,
    view: &ConformanceView<'_>,
) -> bool {
    if coerces(arg, param, view) {
        return true;
    }
    if let Ty::Param(name) = param {
        if generics.contains(name) {
            return match subst.get(name) {
                Some(bound) => bound.matches(arg),
                None => {
                    if !arg.is_wild() {
                        subst.insert(name.clone(), arg.clone());
                    }
                    true
                }
            };
        }
    }
    if param.is_wild() || arg.is_wild() {
        return true;
    }
    match (param, arg) {
        (Ty::Array(a), Ty::Array(b))
        | (Ty::Vector(a), Ty::Vector(b))
        | (Ty::Set(a), Ty::Set(b))
        | (Ty::Option(a), Ty::Option(b))
        | (Ty::Task(a), Ty::Task(b))
        | (Ty::Shared(a), Ty::Shared(b)) => unify(a, b, generics, subst, view),
        (Ty::Map(ak, av), Ty::Map(bk, bv))
        | (Ty::MapEntry(ak, av), Ty::MapEntry(bk, bv))
        | (Ty::Result(ak, av), Ty::Result(bk, bv)) => {
            unify(ak, bk, generics, subst, view) && unify(av, bv, generics, subst, view)
        }
        (Ty::Struct(a, aargs), Ty::Struct(b, bargs)) | (Ty::Enum(a, aargs), Ty::Enum(b, bargs)) => {
            a == b
                && aargs.len() == bargs.len()
                && aargs
                    .iter()
                    .zip(bargs)
                    .all(|(a, b)| unify(a, b, generics, subst, view))
        }
        (Ty::Fn(a), Ty::Fn(b)) => {
            a.is_async == b.is_async
                && a.params.len() == b.params.len()
                && a.params
                    .iter()
                    .zip(&b.params)
                    .all(|(a, b)| unify(a, b, generics, subst, view))
                && unify(&a.ret, &b.ret, generics, subst, view)
        }
        (param, arg) => param.matches(arg),
    }
}

/// Everything needed to answer "does this type conform to this trait?".
///
/// Conformance is explicit, so `declared` is the complete set of `(trait,
/// type)` pairs the module has an `impl Trait for Type` block for. `bounds`
/// adds the type parameters currently in scope, which conform to whatever
/// they are bounded by.
struct ConformanceView<'c> {
    declared: &'c BTreeSet<(String, String)>,
    bounds: &'c BTreeMap<Arc<str>, Vec<TraitBound>>,
}

/// Whether `ty` conforms to the trait named `trait_name`.
///
/// `Unknown` and `Never` conform to everything, for the same reason they
/// match everything: the checker has abstained and must not turn its own
/// silence into an error.
fn conforms(ty: &Ty, trait_name: &str, view: &ConformanceView<'_>) -> bool {
    match ty {
        Ty::Unknown(_) | Ty::Never => true,
        Ty::Struct(name, _) | Ty::Enum(name, _) => view
            .declared
            .contains(&(trait_name.to_string(), name.to_string())),
        // A type parameter conforms to exactly the traits it is bounded by,
        // which is what lets one bounded function call another.
        Ty::Param(name) => view
            .bounds
            .get(name)
            .is_some_and(|bounds| bounds.iter().any(|b| &*b.name == trait_name)),
        // A `dyn Trait` value is not a type parameter and never stands in for
        // one, so it satisfies no bound — not even its own trait's.
        _ => false,
    }
}

/// Whether a value of type `found` may be used where `expected` is written.
///
/// This is the language's only implicit conversion, and it is deliberately
/// narrow: a concrete value becomes a `dyn Trait` value when it conforms to
/// that trait, and nothing else converts. In particular the conversion does
/// not run in reverse (a `dyn Trait` is not a concrete type), does not chain
/// through another trait, and does not reach inside a generic argument —
/// `Array<Booking>` is not an `Array<dyn Display>`, because `Array` is
/// invariant like every other generic type. Writing `[booking, receipt]`
/// where an `Array<dyn Display>` is expected does work, because each element
/// is checked against `dyn Display` on its own.
fn coerces(found: &Ty, expected: &Ty, view: &ConformanceView<'_>) -> bool {
    let Ty::Dyn(trait_name) = expected else {
        return false;
    };
    !matches!(found, Ty::Dyn(_)) && conforms(found, trait_name, view)
}

/// A name written after `dyn` or after `:` in a bound that names no trait.
fn unknown_trait(name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(UNKNOWN_TRAIT, format!("`{name}` is not a trait"))
        .at(span)
        .rule("`dyn` and a type parameter's bound both name a trait the module declares; there are no module-to-module imports yet.")
        .help(format!(
            "declare `trait {name} {{ ... }}` in this module, or name a trait that exists"
        ))
}

/// How a conformance's method differs from the signature its trait declares,
/// or `None` when the two agree.
fn signature_difference(declared: &FnSig, found: &FnSig) -> Option<String> {
    if declared.receiver.is_some() != found.receiver.is_some() {
        return Some(if declared.receiver.is_some() {
            "it takes no `self`".to_string()
        } else {
            "it takes a `self` the trait does not declare".to_string()
        });
    }
    if declared.is_async != found.is_async {
        return Some(if declared.is_async {
            "it is not `async`".to_string()
        } else {
            "it is `async`".to_string()
        });
    }
    if declared.params.len() != found.params.len() {
        return Some(format!(
            "it takes {} parameter(s), not {}",
            found.params.len(),
            declared.params.len()
        ));
    }
    for (want, got) in declared.params.iter().zip(&found.params) {
        if want.name != got.name {
            return Some(format!(
                "its parameter `{}` is named `{}` in the trait",
                got.name, want.name
            ));
        }
        if !want.ty.matches(&got.ty) {
            return Some(format!(
                "its parameter `{}` is `{}`, not `{}`",
                got.name, got.ty, want.ty
            ));
        }
    }
    if !declared.ret.matches(&found.ret) {
        return Some(format!(
            "it returns `{}`, not `{}`",
            found.ret, declared.ret
        ));
    }
    None
}

/// A trait method's signature, written the way it would be declared.
fn trait_signature(sig: &FnSig, name: &str) -> String {
    let mut out = String::new();
    if sig.is_async {
        out.push_str("async ");
    }
    out.push_str("fn ");
    out.push_str(name);
    out.push('(');
    let mut entries: Vec<String> = Vec::new();
    if sig.receiver.is_some() {
        entries.push("self".to_string());
    }
    entries.extend(
        sig.params
            .iter()
            .map(|param| format!("{}: {}", param.name, param.ty)),
    );
    out.push_str(&entries.join(", "));
    out.push(')');
    if sig.ret != Ty::Unit {
        out.push_str(&format!(" -> {}", sig.ret));
    }
    out
}

/// Pairs a declaration's type parameters with the arguments a use of it was
/// written with.
///
/// A short argument list means the arity was already reported, so the
/// padding is a recovery unknown: the diagnostic exists and this stands
/// where the argument the program did not write would have.
fn substitution(generics: &[Arc<str>], args: &[Ty]) -> BTreeMap<Arc<str>, Ty> {
    generics
        .iter()
        .cloned()
        .zip(
            args.iter()
                .cloned()
                .chain(std::iter::repeat(Ty::recovery())),
        )
        .collect()
}

/// Substitutes the arguments a type alias was written with into the type it
/// expands to.
fn expand_alias(generics: Vec<Arc<str>>, ty: Ty, arguments: Vec<Ty>) -> Ty {
    let subst = generics
        .into_iter()
        .zip(
            fit(arguments, 0)
                .into_iter()
                .chain(std::iter::repeat(Ty::recovery())),
        )
        .collect();
    ty.substitute(&subst)
}

/// Truncates or pads `args` to `arity`, so a type written with the wrong
/// number of arguments still has a shape the rest of the pass can use.
fn fit(mut args: Vec<Ty>, arity: usize) -> Vec<Ty> {
    args.truncate(arity);
    while args.len() < arity {
        args.push(Ty::recovery());
    }
    args
}

// ----------------------------------------------------------------- builtins

/// A builtin method's or associated function's signature.
struct BuiltinSig {
    /// Type parameters this signature binds, unified at the call site just
    /// like a declared function's.
    generics: Vec<Arc<str>>,
    params: Vec<(&'static str, Ty)>,
    /// Whether the last parameter takes the rest of the arguments, as
    /// `Vector.of(items: T...)` does.
    variadic: bool,
    ret: Ty,
}

// ---------------------------------------------------------- the host schema

/// The Language Card sentence a Host API diagnostic quotes.
///
/// The schema is one description and both ends read it, so both ends say the
/// same thing about a call that does not fit: this is the compiler's wording
/// of the rule `cove_runtime::host` states at the boundary.
const HOST_SCHEMA_RULE: &str = "A Host API operation's argument, result, and error types come from its schema, which the compiler, the runtime, and the CLI all read.";

/// The type a Host API schema's type is, here.
///
/// The two vocabularies are the same one written twice, so the translation is
/// mechanical except at the ends. [`cove_schema::HostType::Any`] becomes
/// [`Ty::unconstrained`]: an operation that declares `Any` is one whose
/// meaning does not depend on which value it was given — the work
/// `clock.timeout` bounds — and *nothing here depends on a type* is exactly
/// what that unknown means. `Checker::host_result` says so at the call when
/// it is a result rather than a parameter. [`cove_schema::HostType::Named`] becomes
/// [`Ty::Host`], which is nominal and compared by the name the schema wrote.
fn host_ty(declared: &HostType) -> Ty {
    match declared {
        HostType::Unit => Ty::Unit,
        HostType::Bool => Ty::Bool,
        HostType::Int => Ty::Int,
        HostType::String => Ty::Str,
        HostType::Duration => Ty::Duration,
        HostType::Error => Ty::Error,
        HostType::Array(item) => Ty::Array(Box::new(host_ty(item))),
        HostType::Set(item) => Ty::Set(Box::new(host_ty(item))),
        HostType::Map(key, value) => Ty::Map(Box::new(host_ty(key)), Box::new(host_ty(value))),
        HostType::Option(some) => Ty::Option(Box::new(host_ty(some))),
        HostType::Result(ok, error) => Ty::Result(Box::new(host_ty(ok)), Box::new(host_ty(error))),
        HostType::Named(name) => Ty::Host((*name).into()),
        HostType::Any => Ty::unconstrained(),
    }
}

/// The dotted name a place is written with in source, for a diagnostic.
///
/// The same rendering `Interpreter::describe_place` produces, because a
/// place refused here is a place that expression would have been refused as
/// there, and the words a reader has seen for years are the words to keep.
/// Anything that is not a name or a field of one renders as `this
/// expression`, which is how a receiver that is no place at all reads in a
/// sentence.
fn place_text(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Ident(name) => name.clone(),
        ExprKind::Field { base, name } => format!("{}.{}", place_text(base), name.node),
        _ => "this expression".to_string(),
    }
}

/// The signature the schema declares, qualified the way the call was named,
/// which is word for word what the boundary's own diagnostic quotes.
fn declared_signature(shown: &str, operation: &OperationSchema) -> String {
    let owner = match shown.rsplit_once('.') {
        Some((owner, _)) => owner,
        None => shown,
    };
    format!(
        "the Host API schema declares `{owner}.{}`",
        operation.signature()
    )
}

/// The names of the operations in one table, for a diagnostic that has to
/// list what does exist.
fn operation_names(operations: &'static [OperationSchema]) -> Vec<String> {
    operations
        .iter()
        .map(|entry| entry.name.to_string())
        .collect()
}

/// What a name that is not a value is instead.
///
/// This is an enum rather than the sentence it prints because the sentence
/// and the correction have to agree: a module is reached *into* and a type is
/// made *of*, and choosing between those by comparing the printed words is
/// how one of them came to offer a correction the language does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Namespace {
    /// A struct, whether this module declares it or `use` reaches it: a
    /// value of one is *constructed*.
    Struct,
    /// An enum, likewise: a value of one is one of its *cases*.
    Enum,
    /// `Vector`, `Map`, `MapEntry`: a type the language defines, whose values
    /// come from the associated functions it declares.
    BuiltinType,
    /// A type a host module declares, such as `http.Request`.
    HostType,
    /// A type whose shape the site reporting it does not have to hand.
    Type,
    /// A host module named by `use`, such as `console`.
    HostModule,
    /// Another module of the package, imported whole.
    Module,
}

impl Namespace {
    /// How the diagnostic names it: `` `Vector` is a builtin type ``.
    fn what(self) -> &'static str {
        match self {
            Namespace::Struct => "a struct",
            Namespace::Enum => "an enum",
            Namespace::BuiltinType => "a builtin type",
            Namespace::HostType => "a host type",
            Namespace::Type => "a type",
            Namespace::HostModule => "a host module",
            Namespace::Module => "a module",
        }
    }

    /// The correction, which has to name a form the language actually has.
    ///
    /// This is the whole reason the kind is an enum. Choosing between these
    /// by comparing the printed noun is how one of them came to offer
    /// `console.println.<name>(...)`, which is not a Cove form at all.
    fn correction(self, name: &str) -> String {
        match self {
            Namespace::Struct => {
                format!("construct one, as in `{name}(field: value)`, or name a value instead")
            }
            Namespace::Enum => {
                format!("name one of its cases, as in `{name}.<case>`, or name a value instead")
            }
            Namespace::BuiltinType | Namespace::Type => format!(
                "call an associated function of it, as in `{name}.<name>(...)`, or name a value instead"
            ),
            Namespace::HostType => format!(
                "construct one, as in `{name}(field: value)`, or call the operation that answers one"
            ),
            Namespace::HostModule | Namespace::Module => {
                format!("name something in it, as in `{name}.<name>(...)`, or name a value instead")
            }
        }
    }
}

/// A type or a module written where a value belongs.
///
/// Each of these is a name a value can be reached *through*, and the forms
/// that reach one — `Vector.of(1)`, `console.println("x")`, `Booking(id: 1)`
/// — say so at the call. Bare, it is not a value and has no type, which used
/// to be an unknown and is now a mistake with a name.
///
/// A host *operation* is deliberately not here: it is a value, typed from its
/// schema by `Checker::host_operation_value`.
fn not_a_value(name: &str, what: Namespace, span: Span) -> Diagnostic {
    let help = what.correction(name);
    Diagnostic::error(
        NOT_A_VALUE,
        format!("`{name}` is {}, not a value", what.what()),
    )
    .at(span)
    .rule("A value is a literal, a binding, a call, or a constructed struct or enum case. A type and a module are names other forms read; neither is a value on its own.")
    .help(help)
}

/// Nothing written anywhere settles a type the checker was asked to infer.
///
/// This is the language gap the checker can neither fill nor excuse: the
/// program itself is missing the annotation, and every operation that
/// depends on the missing type is unchecked from here on. It is a warning
/// rather than an error because the value is still usable and the operations
/// that do not depend on the type are still checked — and because the
/// correction is always available, which is what `help` says.
fn unconstrained(message: String, help: String, span: Span) -> Diagnostic {
    Diagnostic::warning(UNCONSTRAINED, message)
        .at(span)
        .rule("A type the checker infers is inferred from something written: a value, an annotation, or the type of the place the value is given to.")
        .help(help)
}

/// Whether a declared host type is, or contains, [`HostType::Any`].
fn contains_any(declared: &HostType) -> bool {
    match declared {
        HostType::Any => true,
        HostType::Array(inner) | HostType::Set(inner) | HostType::Option(inner) => {
            contains_any(inner)
        }
        HostType::Map(key, value) | HostType::Result(key, value) => {
            contains_any(key) || contains_any(value)
        }
        _ => false,
    }
}

/// A type reached through a host module no schema describes.
///
/// This is the warning that used to greet every host type. It is now what
/// greets only the ones the checker genuinely cannot answer for: an
/// embedding may register any module it likes, and one whose schema the
/// compiler was not handed is checked by the boundary at run time and by
/// nothing before it.
fn unchecked_host_type(shown: &str, span: Span) -> Diagnostic {
    Diagnostic::warning(
        HOST_TYPE,
        format!("`{shown}` comes from a host module no Host API schema describes, so values of it are unchecked"),
    )
    .at(span)
    .rule("A Host API's types come from its schema; the checker reads the shipped schemas and any an embedder supplies.")
    .help("the checker treats this type as unknown; every operation on it is left to the runtime, which holds the host to the schema it registered with")
}

/// The builtin type `receiver` is one of, when it is one.
///
/// `MapEntry` and `Error` are here for their *fields* rather than their
/// methods: both are builtin structs that answer no methods at all, and what
/// the table says about them is what they carry.
fn builtin_schema_of(receiver: &Ty) -> Option<&'static BuiltinSchema> {
    let name = match receiver {
        Ty::Unit => "Unit",
        Ty::Bool => "Bool",
        Ty::Int => "Int",
        Ty::Float => "Float",
        Ty::Str => "String",
        Ty::Duration => "Duration",
        Ty::Error => "Error",
        Ty::Range => "Range",
        Ty::Array(_) => "Array",
        Ty::Vector(_) => "Vector",
        Ty::Map(_, _) => "Map",
        Ty::MapEntry(_, _) => "MapEntry",
        Ty::Set(_) => "Set",
        Ty::Option(_) => "Option",
        Ty::Result(_, _) => "Result",
        Ty::Task(_) => "Task",
        Ty::Shared(_) => "Shared",
        Ty::Scope => "Scope",
        _ => return None,
    };
    cove_schema::builtin(name)
}

/// The type arguments `receiver` was written with, in the order the schema
/// declares its parameters.
///
/// This is what binds the `T` of `Array<T>.get` to the element type of the
/// array the call was made on.
fn receiver_arguments(receiver: &Ty) -> Vec<Ty> {
    match receiver {
        Ty::Array(item)
        | Ty::Vector(item)
        | Ty::Set(item)
        | Ty::Option(item)
        | Ty::Task(item)
        | Ty::Shared(item) => vec![(**item).clone()],
        Ty::Map(left, right) | Ty::MapEntry(left, right) | Ty::Result(left, right) => {
            vec![(**left).clone(), (**right).clone()]
        }
        _ => Vec::new(),
    }
}

/// The type a builtin schema's type is, here.
///
/// The scalars translate the way [`host_ty`] translates a host's, and the
/// three variants the host vocabulary does not have are the whole reason
/// there are two vocabularies. [`BuiltinType::Param`] is a name the receiver
/// binds -- read off `bound`, so `Array<Int>.get` answers `Option<Int>` -- or,
/// when the signature binds it itself, a [`Ty::Param`] for the call site to
/// unify like any other. [`BuiltinType::SelfType`] is the receiver, which is
/// what `snapshot` answers. [`BuiltinType::Fn`] is an ordinary function type,
/// since a builtin that takes a callback takes an ordinary closure.
fn builtin_ty(declared: &BuiltinType, bound: &BTreeMap<&str, Ty>, receiver: Option<&Ty>) -> Ty {
    let nested = |inner: &BuiltinType| Box::new(builtin_ty(inner, bound, receiver));
    match declared {
        BuiltinType::Unit => Ty::Unit,
        BuiltinType::Bool => Ty::Bool,
        BuiltinType::Int => Ty::Int,
        BuiltinType::Float => Ty::Float,
        BuiltinType::String => Ty::Str,
        BuiltinType::Error => Ty::Error,
        BuiltinType::Duration => Ty::Duration,
        BuiltinType::Array(item) => Ty::Array(nested(item)),
        BuiltinType::Vector(item) => Ty::Vector(nested(item)),
        BuiltinType::Set(item) => Ty::Set(nested(item)),
        BuiltinType::Map(key, value) => Ty::Map(nested(key), nested(value)),
        BuiltinType::MapEntry(key, value) => Ty::MapEntry(nested(key), nested(value)),
        BuiltinType::Option(some) => Ty::Option(nested(some)),
        BuiltinType::Result(ok, error) => Ty::Result(nested(ok), nested(error)),
        BuiltinType::Task(inner) => Ty::Task(nested(inner)),
        BuiltinType::Shared(inner) => Ty::Shared(nested(inner)),
        BuiltinType::Fn(params, ret) => Ty::func(
            false,
            params
                .iter()
                .map(|param| builtin_ty(param, bound, receiver))
                .collect(),
            builtin_ty(ret, bound, receiver),
        ),
        BuiltinType::Param(name) => bound
            .get(name)
            .cloned()
            .unwrap_or_else(|| Ty::Param((*name).into())),
        // Only an associated function is opened with no receiver, and the
        // schema's own tests hold one to naming no `Self`.
        BuiltinType::SelfType => receiver.cloned().unwrap_or(Ty::placeholder()),
    }
}

/// The free builtin `name` describes itself with, when it is one of `kind`.
///
/// The kind is asked for because the two are reached separately: an assertion
/// is dispatched through the path that carries its arguments' source text,
/// and a constructor through the one that reads the type the call site
/// expects.
fn free_builtin(name: &str, kind: FreeBuiltinKind) -> Option<&'static FreeBuiltinSchema> {
    cove_schema::free_builtin(name).filter(|schema| schema.kind == kind)
}

/// What a free builtin's call has settled its type parameters to, and where.
///
/// A free builtin binds every parameter it names itself — there is no
/// receiver to read one off — and exactly two things can settle one: the type
/// the call site expects, and an argument. An unsettled parameter is
/// [`Ty::unconstrained`] rather than a [`Ty::Param`], because nothing declared
/// it for a call site to instantiate: `Ok(1)` written where nothing expects
/// a `Result` genuinely does not know what its error type is, and what
/// settles it is the place the value is given to.
///
/// It is an *unconstrained* unknown and not a placeholder because it does
/// escape: `Ok(1)` in a place that expects nothing produces a
/// `Result<Int, _>` the program goes on holding. That is one of the two
/// admitted holes in what a clean check guarantees, named in the module
/// documentation above.
///
/// The spans are here because a diagnostic points at whatever settled the
/// parameter it is complaining about: `assertEqual("a", 1)` says the `1`
/// should have been a `String` and points at the `"a"` that decided so.
struct FreeBindings {
    types: BTreeMap<&'static str, Ty>,
    origins: BTreeMap<&'static str, Span>,
}

impl FreeBindings {
    /// Every parameter `schema` binds, so far settled by nothing.
    fn new(schema: &'static FreeBuiltinSchema) -> FreeBindings {
        FreeBindings {
            types: schema
                .generics
                .iter()
                .map(|name| (*name, Ty::unconstrained()))
                .collect(),
            origins: BTreeMap::new(),
        }
    }

    /// `declared`, with every parameter settled so far substituted in.
    fn open(&self, declared: &BuiltinType) -> Ty {
        builtin_ty(declared, &self.types, None)
    }

    /// Settles the parameter `declared` names, when it names one bare.
    ///
    /// A parameter mentioned inside a larger type is not settled by an
    /// argument, because no free builtin declares one that way and reading a
    /// type back out of a value's type is unification the checker does not do
    /// here.
    fn bind(&mut self, declared: &BuiltinType, ty: Ty, at: Span) {
        if let BuiltinType::Param(name) = declared {
            self.types.insert(name, ty);
            self.origins.insert(name, at);
        }
    }

    /// Reads the parameters of `declared` off `actual`, as far as the two
    /// have the same shape.
    ///
    /// This is how the type a call site expects reaches a constructor's
    /// payload: `Result<T, E>` against `Result<Int, Error>` settles both, and
    /// `Result<T, E>` against something that is not a `Result` settles
    /// neither, leaving the argument to say what it can.
    fn read_off(&mut self, declared: &BuiltinType, actual: &Ty, at: Span) {
        match (declared, actual) {
            (BuiltinType::Param(_), _) => self.bind(declared, actual.clone(), at),
            (BuiltinType::Array(inner), Ty::Array(ty))
            | (BuiltinType::Vector(inner), Ty::Vector(ty))
            | (BuiltinType::Set(inner), Ty::Set(ty))
            | (BuiltinType::Option(inner), Ty::Option(ty))
            | (BuiltinType::Task(inner), Ty::Task(ty))
            | (BuiltinType::Shared(inner), Ty::Shared(ty)) => self.read_off(inner, ty, at),
            (BuiltinType::Map(key, value), Ty::Map(left, right))
            | (BuiltinType::MapEntry(key, value), Ty::MapEntry(left, right))
            | (BuiltinType::Result(key, value), Ty::Result(left, right)) => {
                self.read_off(key, left, at);
                self.read_off(value, right, at);
            }
            _ => {}
        }
    }

    /// Where the parameter `declared` names was settled, or `fallback` when
    /// it was settled by the call site rather than by an argument.
    fn origin(&self, declared: &BuiltinType, fallback: Span) -> Span {
        match declared {
            BuiltinType::Param(name) => self.origins.get(name).copied().unwrap_or(fallback),
            _ => fallback,
        }
    }
}

/// A free builtin was given the wrong number of arguments.
///
/// The rule and the correction are the caller's, because they are what the
/// two kinds do not share: a constructor carries a value and an assertion
/// checks one.
fn free_arity(schema: &FreeBuiltinSchema, found: usize, span: Span) -> Diagnostic {
    Diagnostic::error(
        ARITY,
        match schema.kind {
            FreeBuiltinKind::Constructor => format!(
                "`{}` takes {} argument, but {found} were given",
                schema.name,
                schema.arity()
            ),
            FreeBuiltinKind::Assertion => format!(
                "`{}` takes {} argument(s), but {found} were given",
                schema.name,
                schema.arity()
            ),
        },
    )
    .at(span)
}

/// What a diagnostic says a free builtin's parameter is for.
///
/// The sentence is the checker's rather than the table's: the table says what
/// a call takes, and this says why, which is prose no other crate reads.
fn free_builtin_reason(schema: &FreeBuiltinSchema, param: &ParamSchema, ty: &Ty) -> String {
    match schema.kind {
        FreeBuiltinKind::Constructor => format!("`{}` carries a `{ty}`", schema.name),
        // An assertion either checks one value against a type it declares or
        // compares a pair against each other, and its one parameter or its
        // two are what say which.
        FreeBuiltinKind::Assertion if schema.arity() == 1 => {
            format!("`{}` checks a `{ty}` {}", schema.name, param.name)
        }
        FreeBuiltinKind::Assertion => {
            format!("`{}` compares two values of one type", schema.name)
        }
    }
}

/// One schema signature, opened for the receiver it was reached through.
///
/// `parameters` and `arguments` are the receiver's, paired positionally; an
/// associated function is called on the type and passes both empty.
fn builtin_sig(
    method: &'static MethodSchema,
    parameters: &'static [&'static str],
    arguments: &[Ty],
    receiver: Option<&Ty>,
) -> BuiltinSig {
    let bound: BTreeMap<&str, Ty> = parameters
        .iter()
        .copied()
        .zip(arguments.iter().cloned())
        .collect();
    BuiltinSig {
        generics: method
            .generics
            .iter()
            .map(|generic| Arc::from(*generic))
            .collect(),
        params: method
            .params
            .iter()
            .map(|param| (param.name, builtin_ty(&param.ty, &bound, receiver)))
            .collect(),
        variadic: method.variadic,
        ret: builtin_ty(&method.result, &bound, receiver),
    }
}

/// What a builtin's own type parameters stand for in `receiver`.
///
/// `Map<String, Int>` binds `K` to `String` and `V` to `Int`, which is what
/// opens a method's signature, a case's payload, and a field's type alike.
fn receiver_binding<'a>(schema: &'a BuiltinSchema, receiver: &Ty) -> BTreeMap<&'a str, Ty> {
    schema
        .parameters
        .iter()
        .copied()
        .zip(receiver_arguments(receiver))
        .collect()
}

/// The types a builtin enum's case binds, read off the scrutinee, or `None`
/// when the enum declares no such case.
///
/// This is the same opening [`builtin_sig`] does, with a case's payload where
/// a signature's parameters would be: `Ok` carries the `T` of the `Result<T,
/// E>` the `match` is over, so one description of what `Ok` carries serves
/// both the pattern's binding here and the value the interpreter builds.
fn builtin_case_payload(scrutinee: &Ty, case: &str) -> Option<Vec<Ty>> {
    let schema = builtin_schema_of(scrutinee)?;
    let case = schema.case(case)?;
    let bound = receiver_binding(schema, scrutinee);
    Some(
        case.payload
            .iter()
            .map(|ty| builtin_ty(ty, &bound, Some(scrutinee)))
            .collect(),
    )
}

/// The signature of a builtin method, or `None` when the receiver has no such
/// method.
///
/// The table is [`cove_schema::builtins`], which the runtime dispatches out of
/// as well: there is one list of what a builtin type answers to, and this is
/// the compiler reading it.
fn builtin_method(receiver: &Ty, name: &str) -> Option<BuiltinSig> {
    let schema = builtin_schema_of(receiver)?;
    let method = schema.method(name)?;
    Some(builtin_sig(
        method,
        schema.parameters,
        &receiver_arguments(receiver),
        Some(receiver),
    ))
}

/// Every associated function the builtin types declare, qualified, as a
/// diagnostic reads them out.
fn builtin_associated_functions() -> String {
    let names: Vec<String> = cove_schema::builtins::builtins()
        .iter()
        .flat_map(|entry| {
            entry
                .associated
                .iter()
                .map(|method| format!("`{}.{}`", entry.name, method.name))
        })
        .collect();
    match names.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{}, and {last}", rest.join(", ")),
        None => "nothing".to_string(),
    }
}

/// The name a builtin type answers to in a diagnostic.
fn builtin_name(ty: &Ty) -> String {
    match ty {
        Ty::Array(_) => "Array".to_string(),
        Ty::Vector(_) => "Vector".to_string(),
        Ty::Option(_) => "Option".to_string(),
        Ty::Result(_, _) => "Result".to_string(),
        Ty::Task(_) => "Task".to_string(),
        Ty::Shared(_) => "Shared".to_string(),
        Ty::Map(_, _) => "Map".to_string(),
        Ty::Set(_) => "Set".to_string(),
        Ty::MapEntry(_, _) => "MapEntry".to_string(),
        other => other.to_string(),
    }
}

/// `value.snapshot()` on a type ADR 0001 excludes by name: a closure, a
/// task, or a task scope.
fn no_snapshot_conformance(receiver: &Ty, span: Span) -> Diagnostic {
    let what = match receiver {
        Ty::Fn(_) => "closures",
        Ty::Task(_) => "tasks",
        Ty::Shared(_) => "synchronized values",
        _ => "task scopes",
    };
    Diagnostic::error(
        UNKNOWN_METHOD,
        format!("`{receiver}` does not implement `Snapshot`"),
    )
    .at(span)
    .rule(format!(
        "Closures, synchronized values, and Host resources do not implement `Snapshot` by default; {what} have no independent mutable graph to copy."
    ))
    .help("a struct or enum conforms explicitly with `impl Snapshot for Type`")
}

fn unknown_builtin_method(receiver: &Ty, name: &str, span: Span) -> Diagnostic {
    let type_name = builtin_name(receiver);
    // Who is taught the spelling is derived rather than listed: a receiver
    // that declares `length` is a receiver a program might have written
    // `count()` on. The two ends used to keep a list each and had drifted by
    // two types, so a `Map` was taught the spelling at run time and told
    // nothing by `cove check`.
    if name == "count" && cove_schema::builtins::declares_length(&type_name) {
        return Diagnostic::error(
            UNKNOWN_METHOD,
            format!("`{type_name}` has no method `count`; Cove spells the number of elements `length()`"),
        )
        .at(span)
        .rule("Every sequence reports its element count as `length()`; there is no `count()`.")
        .help("write `length()` instead of `count()`");
    }
    let known = builtin_methods_of(receiver);
    Diagnostic::error(
        UNKNOWN_METHOD,
        format!("`{type_name}` has no method `{name}`"),
    )
    .at(span)
    .rule("A builtin type's methods are exactly the ones the language defines.")
    .help(if known.is_empty() {
        format!("`{type_name}` has no methods")
    } else {
        format!("`{type_name}` has {}", list(&known))
    })
}

/// Every method name a builtin receiver answers to, for a diagnostic's help.
///
/// This used to be a hand-written list of candidate names filtered through
/// [`builtin_method`], which is a third copy of the table and had drifted
/// like the other two: it had never gained `mapError`, `cancel`, `lock`, or
/// `spawn`, so a `Result` whose method was misspelled was told it had two
/// methods when it has three. Reading the schema's own order removes both the
/// copy and the omission.
fn builtin_methods_of(receiver: &Ty) -> Vec<String> {
    builtin_schema_of(receiver)
        .map(|schema| {
            schema
                .methods
                .iter()
                .map(|method| method.name.to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------- prose

fn operator_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::Eq => "==",
        BinaryOp::Ne => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::Is => "is",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

fn starts_uppercase(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn join_path(path: &[Ident]) -> String {
    path.iter()
        .map(|segment| segment.node.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        return "nothing".to_string();
    }
    items
        .iter()
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn first_case_of(sig: Option<&EnumSig>) -> String {
    sig.and_then(|sig| sig.cases.first())
        .map(|case| case.name.clone())
        .unwrap_or_else(|| "Case".to_string())
}

/// The correction for a mismatch, when the language offers exactly one.
fn conversion_help(expected: &Ty, found: &Ty) -> Option<String> {
    Some(match (expected, found) {
        (Ty::Str, _) => {
            format!("interpolate the `{found}`, as in \"{{value}}\", to make a `String`")
        }
        (Ty::Int, Ty::Str) => {
            "parse the `String` with `Int.parse(text)`, which returns a `Result<Int, Error>`"
                .to_string()
        }
        (Ty::Option(inner), other) if inner.matches(other) => {
            format!("wrap it, as in `Some(value)`, to make an `Option<{inner}>`")
        }
        (Ty::Result(ok, _), other) if ok.matches(other) => {
            format!("wrap it, as in `Ok(value)`, to make a `Result<{ok}, _>`")
        }
        (other, Ty::Option(inner)) if other.matches(inner) => {
            format!(
                "unwrap it, as in `value.unwrapOr(<{other}>)`, which always produces a `{other}`"
            )
        }
        (other, Ty::Result(ok, _)) if other.matches(ok) => {
            format!(
                "unwrap it, as in `value.unwrapOr(<{other}>)`, which always produces a `{other}`"
            )
        }
        (Ty::Array(element), Ty::Vector(other)) if element.matches(other) => {
            "finish the vector, as in `vector.freeze()` or `vector.toArray()`".to_string()
        }
        (Ty::Float, Ty::Int) | (Ty::Int, Ty::Float) => {
            format!("write the literal as a `{expected}`; Cove converts nothing implicitly")
        }
        _ => return None,
    })
}

fn condition_help(ty: &Ty) -> String {
    match ty {
        Ty::Option(_) => "compare it, as in `value.isSome()`".to_string(),
        Ty::Int | Ty::Float => format!("compare it, as in `value != 0`; a `{ty}` is not a `Bool`"),
        _ => format!("compare it, so the condition is a `Bool` rather than a `{ty}`"),
    }
}

fn iterable_help(ty: &Ty) -> String {
    match ty {
        Ty::Option(inner) => {
            format!("match the `Option`, or write `for x in [value.unwrapOr(<{inner}>)]`")
        }
        Ty::Int => "write a range, as in `0..<n`".to_string(),
        Ty::MapEntry(_, _) => {
            "iterate the `Map` itself; `for` already binds each pair as a `MapEntry`".to_string()
        }
        _ => format!("build an `Array`, a `Vector`, or a `Range` from the `{ty}` first"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::package::{Module, Unit};
    use crate::resolve::resolve;
    use cove_diag::{Severity, SourceMap};
    use std::path::{Path, PathBuf};

    /// Type-checks one module written inline, without touching the
    /// filesystem, and returns everything it reported.
    fn diagnostics_of(source: &str) -> Vec<Diagnostic> {
        diagnostics_with(source, Config::default())
    }

    fn diagnostics_with(source: &str, config: Config) -> Vec<Diagnostic> {
        let mut sources = SourceMap::new();
        let path = PathBuf::from("main.cove");
        let file = sources.add(path.clone(), source);
        let ast = cove_syntax::parse_file(&sources, file).expect("test source parses");
        let mut modules = BTreeMap::new();
        modules.insert(
            "main".to_string(),
            Module {
                name: "main".to_string(),
                dir: PathBuf::from("main"),
                units: vec![Unit { file, path, ast }],
            },
        );
        let package = Package {
            root: PathBuf::new(),
            config,
            modules,
        };
        let program = resolve(&package).expect("test source resolves");
        check(&package, &program)
    }

    /// Type-checks several modules written inline, so one can `use` another.
    fn diagnostics_of_modules(modules: &[(&str, &str)]) -> Vec<Diagnostic> {
        let mut sources = SourceMap::new();
        let mut map = BTreeMap::new();
        for (name, source) in modules {
            let path = PathBuf::from(format!("{name}.cove"));
            let file = sources.add(path.clone(), *source);
            let ast = cove_syntax::parse_file(&sources, file).expect("test source parses");
            map.insert(
                (*name).to_string(),
                Module {
                    name: (*name).to_string(),
                    dir: PathBuf::from(*name),
                    units: vec![Unit { file, path, ast }],
                },
            );
        }
        let package = Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules: map,
        };
        let program = resolve(&package).expect("test package resolves");
        check(&package, &program)
    }

    #[track_caller]
    fn accepts_modules(modules: &[(&str, &str)]) {
        let errors: Vec<Diagnostic> = diagnostics_of_modules(modules)
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "expected no errors, found: {}",
            errors
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    #[track_caller]
    fn rejects_modules(modules: &[(&str, &str)]) -> Diagnostic {
        let mut errors: Vec<Diagnostic> = diagnostics_of_modules(modules)
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.len(),
            1,
            "expected exactly one error, found: {}",
            errors
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        );
        errors.remove(0)
    }

    fn errors_of(source: &str) -> Vec<Diagnostic> {
        diagnostics_of(source)
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    fn warnings_of(source: &str) -> Vec<Diagnostic> {
        diagnostics_of(source)
            .into_iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect()
    }

    fn notes_of(source: &str) -> Vec<Diagnostic> {
        diagnostics_of(source)
            .into_iter()
            .filter(|d| d.severity == Severity::Note)
            .collect()
    }

    /// Asserts that `source` produces exactly one warning, and returns it.
    #[track_caller]
    fn warns(source: &str) -> Diagnostic {
        accepts(source);
        let mut warnings = warnings_of(source);
        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one warning, found: {}",
            warnings
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        );
        warnings.remove(0)
    }

    /// Asserts that `source` checks, showing what was reported when it does
    /// not.
    #[track_caller]
    fn accepts(source: &str) {
        let errors = errors_of(source);
        assert!(
            errors.is_empty(),
            "expected no errors, found: {}",
            errors
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    /// Asserts that `source` produces exactly one error, and returns it.
    #[track_caller]
    fn rejects(source: &str) -> Diagnostic {
        let mut errors = errors_of(source);
        assert_eq!(
            errors.len(),
            1,
            "expected exactly one error, found: {}",
            errors
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        );
        errors.remove(0)
    }

    #[test]
    fn accepts_a_test_of_the_shape_the_runner_calls() {
        accepts("test fn passes() -> Result<Unit, Error> {\n  Ok(())\n}\n");
    }

    #[test]
    fn rejects_a_test_that_declares_a_parameter() {
        let error = rejects("test fn passes(n: Int) -> Result<Unit, Error> {\n  Ok(())\n}\n");
        assert_eq!(error.code, TEST);
        assert!(error.message.contains("declares 1 parameter(s)"));
        assert_eq!(
            error.help.as_deref(),
            Some("write `test fn passes() -> Result<Unit, Error>`")
        );
    }

    #[test]
    fn rejects_a_test_that_returns_something_else() {
        for source in [
            "test fn passes() -> Int {\n  1\n}\n",
            "test fn passes() {\n}\n",
            "test fn passes() -> Result<Int, Error> {\n  Ok(1)\n}\n",
        ] {
            let error = rejects(source);
            assert_eq!(error.code, TEST, "{source}");
            assert!(
                error
                    .message
                    .contains("a test returns `Result<Unit, Error>`"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn rejects_an_async_test() {
        let error = rejects("test async fn passes() -> Result<Unit, Error> {\n  Ok(())\n}\n");
        assert_eq!(error.code, TEST);
        assert!(error.message.contains("is `async`"));
    }

    #[test]
    fn assert_takes_a_bool_and_produces_a_result() {
        accepts("test fn passes() -> Result<Unit, Error> {\n  assert(1 == 1)?\n  Ok(())\n}\n");
        let error =
            rejects("test fn passes() -> Result<Unit, Error> {\n  assert(1)?\n  Ok(())\n}\n");
        assert_eq!(error.code, MISMATCH);
    }

    #[test]
    fn assert_equal_compares_two_values_of_one_type() {
        accepts(
            "test fn passes() -> Result<Unit, Error> {\n  assertEqual(1 + 1, 2)?\n  Ok(())\n}\n",
        );
        let error = rejects(
            "test fn passes() -> Result<Unit, Error> {\n  assertEqual(1, \"1\")?\n  Ok(())\n}\n",
        );
        assert_eq!(error.code, MISMATCH);
    }

    #[test]
    fn an_assertion_takes_the_number_of_arguments_it_declares() {
        let error =
            rejects("test fn passes() -> Result<Unit, Error> {\n  assert()?\n  Ok(())\n}\n");
        assert_eq!(error.code, ARITY);
        let error =
            rejects("test fn passes() -> Result<Unit, Error> {\n  assertEqual(1)?\n  Ok(())\n}\n");
        assert_eq!(error.code, ARITY);
    }

    #[test]
    fn a_declaration_of_the_same_name_wins_over_the_assertion_builtin() {
        // The module's own `assert` answers first, exactly as it does for
        // every other builtin the checker knows.
        accepts(
            "fn assert(message: String) -> Result<Unit, Error> {\n  Ok(())\n}\n\n             test fn passes() -> Result<Unit, Error> {\n  assert(\"anything\")?\n  Ok(())\n}\n",
        );
    }

    /// Wraps `body` in an entry function, the shape most of these tests need.
    fn in_main(body: &str) -> String {
        format!(
            "use console.println\n\nexport fn main() -> Result<Unit, Error> {{\n{body}\n  Ok(())\n}}\n"
        )
    }

    #[track_caller]
    fn accepts_body(body: &str) {
        accepts(&in_main(body));
    }

    #[track_caller]
    fn rejects_body(body: &str) -> Diagnostic {
        rejects(&in_main(body))
    }

    // ------------------------------------------------------- accepted

    #[test]
    fn accepts_the_card_s_greeting_program() {
        accepts(
            "\
use console.println

export fn greet(name: String) -> String {
  \"Hello, {name}!\"
}

export fn main(args: Array<String>) -> Result<Unit, Error> {
  let name = args.get(0).unwrapOr(\"world\")
  console.println(greet(name))?
  Ok(())
}
",
        );
    }

    #[test]
    fn infers_a_let_from_its_initializer() {
        accepts_body("  let n = 1\n  let doubled = n * 2\n  println(\"{doubled}\")?");
    }

    #[test]
    fn checks_a_written_let_annotation() {
        accepts_body("  let n: Int = 1\n  println(\"{n}\")?");
        let error = rejects_body("  let n: Int = \"one\"");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
        assert_eq!(error.rule.unwrap(), "Types are nominal and the only implicit conversion is to `dyn Trait`: a value must otherwise already have the type its place asks for.");
        assert_eq!(
            error.help.unwrap(),
            "parse the `String` with `Int.parse(text)`, which returns a `Result<Int, Error>`"
        );
    }

    #[test]
    fn an_array_literal_takes_its_element_type_from_its_elements() {
        accepts_body("  let items = [1, 2]\n  let first: Option<Int> = items.get(0)");
        let error = rejects_body("  let items = [1, \"two\"]");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn an_empty_array_literal_has_no_element_type_to_infer() {
        // Nothing says what the elements are, so operations that do not
        // depend on the element type still work and the rest is unchecked.
        // The gap is a warning of its own, pinned with the other unknowns.
        accepts_body("  let empty = []\n  println(\"{empty.length()} {empty.isEmpty()}\")?");
    }

    #[test]
    fn a_vector_grows_and_freezes_into_an_array() {
        accepts(
            "\
fn build(upTo: Int) -> Array<Int> {
  var building = Vector.of(1)
  for n in 1..upTo {
    building.push(n)
  }
  building.freeze()
}
",
        );
        let error = rejects(
            "\
fn build() -> Array<String> {
  var building = Vector.of(1)
  building.freeze()
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "expected `Array<String>`, found `Array<Int>`"
        );
    }

    #[test]
    fn snapshot_returns_the_receiver_s_own_type_for_every_builtin_value() {
        accepts_body(
            "\
  let n: Int = 1.snapshot()
  let s: String = \"a\".snapshot()
  let arr: Array<Int> = [1, 2].snapshot()
  var v: Vector<Int> = Vector.of(1).snapshot()
  println(\"{n} {s} {arr} {v}\")?
",
        );
    }

    #[test]
    fn rejects_snapshot_on_a_closure() {
        let error = rejects_body(
            "\
  let handler = fn(x: Int) { x }
  println(\"{handler.snapshot()}\")?
",
        );
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(
            error.message,
            "`fn(Int) -> Int` does not implement `Snapshot`"
        );
        assert!(error.rule.unwrap().contains("Closures"));
    }

    #[test]
    fn a_vector_push_takes_the_element_type() {
        let error = rejects(
            "\
fn build() -> Array<Int> {
  var building = Vector.of(1)
  building.push(\"two\")
  building.freeze()
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    /// `Result.unwrapOr` is `Option.unwrapOr`'s sibling, so the two are
    /// checked together: the fallback is the type inside, the result is that
    /// type, and the error type is not named by either half of the
    /// signature.
    #[test]
    fn unwrap_or_takes_the_type_inside_on_both_option_and_result() {
        accepts_body(
            "  let found: Int = [1].get(0).unwrapOr(0)\n\
             \x20 let parsed: Int = Int.parse(\"1\").unwrapOr(0)\n\
             \x20 let mapped: Int = Int.parse(\"1\").mapError { \"bad\" }.unwrapOr(0)",
        );
        let error = rejects_body("  Int.parse(\"1\").unwrapOr(\"zero\")");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
        let error = rejects_body("  let n: String = Int.parse(\"1\").unwrapOr(0)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    /// A `Result` where its own `Ok` type was expected now has the same one
    /// correction an `Option` has, because it now has the same method.
    #[test]
    fn a_result_where_its_ok_type_belongs_is_told_about_unwrap_or() {
        let error = rejects_body("  let n: Int = Int.parse(\"1\")");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.help.unwrap(),
            "unwrap it, as in `value.unwrapOr(<Int>)`, which always produces a `Int`"
        );
    }

    /// `Int.parse` is one argument and decimal, and `Int.parseRadix` is two
    /// and is not. That `parse` keeps its arity is the point of their being
    /// two functions, so it is what this checks first.
    #[test]
    fn int_parse_and_parse_radix_are_two_signatures() {
        accepts_body(
            "  let decimal: Result<Int, Error> = Int.parse(\"12\")\n\
             \x20 let hex: Result<Int, Error> = Int.parseRadix(\"ff\", 16)",
        );
        let error = rejects_body("  Int.parse(\"ff\", 16)");
        assert_eq!(error.code, ARITY);
        let error = rejects_body("  Int.parseRadix(\"ff\")");
        assert_eq!(error.code, MISSING_ARGUMENT);
        let error = rejects_body("  Int.parseRadix(\"ff\", \"16\")");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    /// A character is a `String` of length 1, so this answers a `String` and
    /// not some type of its own — and a `Result`, because a number that names
    /// no character is a failure the caller handles.
    #[test]
    fn from_code_point_answers_a_result_of_string() {
        accepts_body(
            "  let character: Result<String, Error> = String.fromCodePoint(65)\n\
             \x20 let letter: String = String.fromCodePoint(65).unwrapOr(\"?\")",
        );
        let error = rejects_body("  String.fromCodePoint(\"A\")");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
        let error = rejects_body("  let letter: String = String.fromCodePoint(65)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "expected `String`, found `Result<String, Error>`"
        );
    }

    #[test]
    fn checks_every_map_operation() {
        accepts_body(
            "  let ages = Map.of(MapEntry(key: \"Alice\", value: 30))\n\
             \x20 let found: Option<Int> = ages.get(\"Alice\")\n\
             \x20 let has: Bool = ages.contains(\"Bob\")\n\
             \x20 let n: Int = ages.length()\n\
             \x20 let empty: Bool = ages.isEmpty()\n\
             \x20 let names: Array<String> = ages.keys()\n\
             \x20 let numbers: Array<Int> = ages.values()\n\
             \x20 let more: Map<String, Int> = ages.inserted(\"Carol\", 41)\n\
             \x20 let fewer: Map<String, Int> = ages.removed(\"Alice\")",
        );
        let error =
            rejects_body("  let ages = Map.of(MapEntry(key: \"Alice\", value: 30))\n  ages.get(1)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn checks_every_set_operation() {
        accepts_body(
            "  let tags = Set.of(\"a\", \"b\")\n\
             \x20 let has: Bool = tags.contains(\"a\")\n\
             \x20 let n: Int = tags.length()\n\
             \x20 let empty: Bool = tags.isEmpty()\n\
             \x20 let items: Array<String> = tags.toArray()\n\
             \x20 let bigger: Set<String> = tags.inserted(\"c\")\n\
             \x20 let smaller: Set<String> = tags.removed(\"a\")",
        );
        let error = rejects_body("  let tags = Set.of(\"a\")\n  tags.contains(1)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn map_of_collects_map_entries() {
        let error = rejects_body("  let ages = Map.of(1)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `MapEntry<_, _>`, found `Int`");

        let error = rejects_body(
            "  let ages = Map.of(MapEntry(key: \"a\", value: 1), MapEntry(key: 2, value: 3))",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "expected `MapEntry<String, Int>`, found `MapEntry<Int, Int>`"
        );
    }

    #[test]
    fn a_map_entry_carries_a_key_and_a_value() {
        accepts_body(
            "  let entry = MapEntry(key: \"a\", value: 1)\n\
             \x20 let key: String = entry.key\n\
             \x20 let value: Int = entry.value",
        );
        let error = rejects_body("  let entry = MapEntry(key: \"a\", value: 1)\n  entry.other");
        assert_eq!(error.code, UNKNOWN_FIELD);
        assert_eq!(error.message, "`MapEntry` has no field `other`");
        assert_eq!(
            error.rule.unwrap(),
            "A builtin struct's fields are exactly the ones the language defines."
        );
        assert_eq!(error.help.unwrap(), "`MapEntry` declares `key`, `value`");
    }

    /// The runtime builds an `Error` with a `message` and has always served a
    /// read of it; the checker used to answer that `Error` had no such field
    /// and suggest a method `Error` does not have. One table is what let the
    /// two agree.
    #[test]
    fn an_error_carries_a_message() {
        accepts_body("  let message: String = Error(\"boom\").message");
        accepts_body(
            "  let outcome: Result<Int, Error> = Err(Error(\"boom\"))\n\
             \x20 match outcome {\n\
             \x20   Ok(n) => n,\n\
             \x20   Err(failure) => failure.message.length()\n\
             \x20 }",
        );
        let error = rejects_body("  let code = Error(\"boom\").code");
        assert_eq!(error.code, UNKNOWN_FIELD);
        assert_eq!(error.message, "`Error` has no field `code`");
        assert_eq!(
            error.rule.unwrap(),
            "A builtin struct's fields are exactly the ones the language defines."
        );
        assert_eq!(error.help.unwrap(), "`Error` declares `message`");
    }

    /// An `Error`'s message is a `String`, so using it as anything else is
    /// the ordinary mismatch rather than an unknown field.
    #[test]
    fn an_error_s_message_is_a_string() {
        let error = rejects_body("  let code: Int = Error(\"boom\").message");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn a_map_iterates_map_entries_and_a_set_its_elements() {
        accepts_body(
            "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n\
             \x20 for entry in ages {\n\
             \x20   let key: String = entry.key\n\
             \x20   let value: Int = entry.value\n\
             \x20 }\n\
             \x20 for tag in Set.of(\"a\") {\n\
             \x20   let element: String = tag\n\
             \x20 }",
        );
        let error = rejects_body(
            "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n\
             \x20 for entry in ages {\n\
             \x20   let key: Int = entry.key\n\
             \x20 }",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn checks_every_range_operation() {
        accepts_body(
            "  let range = 0..<3\n\
             \x20 let n: Int = range.length()\n\
             \x20 let empty: Bool = range.isEmpty()\n\
             \x20 let has: Bool = range.contains(1)",
        );
        let error = rejects_body("  let range = 0..<3\n  range.contains(\"one\")");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn checks_struct_initialization_and_field_access() {
        accepts(
            "\
struct Point { x: Int, y: Int }

fn sum(point: Point) -> Int {
  point.x + point.y
}

fn origin() -> Point {
  Point(x: 0, y: 0)
}
",
        );
    }

    #[test]
    fn rejects_a_struct_field_of_the_wrong_type() {
        let error = rejects(
            "\
struct Point { x: Int, y: Int }

fn origin() -> Point {
  Point(x: 0, y: \"zero\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
        assert_eq!(error.labels[0].message, "the field `y` is `Int`");
    }

    #[test]
    fn rejects_a_missing_struct_field() {
        let error = rejects(
            "\
struct Point { x: Int, y: Int }

fn origin() -> Point {
  Point(x: 0)
}
",
        );
        assert_eq!(error.code, MISSING_ARGUMENT);
        assert_eq!(error.message, "`Point` needs the field `y`");
        assert_eq!(
            error.rule.unwrap(),
            "A call passes every parameter that has no default."
        );
        assert_eq!(error.help.unwrap(), "pass `y: <Int>`");
    }

    #[test]
    fn rejects_a_field_the_struct_does_not_declare() {
        let error = rejects(
            "\
struct Point { x: Int, y: Int }

fn z(point: Point) -> Int {
  point.z
}
",
        );
        assert_eq!(error.code, UNKNOWN_FIELD);
        assert_eq!(error.message, "`Point` has no field `z`");
        assert_eq!(
            error.rule.unwrap(),
            "A struct's fields are exactly the ones its declaration lists."
        );
        assert_eq!(error.help.unwrap(), "`Point` declares `x`, `y`");
    }

    #[test]
    fn checks_enum_construction_and_match_payloads() {
        accepts(
            "\
enum Status {
  Pending
  Active(Int)
}

fn describe(status: Status) -> String {
  match status {
    Status.Pending => \"pending\"
    Status.Active(since) => \"active since {since}\"
  }
}

fn active() -> Status {
  Status.Active(7)
}
",
        );
    }

    #[test]
    fn rejects_an_enum_payload_of_the_wrong_type() {
        let error = rejects(
            "\
enum Status {
  Active(Int)
}

fn active() -> Status {
  Status.Active(\"now\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn rejects_an_enum_payload_of_the_wrong_arity() {
        let error = rejects(
            "\
enum Status {
  Active(Int)
}

fn active() -> Status {
  Status.Active(1, 2)
}
",
        );
        assert_eq!(error.code, PAYLOAD_ARITY);
        assert_eq!(
            error.message,
            "`Status.Active` carries 1 value(s), but 2 were given"
        );
        assert_eq!(
            error.rule.unwrap(),
            "An enum case carries exactly the payload its declaration writes."
        );
        assert_eq!(error.help.unwrap(), "write `Status.Active(Int)`");
    }

    #[test]
    fn rejects_a_case_the_enum_does_not_declare() {
        let error = rejects(
            "\
enum Status {
  Pending
}

fn active() -> Status {
  Status.Active
}
",
        );
        assert_eq!(error.code, UNKNOWN_CASE);
        assert_eq!(error.message, "`Status` has no case `Active`");
        assert_eq!(
            error.rule.unwrap(),
            "An enum's cases are exactly the ones its declaration lists."
        );
        assert_eq!(error.help.unwrap(), "`Status` declares `Pending`");
    }

    #[test]
    fn rejects_a_pattern_from_another_enum() {
        let error = rejects(
            "\
enum Suit {
  Hearts
}

enum Card {
  Blank
}

fn name(card: Card) -> String {
  match card {
    Suit.Hearts => \"hearts\"
    _ => \"other\"
  }
}
",
        );
        assert_eq!(error.code, PATTERN);
        assert_eq!(
            error.message,
            "this pattern matches `Suit`, but the scrutinee is `Card`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A pattern matches values of the scrutinee's type."
        );
        assert_eq!(
            error.help.unwrap(),
            "write a `Card` case, such as `Card.Blank`"
        );
    }

    #[test]
    fn rejects_a_literal_pattern_of_another_type() {
        let error = rejects(
            "\
fn name(n: Int) -> String {
  match n {
    \"one\" => \"one\"
    _ => \"other\"
  }
}
",
        );
        assert_eq!(error.code, PATTERN);
        assert_eq!(
            error.message,
            "this pattern matches `String`, but the scrutinee is `Int`"
        );
        assert_eq!(
            error.help.unwrap(),
            "write a `Int` literal, or a binding such as `other`"
        );
    }

    #[test]
    fn checks_methods_and_associated_functions() {
        accepts(
            "\
struct Counter { hits: Int }

impl Counter {
  fn start() -> Counter {
    Counter(hits: 0)
  }

  fn hit(var self) {
    self.hits += 1
  }

  fn describe(self) -> String {
    \"{self.hits}\"
  }
}

fn run() -> String {
  var counter = Counter.start()
  counter.hit()
  counter.describe()
}
",
        );
    }

    #[test]
    fn a_method_needs_a_receiver_and_an_associated_function_takes_none() {
        let source = "\
struct Counter { hits: Int }

impl Counter {
  fn start() -> Counter {
    Counter(hits: 0)
  }

  fn describe(self) -> String {
    \"{self.hits}\"
  }
}
";
        let error = rejects(&format!(
            "{source}\nfn run() -> String {{\n  Counter.describe()\n}}\n"
        ));
        assert_eq!(error.code, RECEIVER);
        assert_eq!(
            error.message,
            "`Counter.describe` is a method and needs a receiver"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A method is called on a value; only an associated function is called on its type."
        );
        assert_eq!(
            error.help.unwrap(),
            "call it on a value, as in `value.describe(...)`, or declare `fn describe()` without `self`"
        );

        let error = rejects(&format!(
            "{source}\nfn run(counter: Counter) -> Counter {{\n  counter.start()\n}}\n"
        ));
        assert_eq!(error.code, RECEIVER);
        assert_eq!(error.message, "`Counter.start` takes no receiver");
        assert_eq!(error.help.unwrap(), "write `Counter.start(...)`");
    }

    #[test]
    fn rejects_a_method_the_type_does_not_declare() {
        let error = rejects(
            "\
struct Counter { hits: Int }

impl Counter {
  fn describe(self) -> String {
    \"{self.hits}\"
  }
}

fn run(counter: Counter) -> String {
  counter.report()
}
",
        );
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Counter` has no method `report`");
        assert_eq!(
            error.rule.unwrap(),
            "A method is declared in its type's `impl` block."
        );
        assert_eq!(error.help.unwrap(), "`Counter` declares `describe`");
    }

    #[test]
    fn rejects_an_associated_function_the_type_does_not_declare() {
        let error = rejects(
            "\
struct Counter { hits: Int }

fn run() -> Counter {
  Counter.start()
}
",
        );
        assert_eq!(error.code, UNKNOWN_ASSOCIATED);
        assert_eq!(
            error.message,
            "`Counter` has no associated function `start`"
        );
        assert_eq!(
            error.help.unwrap(),
            "`Counter` declares no methods; declare one in `impl Counter`"
        );
    }

    #[test]
    fn count_is_spelled_length() {
        let error = rejects_body("  let items = [1]\n  println(\"{items.count()}\")?");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(
            error.message,
            "`Array` has no method `count`; Cove spells the number of elements `length()`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "Every sequence reports its element count as `length()`; there is no `count()`."
        );
        assert_eq!(error.help.unwrap(), "write `length()` instead of `count()`");
    }

    /// Every receiver that answers `length()` is told so, which is what the
    /// runtime already did: `Map` and `Set` used to be taught the spelling at
    /// run time and told nothing here.
    #[test]
    fn every_sequence_is_told_that_count_is_spelled_length() {
        let receivers = [
            ("Array", "let items = [1]\n  items"),
            ("Vector", "var items = Vector.of(1)\n  items"),
            ("String", "let text = \"ab\"\n  text"),
            ("Range", "let span = 0..<3\n  span"),
            (
                "Map",
                "let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  ages",
            ),
            ("Set", "let seen = Set.of(1)\n  seen"),
        ];
        for (type_name, receiver) in receivers {
            let error = rejects_body(&format!("  {receiver}.count()"));
            assert_eq!(error.code, UNKNOWN_METHOD, "{type_name}");
            assert_eq!(
                error.message,
                format!(
                    "`{type_name}` has no method `count`; Cove spells the number of elements `length()`"
                )
            );
            assert_eq!(
                error.help.unwrap(),
                "write `length()` instead of `count()`",
                "{type_name}"
            );
        }
    }

    // ------------------------------ walking a sequence with a closure

    /// A callback's own parameters come from the receiver's element type,
    /// with nothing written, and its body is checked in them.
    ///
    /// This is the signature a higher-order builtin is most easily got
    /// wrong: the closure is written at the call site with no types on it,
    /// so everything it is held to comes from the shared table by way of the
    /// receiver.
    #[test]
    fn a_callbacks_parameters_come_from_the_element_type() {
        accepts_body(
            "  let words = [\"a\", \"bb\"]\n  \
             let lengths = words.map(fn(w) { w.length() })\n  \
             let long = words.filter(fn(w) { w.length() > 1 })\n  \
             let total = words.fold(0, fn(t, w) { t + w.length() })\n  \
             let ordered = words.sorted(by: fn(a, b) { a < b })",
        );
        let error = rejects_body("  let words = [\"a\"]\n  let n = words.map(fn(w) { w + 1 })");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `String` and `Int`");
    }

    /// What a walk answers is read off the callback, and it is an `Array`
    /// whichever sequence the walk started from.
    #[test]
    fn a_walk_answers_an_array_of_what_its_callback_produced() {
        for (receiver, answer) in [
            ("let items = [1, 2]", "Array<String>"),
            ("var items = Vector.of(1, 2)", "Array<String>"),
        ] {
            let error = rejects_body(&format!(
                "  {receiver}\n  let n: Int = items.map(fn(v) {{ \"{{v}}\" }})"
            ));
            assert_eq!(error.code, MISMATCH);
            assert_eq!(error.message, format!("expected `Int`, found `{answer}`"));
        }
        let error = rejects_body(
            "  let items = [1, 2]\n  let n: Int = items.sorted(by: fn(a, b) { a < b })",
        );
        assert_eq!(error.message, "expected `Int`, found `Array<Int>`");
        let error = rejects_body(
            "  var items = Vector.of(1, 2)\n  let n: Int = items.filter(fn(v) { v > 1 })",
        );
        assert_eq!(error.message, "expected `Int`, found `Array<Int>`");
    }

    /// `fold`'s accumulator is settled by `initial`, so a `step` that answers
    /// something else is a mismatch and not a second accumulator type.
    #[test]
    fn folds_accumulator_is_the_type_its_initial_value_has() {
        accepts_body(
            "  let items = [1, 2]\n  let text = items.fold(\"\", fn(t, n) { \"{t}{n}\" })",
        );
        let error =
            rejects_body("  let items = [1, 2]\n  let n = items.fold(0, fn(t, v) { \"{t}\" })");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    /// A callback of the wrong arity is reported against the shape the
    /// signature declares, at the closure rather than at the call.
    #[test]
    fn a_callback_takes_the_parameters_its_builtin_declares() {
        let error =
            rejects_body("  let items = [2, 1]\n  let n = items.sorted(by: fn(a) { true })");
        assert_eq!(error.code, ARITY);
        assert_eq!(
            error.message,
            "this function takes 1 parameter(s), but 2 were expected here"
        );
        let error = rejects_body("  let items = [2, 1]\n  let n = items.map(fn(a, b) { a })");
        assert_eq!(error.code, ARITY);
        assert_eq!(
            error.message,
            "this function takes 2 parameter(s), but 1 were expected here"
        );
    }

    /// `filter` and `sorted` declare a `Bool` result, so a callback that
    /// answers anything else is refused — which is also what makes a `?`
    /// inside one a check-time mismatch rather than a runtime surprise.
    #[test]
    fn a_predicate_callback_must_answer_a_bool() {
        let error = rejects_body("  let items = [1, 2]\n  let n = items.filter(fn(v) { v })");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Bool`, found `Int`");
        let error =
            rejects_body("  let items = [2, 1]\n  let n = items.sorted(by: fn(a, b) { a - b })");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Bool`, found `Int`");
    }

    // -------------------- membership, position, and part of a sequence

    /// `contains`, `indexOf`, and `slice` read the same on either sequence,
    /// and each answers what the shared table says.
    ///
    /// The element parameter is where these are got wrong: it is the
    /// receiver's own `T`, so a `contains` of the wrong type is a mismatch
    /// rather than a `false`, which is the whole reason a sequence's
    /// membership is checked and a `Map`'s `Any` key would not be.
    #[test]
    fn a_sequence_answers_membership_position_and_a_part_of_itself() {
        for receiver in ["let items = [1, 2]", "var items = Vector.of(1, 2)"] {
            accepts_body(&format!(
                "  {receiver}\n  \
                 let held: Bool = items.contains(1)\n  \
                 let at: Option<Int> = items.indexOf(2)\n  \
                 let first: Array<Int> = items.slice(0, 1)"
            ));
            let error = rejects_body(&format!("  {receiver}\n  let n = items.contains(\"1\")"));
            assert_eq!(error.code, MISMATCH);
            assert_eq!(error.message, "expected `Int`, found `String`");
            let error = rejects_body(&format!("  {receiver}\n  let n: Int = items.indexOf(1)"));
            assert_eq!(error.message, "expected `Int`, found `Option<Int>`");
            let error = rejects_body(&format!("  {receiver}\n  let n = items.slice(0)"));
            assert_eq!(error.code, MISSING_ARGUMENT);
        }
    }

    /// A `Set` answers membership and nothing about a position, because a
    /// set has none to answer about.
    ///
    /// The ascending order a `Set` and a `Map` are stored in is the
    /// collection's, not a caller's: `toArray()` is where a program takes
    /// that ordering as its own, and what it answers has both.
    #[test]
    fn an_unordered_collection_answers_membership_and_not_a_position() {
        accepts_body("  let seen = Set.of(1, 2)\n  let held: Bool = seen.contains(1)");
        accepts_body(
            "  let seen = Set.of(1, 2)\n  let at: Option<Int> = seen.toArray().indexOf(1)",
        );
        let error = rejects_body("  let seen = Set.of(1, 2)\n  let n = seen.indexOf(1)");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Set` has no method `indexOf`");
        let error = rejects_body(
            "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n  let n = ages.slice(0, 1)",
        );
        assert_eq!(error.message, "`Map` has no method `slice`");
    }

    /// `set` replaces an element, so it takes the receiver's own element
    /// type and answers what was there.
    #[test]
    fn a_vector_replaces_an_element_with_one_of_its_own_type() {
        accepts_body("  var items = Vector.of(1, 2)\n  let was: Option<Int> = items.set(0, 9)");
        let error = rejects_body("  var items = Vector.of(1, 2)\n  let n = items.set(0, \"9\")");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
        let error = rejects_body("  var items = Vector.of(1, 2)\n  let n = items.set(\"0\", 9)");
        assert_eq!(error.message, "expected `Int`, found `String`");
        let error = rejects_body("  var items = Vector.of(1, 2)\n  let n: Int = items.set(0, 9)");
        assert_eq!(error.message, "expected `Int`, found `Option<Int>`");
        // An `Array` is immutable, so it has no such method to reach at all
        // — however the receiver was bound. The place rule asks the shared
        // table what *this* receiver declares rather than asking the name,
        // so a `let` array is told it has no `set` rather than told to be a
        // `var` first.
        for receiver in ["var items = [1, 2]", "let items = [1, 2]"] {
            let error = rejects_body(&format!("  {receiver}\n  let n = items.set(0, 9)"));
            assert_eq!(error.code, UNKNOWN_METHOD);
            assert_eq!(error.message, "`Array` has no method `set`");
        }
    }

    /// `pop` and `remove` take an element back out, so both answer the
    /// receiver's own element type inside an `Option`, and `remove` takes
    /// the index by the name `get` and `set` already call it.
    #[test]
    fn a_vector_answers_what_it_took_out() {
        accepts_body(
            "  var items = Vector.of(1, 2)\n  \
             let last: Option<Int> = items.pop()\n  \
             let first: Option<Int> = items.remove(0)",
        );
        let error = rejects_body("  var items = Vector.of(1, 2)\n  let n: Int = items.pop()");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `Option<Int>`");
        let error = rejects_body("  var items = Vector.of(1, 2)\n  let n = items.remove(\"0\")");
        assert_eq!(error.message, "expected `Int`, found `String`");
        // An `Array` is immutable and a `Set`'s removal answers a new set,
        // so neither has these; and `Vector` has no `removed`, because a
        // past participle would say it answered a new collection.
        for receiver in ["let items = [1, 2]", "var items = [1, 2]"] {
            let error = rejects_body(&format!("  {receiver}\n  let n = items.pop()"));
            assert_eq!(error.code, UNKNOWN_METHOD);
            assert_eq!(error.message, "`Array` has no method `pop`");
        }
        let error = rejects_body("  var items = Vector.of(1, 2)\n  let n = items.removed(0)");
        assert_eq!(error.message, "`Vector` has no method `removed`");
        // There is no `clear`: emptying a vector is `pop` in a loop, or a
        // rebinding.
        let error = rejects_body("  var items = Vector.of(1, 2)\n  items.clear()");
        assert_eq!(error.message, "`Vector` has no method `clear`");
    }

    /// `pop` and `remove` mutate, so their receivers are under exactly the
    /// place rule `push` and `set` are under.
    #[test]
    fn rejects_a_removal_on_a_read_only_place_and_on_no_place() {
        for call in ["pop()", "remove(0)"] {
            let error = rejects(&format!(
                "fn run() -> Int {{\n  let items = Vector.of(1)\n  let n = items.{call}\n  0\n}}\n"
            ));
            assert_eq!(error.code, READ_ONLY_PLACE);
            assert!(
                error.message.ends_with("but `items` is a read-only place"),
                "{}",
                error.message
            );
            let error = rejects(&format!(
                "fn run() -> Int {{\n  let n = Vector.of(1).{call}\n  0\n}}\n"
            ));
            assert_eq!(error.code, NOT_A_PLACE);
        }
    }

    /// `toVector` answers a growable copy of an array, and it is on `Array`
    /// only: an independent vector from a vector is `snapshot()`.
    #[test]
    fn an_array_answers_a_growable_copy_of_itself() {
        accepts_body("  let items = [1, 2]\n  var building: Vector<Int> = items.toVector()");
        let error = rejects_body("  let items = [1, 2]\n  let n: Array<Int> = items.toVector()");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Array<Int>`, found `Vector<Int>`");
        let error = rejects_body("  var items = Vector.of(1, 2)\n  let n = items.toVector()");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Vector` has no method `toVector`");
    }

    /// `set` mutates, so its receiver is the caller's place under exactly
    /// the rule `push`'s receiver is under.
    #[test]
    fn rejects_set_on_a_read_only_place_and_on_no_place() {
        let error = rejects(
            "fn run() -> Int {\n  let items = Vector.of(1)\n  items.set(0, 2)\n  items.length()\n}\n",
        );
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "`set` takes a `var self` receiver, but `items` is a read-only place"
        );
        assert_eq!(error.help.unwrap(), "declare it with `var items`");
        let error = rejects("fn run() -> Int {\n  Vector.of(1).set(0, 2)\n  0\n}\n");
        assert_eq!(error.code, NOT_A_PLACE);
        assert_eq!(
            error.message,
            "`set` takes a `var self` receiver, but `this expression` is not a place"
        );
    }

    // ------------------------------------ building and reading a duration

    /// A `Duration` is built from a number in any of the six units a literal
    /// is written in, and read back in the same six.
    #[test]
    fn a_duration_is_built_from_a_count_and_read_back_as_one() {
        accepts_body(
            "  let timeout: Duration = Duration.millis(250)\n  \
             let whole: Duration = Duration.nanos(1) + Duration.micros(1) + \
             Duration.seconds(1) + Duration.minutes(1) + Duration.hours(1)\n  \
             let back: Int = timeout.millis()\n  \
             let coarse: Int = whole.seconds()",
        );
        // The builder takes an `Int`; a `Duration` is what it answers rather
        // than what it takes.
        let error = rejects_body("  let d = Duration.millis(1s)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `Duration`");
        let error = rejects_body("  let n: Int = Duration.seconds(1)");
        assert_eq!(error.message, "expected `Int`, found `Duration`");
        let error = rejects_body("  let d = 1s\n  let n: Duration = d.seconds()");
        assert_eq!(error.message, "expected `Duration`, found `Int`");
    }

    /// A unit no literal suffix names is not a unit, in either direction.
    #[test]
    fn a_duration_has_only_the_units_a_literal_is_written_in() {
        let error = rejects_body("  let d = Duration.weeks(1)");
        assert_eq!(error.code, UNKNOWN_ASSOCIATED);
        assert_eq!(
            error.message,
            "`Duration` has no associated function `weeks`"
        );
        let error = rejects_body("  let d = 1s\n  let n = d.weeks()");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Duration` has no method `weeks`");
        assert_eq!(
            error.help.unwrap(),
            "`Duration` has `nanos`, `micros`, `millis`, `seconds`, `minutes`, `hours`, `snapshot`"
        );
    }

    /// A receiver that is not a sequence has none of the four.
    #[test]
    fn only_a_sequence_walks_with_a_closure() {
        let error = rejects_body("  let ages = Set.of(1, 2)\n  let n = ages.map(fn(v) { v })");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Set` has no method `map`");
    }

    /// A receiver that reports no element count is told what it does have
    /// instead, because `count()` teaches nothing about an `Option`.
    #[test]
    fn a_receiver_that_has_no_length_is_not_taught_the_spelling() {
        let error = rejects_body("  let value = Some(1)\n  let n = value.count()");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Option` has no method `count`");
    }

    #[test]
    fn rejects_a_builtin_method_that_does_not_exist() {
        let error = rejects_body("  println(\"{\"text\".scream()}\")?");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`String` has no method `scream`");
        assert_eq!(
            error.help.unwrap(),
            "`String` has `length`, `isEmpty`, `words`, `chars`, `split`, `join`, `slice`, \
             `trim`, `contains`, `startsWith`, `endsWith`, `indexOf`, `replace`, `toUpper`, \
             `toLower`, `snapshot`"
        );
    }

    /// The help lists what the shared table declares, all of it, in the
    /// order the table declares it. The hand-written candidate list it
    /// replaced had never gained `mapError`, so a `Result` used to be told
    /// it had two methods when it has four; `unwrapOr` reads between the
    /// queries and `mapError` here because that is where `Option` puts it.
    #[test]
    fn the_methods_a_diagnostic_lists_are_the_ones_the_table_declares() {
        let error = rejects_body("  let outcome = Ok(1)\n  println(\"{outcome.unwrap()}\")?");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Result` has no method `unwrap`");
        assert_eq!(
            error.help.unwrap(),
            "`Result` has `isOk`, `isError`, `unwrapOr`, `mapError`"
        );
    }

    /// `lock` is a `Shared`'s only operation, and the help says so rather
    /// than claiming a `Shared` has no methods at all.
    #[test]
    fn a_shared_is_told_that_lock_is_what_it_has() {
        let error = rejects_body("  let counts = Shared(1)\n  let value = counts.get()");
        assert_eq!(error.help.unwrap(), "`Shared` has `lock`");
    }

    /// The rule sentence reads the associated functions out of the table
    /// rather than restating them, so a new one cannot go unmentioned.
    #[test]
    fn an_unknown_associated_function_names_the_ones_that_exist() {
        let error = rejects_body("  let items = Array.of(1)");
        assert_eq!(error.code, UNKNOWN_ASSOCIATED);
        assert_eq!(error.message, "`Array` has no associated function `of`");
        assert_eq!(
            error.rule.unwrap(),
            "A builtin type's associated functions are `Vector.of`, `Map.of`, `Set.of`, `String.fromCodePoint`, `Int.parse`, `Int.parseRadix`, `Float.parse`, `Duration.nanos`, `Duration.micros`, `Duration.millis`, `Duration.seconds`, `Duration.minutes`, and `Duration.hours`."
        );
    }

    // ------------------------------------------------------ calls

    #[test]
    fn checks_an_argument_against_its_parameter() {
        let error = rejects(
            "\
fn greet(name: String) -> String {
  name
}

fn run() -> String {
  greet(42)
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
        assert_eq!(error.labels[0].message, "the parameter `name` is `String`");
        assert_eq!(
            error.help.unwrap(),
            "interpolate the `Int`, as in \"{value}\", to make a `String`"
        );
    }

    #[test]
    fn rejects_too_many_arguments() {
        let error = rejects(
            "\
fn greet(name: String) -> String {
  name
}

fn run() -> String {
  greet(\"a\", \"b\")
}
",
        );
        assert_eq!(error.code, ARITY);
        assert_eq!(
            error.message,
            "`greet` takes 1 argument(s), but more were given"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A call passes exactly the arguments the declaration binds."
        );
        assert_eq!(error.help.unwrap(), "`greet` declares `name`");
    }

    #[test]
    fn rejects_a_label_that_names_no_parameter() {
        let error = rejects(
            "\
fn between(low: Int, high: Int) -> Int {
  high - low
}

fn run() -> Int {
  between(low: 1, top: 2)
}
",
        );
        assert_eq!(error.code, UNKNOWN_LABEL);
        assert_eq!(error.message, "`between` has no parameter labeled `top`");
        assert_eq!(
            error.rule.unwrap(),
            "Argument labels are parameter names and part of the API contract."
        );
        assert_eq!(error.help.unwrap(), "known labels: `low`, `high`");
    }

    #[test]
    fn labels_bind_arguments_to_the_parameters_they_name() {
        accepts(
            "\
fn between(low: Int, high: Int) -> Int {
  high - low
}

fn run() -> Int {
  between(low: 1, high: 2)
}
",
        );
    }

    #[test]
    fn rejects_labels_that_stand_out_of_declaration_order() {
        let error = rejects(
            "\
fn between(low: Int, high: Int) -> Int {
  high - low
}

fn run() -> Int {
  between(high: 2, low: 1)
}
",
        );
        assert_eq!(error.code, LABEL_ORDER);
        assert_eq!(
            error.message,
            "`between` was given the label `low` out of declaration order"
        );
        assert_eq!(
            error.rule.unwrap(),
            "Labeled arguments appear in declaration order, so argument order matches parameter order."
        );
        assert_eq!(
            error.help.unwrap(),
            "write the arguments in this order: low, high"
        );
    }

    /// A struct's synthesized initializer takes its labels in declaration
    /// order exactly as a declared function does, which is the half of this
    /// rule a reader is most likely to meet.
    #[test]
    fn rejects_a_struct_initializer_whose_labels_are_out_of_order() {
        let error = rejects(
            "\
struct Point {
  x: Int
  y: Int
}

fn run() -> Point {
  Point(y: 20, x: 10)
}
",
        );
        assert_eq!(error.code, LABEL_ORDER);
        assert_eq!(
            error.message,
            "`Point` was given the label `x` out of declaration order"
        );
    }

    /// The same label twice is reported once, as the parameter it left
    /// unfilled, rather than twice.
    #[test]
    fn a_label_written_twice_is_one_diagnostic() {
        let error = rejects(
            "\
fn between(low: Int, high: Int) -> Int {
  high - low
}

fn run() -> Int {
  between(low: 1, low: 2)
}
",
        );
        assert_eq!(error.code, MISSING_ARGUMENT);
    }

    // ------------------------------------------------------------- places

    // A place is a name a body bound, or a field of one, and `let` makes a
    // read-only place. ADR 0021 is why these are here rather than at run
    // time; the wording is the interpreter's, because it is what a person
    // reading Cove errors has always seen.

    #[test]
    fn rejects_an_assignment_to_a_let_binding() {
        let error = rejects("fn run() -> Int {\n  let x = 1\n  x = 2\n  x\n}\n");
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "cannot assign to `x`, which is a read-only place"
        );
        assert_eq!(
            error.rule.unwrap(),
            "`let` creates a read-only place; `var` creates a mutable place."
        );
        assert_eq!(
            error.help.unwrap(),
            "declare it with `var x` to make it assignable"
        );
    }

    /// An ordinary parameter is a read-only place too: it receives a shallow
    /// copy, and only `var` names the caller's own storage.
    #[test]
    fn rejects_an_assignment_to_a_parameter_that_is_not_var() {
        let error = rejects("fn run(n: Int) -> Int {\n  n = 2\n  n\n}\n");
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "cannot assign to `n`, which is a read-only place"
        );
    }

    /// A field of a read-only place is a read-only place: the walk asks the
    /// root and a field inherits its answer.
    #[test]
    fn rejects_an_assignment_to_a_field_of_a_let_binding() {
        let error = rejects(
            "struct P {\n  x: Int\n}\n\nfn run() -> Int {\n  let p = P(x: 1)\n  p.x = 2\n  p.x\n}\n",
        );
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "cannot assign to `p.x`, which is a read-only place"
        );
    }

    /// A `var` binding, a `var` parameter, and a field of either are the
    /// places source may write.
    #[test]
    fn accepts_a_write_to_a_var_binding_and_to_its_fields() {
        accepts(
            "struct P {\n  x: Int\n}\n\nfn bump(var n: Int) {\n  n += 1\n}\n\nfn run() -> Int {\n  var p = P(x: 1)\n  p.x = 2\n  var n = 0\n  n += 1\n  bump(var n)\n  p.x + n\n}\n",
        );
    }

    /// A closure holds a *copy* of what it captured, so a captured `var` is
    /// a read-only place inside it — which is the `Place::binding(value,
    /// false)` `Env::declare_capture` builds, read here instead.
    #[test]
    fn rejects_an_assignment_to_a_captured_var_binding() {
        let error = rejects(
            "fn run() -> Int {\n  var count = 0\n  let bump = fn() {\n    count = count + 1\n  }\n  count\n}\n",
        );
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "cannot assign to `count`, which is a read-only place"
        );
    }

    /// A local `fn` is built as a closure too, so the same rule reaches it.
    #[test]
    fn rejects_an_assignment_to_a_var_captured_by_a_local_fn() {
        let error = rejects(
            "fn run() -> Int {\n  var count = 0\n  fn bump() {\n    count = count + 1\n  }\n  count\n}\n",
        );
        assert_eq!(error.code, READ_ONLY_PLACE);
    }

    #[test]
    fn rejects_a_var_argument_that_is_a_read_only_place() {
        let error = rejects(
            "fn bump(var n: Int) {\n  n += 1\n}\n\nfn run() -> Int {\n  let total = 1\n  bump(var total)\n  total\n}\n",
        );
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "`total` is a read-only place, so it cannot be passed as `var`"
        );
    }

    #[test]
    fn rejects_a_var_argument_that_is_not_a_place() {
        let error = rejects(
            "fn bump(var n: Int) {\n  n += 1\n}\n\nfn run() -> Int {\n  bump(var 1 + 2)\n  0\n}\n",
        );
        assert_eq!(error.code, NOT_A_PLACE);
        assert_eq!(
            error.message,
            "this expression is not a place, so it cannot be assigned or aliased"
        );
        assert_eq!(
            error.rule.unwrap(),
            "Only variables and their struct fields are places."
        );
    }

    #[test]
    fn rejects_push_on_a_read_only_place() {
        let error = rejects(
            "fn run() -> Int {\n  let items = Vector.of(1)\n  items.push(2)\n  items.length()\n}\n",
        );
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "`push` takes a `var self` receiver, but `items` is a read-only place"
        );
        assert_eq!(error.help.unwrap(), "declare it with `var items`");
    }

    #[test]
    fn rejects_push_on_a_receiver_that_is_not_a_place() {
        let error = rejects("fn run() -> () {\n  Vector.of(1).push(2)\n}\n");
        assert_eq!(error.code, NOT_A_PLACE);
        assert_eq!(
            error.message,
            "`push` takes a `var self` receiver, but `this expression` is not a place"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A mutating receiver declares `var self` and mutates the caller's place."
        );
    }

    /// `freeze` is the one mutating builtin that tolerates a receiver which
    /// is no place at all: a temporary holds the only handle to its own
    /// storage, so freezing it answers from the temporary. It still needs a
    /// *writable* place when it has one.
    #[test]
    fn freeze_needs_a_writable_place_only_when_it_has_one() {
        accepts("fn run() -> Int {\n  Vector.of(1).freeze().length()\n}\n");
        let error =
            rejects("fn run() -> Int {\n  let v = Vector.of(1)\n  v.freeze().length()\n}\n");
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "`freeze` takes a `var self` receiver, but `v` is a read-only place"
        );
    }

    /// A declared `var self` method is the same question asked of a
    /// declaration rather than of a builtin.
    #[test]
    fn rejects_a_var_self_method_on_a_read_only_place() {
        let error = rejects(
            "struct Counter {\n  value: Int\n}\n\nimpl Counter {\n  fn bump(var self) {\n    self.value += 1\n  }\n}\n\nfn run() -> Int {\n  let counter = Counter(value: 1)\n  counter.bump()\n  counter.value\n}\n",
        );
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "`bump` takes a `var self` receiver, but `counter` is a read-only place"
        );
    }

    /// A `lock` closure that does not declare `var` receives a copy, and the
    /// `var self` method that would change it is refused, because a copy is
    /// not the place the value lives in.
    #[test]
    fn rejects_a_mutating_method_on_a_lock_closures_copy() {
        let source = "struct Counter {\n  value: Int\n}\n\nimpl Counter {\n  fn bump(var self) {\n    self.value += 1\n  }\n}\n\nfn run() -> () {\n  let shared = Shared(Counter(value: 0))\n  shared.lock(fn(value) {\n    value.bump()\n  })\n}\n";
        let error = rejects(source);
        assert_eq!(error.code, READ_ONLY_PLACE);
        assert_eq!(
            error.message,
            "`bump` takes a `var self` receiver, but `value` is a read-only place"
        );
        accepts(&source.replace("fn(value)", "fn(var value)"));
    }

    /// A receiver whose type nothing settled is left alone. The interpreter
    /// reaches a host resource's own operations before it reaches any of
    /// this, so a name that means `push` there might not mean it here.
    #[test]
    fn abstains_about_a_mutating_method_on_an_unknown_receiver() {
        accepts(
            "use unknownhost.open\n\nfn run() -> () {\n  let handle = open()\n  handle.push(1)\n}\n",
        );
    }

    #[test]
    fn a_parameter_with_a_default_may_be_omitted() {
        accepts(
            "\
fn measure(value: Int, unit: String = \"m\") -> String {
  \"{value}{unit}\"
}

fn run() -> String {
  measure(3)
}
",
        );
        let error = rejects(
            "\
fn measure(value: Int, unit: String = 1) -> String {
  \"{value}{unit}\"
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn checks_a_variadic_parameter_and_its_spread() {
        accepts(
            "\
fn joinAll(separator: String, items: String...) -> Int {
  items.length()
}

fn run() -> Int {
  let ready = [\"x\"]
  joinAll(\"-\", \"a\", ...ready)
}
",
        );
        let error = rejects(
            "\
fn joinAll(items: String...) -> Int {
  items.length()
}

fn run() -> Int {
  joinAll(\"a\", 2)
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn a_declaration_parameter_without_a_type_is_refused() {
        // ADR 0004: a declaration's parameters are written, not inferred.
        // `x` has no expected type to fall back on the way a lambda's would.
        let error = rejects(
            "\
fn double(x) -> Int {
  x + x
}
",
        );
        assert_eq!(error.code, MISSING_PARAMETER_TYPE);
        assert_eq!(error.message, "parameter `x` has no declared type");
        assert_eq!(
            error.rule.unwrap(),
            "A declaration's parameters are written: only a lambda's infer, from the expected type at its call site."
        );
        assert_eq!(error.help.unwrap(), "write `x: <type>`");
    }

    #[test]
    fn a_lambda_parameter_without_a_type_still_infers() {
        // The same `Param` node, and the same missing `: Type`, is not
        // refused here: a lambda has an expected type to infer from, and
        // `checks_a_lambda_...` tests already cover it, but this pins the
        // point next to the declaration it is not: a lambda passed where no
        // type is expected still checks, inferring nothing about `n` rather
        // than reporting `MISSING_PARAMETER_TYPE`.
        accepts_body("  let double = fn(n) { n + n }\n  println(\"{double(1)}\")?");
    }

    #[test]
    fn rejects_a_spread_of_the_wrong_element_type() {
        let error = rejects(
            "\
fn joinAll(items: String...) -> Int {
  items.length()
}

fn run() -> Int {
  joinAll(...[1, 2])
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
        assert_eq!(
            error.rule.unwrap(),
            "A variadic parameter is an `Array<T>`; every spread element is a `T`."
        );
        assert_eq!(error.help.unwrap(), "spread a sequence of `String`");
    }

    #[test]
    fn rejects_calling_something_that_is_not_a_function() {
        let error = rejects_body("  let n = 1\n  println(\"{n(2)}\")?");
        assert_eq!(error.code, NOT_CALLABLE);
        assert_eq!(error.message, "`Int` is not a function");
        assert_eq!(error.rule.unwrap(), "Only a function value can be called.");
    }

    #[test]
    fn rejects_calling_an_enum_rather_than_a_case() {
        let error = rejects(
            "\
enum Status {
  Pending
}

fn run() -> Status {
  Status(1)
}
",
        );
        assert_eq!(error.code, NOT_CALLABLE);
        assert_eq!(error.message, "`Status` is an enum, not a function");
        assert_eq!(error.help.unwrap(), "name a case, such as `Status.Pending`");
    }

    // -------------------------------------------------------- traits

    /// The trait, two conforming types, and one that does not conform, which
    /// every trait test below builds on.
    const TRAITS: &str = "\
/// Renders itself.
trait Display {
  /// The full form.
  fn describe(self) -> String

  /// A short form, defaulting to the full one.
  fn label(self) -> String { self.describe() }
}

/// A booking.
struct Booking(id: Int)

/// A receipt.
struct Receipt(total: Int)

/// Conforms to nothing.
struct Ticket(seat: Int)

impl Display for Booking {
  fn describe(self) -> String { \"booking\" }
  fn label(self) -> String { \"#\" }
}

impl Display for Receipt {
  fn describe(self) -> String { \"receipt\" }
}
";

    fn with_traits(source: &str) -> String {
        format!("{TRAITS}\n{source}")
    }

    #[track_caller]
    fn accepts_with_traits(source: &str) {
        accepts(&with_traits(source));
    }

    #[track_caller]
    fn rejects_with_traits(source: &str) -> Diagnostic {
        rejects(&with_traits(source))
    }

    #[test]
    fn a_bound_makes_the_trait_s_methods_callable_on_a_type_parameter() {
        accepts_with_traits(
            "fn render<T: Display>(value: T) -> String {\n  \"{value.label()}: {value.describe()}\"\n}\n\nfn run() -> String {\n  render(Booking(id: 1))\n}\n",
        );
    }

    #[test]
    fn rejects_a_type_argument_that_does_not_conform_to_the_bound() {
        let error = rejects_with_traits(
            "fn render<T: Display>(value: T) -> String {\n  value.describe()\n}\n\nfn run() -> String {\n  render(Ticket(seat: 1))\n}\n",
        );
        assert_eq!(error.code, UNSATISFIED_BOUND);
        assert_eq!(error.message, "`Ticket` does not conform to `Display`");
        assert_eq!(error.labels[0].message, "`render` requires `T: Display`");
        assert_eq!(
            error.help.as_deref(),
            Some("write `impl Display for Ticket { ... }`")
        );
    }

    #[test]
    fn several_bounds_are_all_checked_and_all_searched_for_a_method() {
        let source = "\
/// Names itself.
trait Named {
  /// The name.
  fn name(self) -> String
}

/// Weighs itself.
trait Weighed {
  /// The weight.
  fn weight(self) -> Int
}

/// A crate.
struct Crate(label: String, kilos: Int)

/// A pebble, which is named but not weighed.
struct Pebble(label: String)

impl Named for Crate {
  fn name(self) -> String { self.label }
}

impl Weighed for Crate {
  fn weight(self) -> Int { self.kilos }
}

impl Named for Pebble {
  fn name(self) -> String { self.label }
}

fn tag<T: Named + Weighed>(item: T) -> String {
  \"{item.name()}({item.weight()})\"
}

fn ok() -> String {
  tag(Crate(label: \"a\", kilos: 3))
}
";
        accepts(source);
        let error = rejects(&format!(
            "{source}\nfn bad() -> String {{\n  tag(Pebble(label: \"b\"))\n}}\n"
        ));
        assert_eq!(error.code, UNSATISFIED_BOUND);
        assert_eq!(error.message, "`Pebble` does not conform to `Weighed`");
    }

    #[test]
    fn rejects_a_method_call_on_an_unbounded_type_parameter() {
        let error =
            rejects_with_traits("fn render<T>(value: T) -> String {\n  value.describe()\n}\n");
        assert_eq!(error.code, UNBOUNDED_PARAMETER);
        assert_eq!(
            error.message,
            "`T` has no bound, so it has no method `describe`"
        );
    }

    #[test]
    fn rejects_a_method_no_bound_of_the_parameter_declares() {
        let error =
            rejects_with_traits("fn render<T: Display>(value: T) -> Int {\n  value.total()\n}\n");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(
            error.message,
            "no trait `T` is bounded by declares a method `total`"
        );
    }

    #[test]
    fn one_bounded_function_may_call_another() {
        accepts_with_traits(
            "fn render<T: Display>(value: T) -> String {\n  value.describe()\n}\n\nfn shout<U: Display>(value: U) -> String {\n  render(value)\n}\n",
        );
    }

    #[test]
    fn a_conforming_value_is_accepted_where_dyn_is_expected() {
        accepts_with_traits(
            "fn show(value: dyn Display) -> String {\n  value.describe()\n}\n\nfn run() -> String {\n  show(Booking(id: 1))\n}\n",
        );
    }

    #[test]
    fn rejects_a_value_that_does_not_conform_where_dyn_is_expected() {
        let error = rejects_with_traits(
            "fn show(value: dyn Display) -> String {\n  value.describe()\n}\n\nfn run() -> String {\n  show(Ticket(seat: 1))\n}\n",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "`Ticket` does not conform to `Display`, so it is not a `dyn Display`"
        );
    }

    #[test]
    fn an_array_of_dyn_mixes_conforming_types_element_by_element() {
        // The conversion applies to each element on its own; the array type
        // itself is invariant, so `Array<Booking>` is still not an
        // `Array<dyn Display>`.
        accepts_with_traits(
            "fn run() -> Array<dyn Display> {\n  [Booking(id: 1), Receipt(total: 2)]\n}\n",
        );
        let error = rejects_with_traits(
            "fn run(bookings: Array<Booking>) -> Array<dyn Display> {\n  bookings\n}\n",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "expected `Array<dyn Display>`, found `Array<Booking>`"
        );
    }

    #[test]
    fn dyn_is_not_a_type_parameter_and_satisfies_no_bound() {
        let error = rejects_with_traits(
            "fn render<T: Display>(value: T) -> String {\n  value.describe()\n}\n\nfn run(value: dyn Display) -> String {\n  render(value)\n}\n",
        );
        assert_eq!(error.code, UNSATISFIED_BOUND);
        assert_eq!(
            error.message,
            "`dyn Display` cannot be used as a type argument"
        );
    }

    #[test]
    fn a_dyn_value_does_not_convert_back_to_its_concrete_type() {
        let error = rejects_with_traits("fn run(value: dyn Display) -> Booking {\n  value\n}\n");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Booking`, found `dyn Display`");
    }

    #[test]
    fn only_the_trait_s_methods_are_reachable_through_dyn() {
        let source = format!(
            "{TRAITS}\nimpl Booking {{\n  /// The identifier.\n  fn id(self) -> Int {{ self.id }}\n}}\n\nfn run(value: dyn Display) -> Int {{\n  value.id()\n}}\n"
        );
        let error = rejects(&source);
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Display` has no method `id`");
        assert_eq!(
            error.help.as_deref(),
            Some("`Display` declares `describe`, `label`")
        );
    }

    #[test]
    fn an_associated_function_is_not_callable_through_dyn() {
        let source = "\
/// Renders itself.
trait Display {
  /// The full form.
  fn describe(self) -> String

  /// Builds one.
  fn blank() -> Int
}

/// A booking.
struct Booking(id: Int)

impl Display for Booking {
  fn describe(self) -> String { \"booking\" }
  fn blank() -> Int { 0 }
}

fn run(value: dyn Display) -> Int {
  value.blank()
}
";
        let error = rejects(source);
        assert_eq!(error.code, DYN_ASSOCIATED);
        assert_eq!(
            error.message,
            "`Display.blank` takes no `self`, so it cannot be called through `dyn Display`"
        );
    }

    #[test]
    fn a_mutating_method_is_not_callable_through_dyn() {
        let source = "\
/// Counts.
trait Bump {
  /// Adds one.
  fn bump(var self)
}

/// A counter.
struct Counter(hits: Int)

impl Bump for Counter {
  fn bump(var self) { self.hits += 1 }
}

fn run(var value: dyn Bump) {
  value.bump()
}
";
        let error = rejects(source);
        assert_eq!(error.code, DYN_MUTATING);
        assert_eq!(
            error.message,
            "`Bump.bump` takes `var self`, so it cannot be called through `dyn Bump`"
        );
    }

    #[test]
    fn a_mutating_method_is_callable_through_a_bound() {
        // Through a bound the receiver is still the caller's own place, so
        // the restriction that `dyn` imposes does not apply.
        accepts(
            "\
/// Counts.
trait Bump {
  /// Adds one.
  fn bump(var self)
}

/// A counter.
struct Counter(hits: Int)

impl Bump for Counter {
  fn bump(var self) { self.hits += 1 }
}

fn run<T: Bump>(var value: T) {
  value.bump()
}
",
        );
    }

    #[test]
    fn a_trait_method_call_is_checked_against_the_trait_s_signature() {
        let error = rejects_with_traits(
            "fn render<T: Display>(value: T) -> String {\n  value.describe(1)\n}\n",
        );
        assert_eq!(error.code, ARITY);
    }

    #[test]
    fn rejects_a_conformance_whose_method_has_the_wrong_signature() {
        let source = "\
/// Renders itself.
trait Display {
  /// The full form.
  fn describe(self) -> String
}

/// A booking.
struct Booking(id: Int)

impl Display for Booking {
  fn describe(self) -> Int { 1 }
}
";
        let error = rejects(source);
        assert_eq!(error.code, CONFORMANCE_SIGNATURE);
        assert_eq!(
            error.message,
            "`Booking.describe` does not match the signature `Display` declares: it returns `Int`, not `String`"
        );
        assert_eq!(
            error.help.as_deref(),
            Some("write `fn describe(self) -> String`")
        );
    }

    #[test]
    fn a_default_body_sees_its_trait_and_nothing_of_the_conforming_type() {
        // Checked once against `Self: Summary`, not once per conformance, so
        // a default body cannot reach a conforming type's fields even when
        // every implementor happens to have one by that name.
        let source = "\
/// Renders itself.
trait Summary {
  /// The tag.
  fn tag(self) -> Int

  /// A line, which reaches for a field no trait declares.
  fn line(self) -> String { \"{self.id}\" }
}

/// A booking.
struct Booking(id: Int)

impl Summary for Booking {
  fn tag(self) -> Int { self.id }
}
";
        let error = rejects(source);
        assert_eq!(error.code, UNKNOWN_FIELD);
        assert_eq!(error.message, "`Self` has no field `id`");
    }

    #[test]
    fn a_default_body_may_call_the_trait_s_own_methods() {
        accepts_with_traits("fn run(value: Booking) -> String {\n  value.label()\n}\n");
    }

    #[test]
    fn a_default_body_is_reported_once_however_many_types_conform() {
        let source = "\
/// Renders itself.
trait Summary {
  /// The tag.
  fn tag(self) -> Int

  /// A line whose body does not type-check.
  fn line(self) -> String { self.tag() }
}

/// A booking.
struct Booking(id: Int)

/// A receipt.
struct Receipt(cents: Int)

impl Summary for Booking {
  fn tag(self) -> Int { self.id }
}

impl Summary for Receipt {
  fn tag(self) -> Int { self.cents }
}
";
        let error = rejects(source);
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn rejects_a_dyn_or_a_bound_that_names_no_trait() {
        let error = rejects("fn run(value: dyn Missing) {\n}\n");
        assert_eq!(error.code, UNKNOWN_TRAIT);
        let error = rejects("fn run<T: Missing>(value: T) {\n}\n");
        assert_eq!(error.code, UNKNOWN_TRAIT);
    }

    #[test]
    fn rejects_a_bound_where_the_mvp_never_checks_one() {
        let source = with_traits("struct Box<T: Display>(value: T)\n");
        let error = rejects(&source);
        assert_eq!(error.code, UNSUPPORTED_BOUND);
        assert_eq!(
            error.message,
            "a bound on a struct's type parameter is not checked in the MVP"
        );
    }

    // ------------------------------------------------------ generics

    #[test]
    fn unifies_a_type_parameter_at_the_call_site() {
        accepts(
            "\
fn first<T>(items: Array<T>, fallback: T) -> T {
  items.get(0).unwrapOr(fallback)
}

fn run() -> Int {
  first([1, 2], 0)
}
",
        );
    }

    #[test]
    fn rejects_a_type_parameter_used_at_two_types() {
        let error = rejects(
            "\
fn pair<T>(left: T, right: T) -> T {
  left
}

fn run() -> Int {
  pair(1, \"two\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn substitutes_a_type_parameter_into_the_result() {
        let error = rejects(
            "\
fn identity<T>(value: T) -> T {
  value
}

fn run() -> String {
  identity(1)
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn checks_a_generic_struct_s_fields_through_its_arguments() {
        accepts(
            "\
struct Box<T> { value: T }

fn unwrap(box: Box<Int>) -> Int {
  box.value
}
",
        );
        let error = rejects(
            "\
struct Box<T> { value: T }

fn unwrap(box: Box<String>) -> Int {
  box.value
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn a_generic_enum_takes_its_arguments_from_its_payload() {
        accepts(
            "\
enum Slot<T> {
  Full(T)
  Empty
}

fn run() -> Slot<Int> {
  Slot.Full(1)
}
",
        );
        let error = rejects(
            "\
enum Slot<T> {
  Full(T)
  Empty
}

fn run() -> Slot<Int> {
  Slot.Full(\"one\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Slot<Int>`, found `Slot<String>`");
    }

    #[test]
    fn a_generic_type_s_method_sees_its_arguments() {
        accepts(
            "\
struct Slot<T> { value: T }

impl Slot {
  fn get(self) -> T {
    self.value
  }
}

fn run(slot: Slot<Int>) -> Int {
  slot.get()
}
",
        );
        let error = rejects(
            "\
struct Slot<T> { value: T }

impl Slot {
  fn get(self) -> T {
    self.value
  }
}

fn run(slot: Slot<String>) -> Int {
  slot.get()
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn rejects_the_wrong_number_of_type_arguments() {
        let error = rejects("fn run(items: Array<Int, String>) -> Int {\n  1\n}\n");
        assert_eq!(error.code, TYPE_ARGUMENTS);
        assert_eq!(
            error.message,
            "`Array` takes 1 type argument(s), but 2 were written"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A generic type is written with exactly the arguments its declaration binds."
        );
        assert_eq!(error.help.unwrap(), "write `Array<_>`");
    }

    // ------------------------------------------------------ lambdas

    #[test]
    fn a_lambda_takes_its_parameter_types_from_the_expected_type() {
        accepts(
            "\
fn apply(value: Int, transform: fn(Int) -> Int) -> Int {
  transform(value)
}

fn run() -> Int {
  apply(5, fn(n) { n + 1 })
}
",
        );
    }

    #[test]
    fn rejects_a_lambda_whose_result_does_not_fit() {
        let error = rejects(
            "\
fn apply(value: Int, transform: fn(Int) -> Int) -> Int {
  transform(value)
}

fn run() -> Int {
  apply(5, fn(n) { \"{n}\" })
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn rejects_a_lambda_with_the_wrong_number_of_parameters() {
        let error = rejects(
            "\
fn apply(transform: fn(Int) -> Int) -> Int {
  transform(1)
}

fn run() -> Int {
  apply(fn(a, b) { a })
}
",
        );
        assert_eq!(error.code, ARITY);
        assert_eq!(
            error.message,
            "this function takes 2 parameter(s), but 1 were expected here"
        );
        assert_eq!(error.help.unwrap(), "write `fn(p0) { ... }`");
    }

    #[test]
    fn a_lambda_with_no_expected_type_infers_nothing_about_its_parameters() {
        // Nothing says what `n` is, so the checker abstains rather than
        // guessing, and the body is still walked. The gap is a warning of
        // its own, pinned with the other unknowns.
        accepts_body("  let double = fn(n) { n * 2 }\n  println(\"{double(4)}\")?");
    }

    #[test]
    fn checks_a_function_value_s_arguments() {
        let error = rejects(
            "\
fn apply(transform: fn(Int) -> Int) -> Int {
  transform(\"one\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    // ------------------------------------------------- operators

    #[test]
    fn rejects_mixed_arithmetic() {
        let error = rejects_body("  println(\"{1 + 1.0}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `Int` and `Float`");
        assert_eq!(
            error.rule.unwrap(),
            "There are no implicit numeric, string, or boolean conversions."
        );
        assert_eq!(
            error.help.unwrap(),
            "arithmetic combines two values of the same type"
        );
    }

    #[test]
    fn rejects_mixed_equality() {
        let error = rejects_body("  println(\"{1 == \"1\"}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "cannot compare `Int` with `String`");
        assert_eq!(
            error.rule.unwrap(),
            "`==` means value equality between values of the same type."
        );
        assert_eq!(
            error.help.unwrap(),
            "convert one side explicitly so both are `Int`, or compare values that already share a type"
        );
    }

    #[test]
    fn is_compares_the_identity_of_two_vectors() {
        accepts_body(
            "\
  var a = Vector.of(1, 2)
  var b = a
  println(\"{a is b}\")?
",
        );
    }

    #[test]
    fn rejects_is_between_different_types() {
        let error = rejects_body("  println(\"{Vector.of(1) is Vector.of(\"x\")}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(
            error.message,
            "cannot compare the identity of `Vector<Int>` with `Vector<String>`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "`is` compares identity between values of the same type."
        );
    }

    #[test]
    fn rejects_is_on_a_value_type() {
        let error = rejects_body("  println(\"{1 is 1}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "identity is not available for `Int`");
        assert_eq!(
            error.rule.unwrap(),
            "`==` means value equality. Identity, when available, is explicit."
        );
    }

    #[test]
    fn rejects_adding_two_strings() {
        let error = rejects_body("  println(\"{\"a\" + \"b\"}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `String`");
        assert_eq!(
            error.rule.unwrap(),
            "There are no implicit string conversions."
        );
        assert_eq!(
            error.help.unwrap(),
            "use string interpolation, such as \"{left}{right}\""
        );
    }

    #[test]
    fn accepts_duration_arithmetic_and_comparison() {
        accepts_body("  println(\"{1s + 500ms} {1s > 999ms}\")?");
        let error = rejects_body("  println(\"{1s * 2s}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(
            error.message,
            "`*` is not defined for `Duration` and `Duration`"
        );
    }

    #[test]
    fn rejects_negating_a_string() {
        let error = rejects_body("  println(\"{-\"a\"}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`-` is not defined for `String`");
        assert_eq!(
            error.help.unwrap(),
            "`-` negates an `Int`, a `Float`, or a `Duration`"
        );
    }

    #[test]
    fn rejects_a_non_bool_operand_of_and() {
        let error = rejects_body("  println(\"{1 && true}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`&&` is not defined for `Int` and `Bool`");
        assert_eq!(error.help.unwrap(), "`&&` and `||` combine two `Bool`s");
    }

    #[test]
    fn rejects_ordering_two_bools() {
        let error = rejects_body("  println(\"{true < false}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`<` is not defined for `Bool` and `Bool`");
    }

    #[test]
    fn rejects_a_non_bool_condition() {
        let error = rejects_body("  if 1 {\n    println(\"never\")?\n  }");
        assert_eq!(error.code, CONDITION);
        assert_eq!(
            error.message,
            "a condition must be a `Bool`, but found `Int`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "There are no implicit boolean conversions."
        );
        assert_eq!(
            error.help.unwrap(),
            "compare it, as in `value != 0`; a `Int` is not a `Bool`"
        );
    }

    // ------------------------------------------------- control flow

    #[test]
    fn an_if_with_no_else_is_a_statement() {
        accepts_body("  var seen = 0\n  if true {\n    seen = 1\n  }\n  println(\"{seen}\")?");
        // Its value is `()` whatever the branch produces, so binding it and
        // using it as an `Int` is an error.
        let error = rejects_body("  let n: Int = if true { 1 }");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `()`");
    }

    #[test]
    fn if_branches_must_agree() {
        accepts_body("  let n = if true { 1 } else { 2 }\n  println(\"{n}\")?");
        let error = rejects_body("  let n = if true { 1 } else { \"two\" }");
        assert_eq!(error.code, BRANCHES);
        assert_eq!(
            error.message,
            "this branch produces `String`, but the other produces `Int`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "Every branch of an `if` or `match` used as an expression produces the same type."
        );
        assert_eq!(
            error.help.unwrap(),
            "make both branches produce `Int`, or bind them separately"
        );
    }

    #[test]
    fn every_loop_is_unit_and_a_break_operand_is_discarded() {
        // Every loop can reach its end without breaking, and there is
        // nothing at that end to produce but `()`, so the loop is `()` and a
        // `break` operand is checked on its own and its value discarded --
        // the rule an `if` with no `else` already follows. Whether a loop
        // should ever carry a value is issue #87.
        accepts_body("  let ran = for value in [1, 2] {\n    value\n  }\n  println(\"{ran}\")?");
        accepts_body(
            "  let ran = for value in [1, 2] {\n    break value\n  }\n  println(\"{ran}\")?",
        );
        accepts_body(
            "  var seen = 0\n  let ran = while seen < 2 {\n    seen += 1\n    break seen\n  }\n  println(\"{ran}\")?",
        );
        // `while true` is an ordinary `while`: nothing about the condition
        // makes it a form the two passes have to treat specially.
        accepts_body("  let ran = while true {\n    break 1\n  }\n  println(\"{ran}\")?");
        // Two `break`s out of one loop are checked separately, because
        // neither of them says what the loop produces.
        accepts_body(
            "  let ran = while true {\n    if true {\n      break 1\n    }\n    break \"two\"\n  }\n  println(\"{ran}\")?",
        );
        // A binding that asks the loop for anything but `()` is a mismatch,
        // whatever the `break`s carry.
        let error = rejects_body("  let n: Int = while true {\n    break 1\n  }");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `()`");
        let error = rejects_body("  let n: Int = for value in [1, 2] {\n    break value\n  }");
        assert_eq!(error.code, MISMATCH);
        // The operand is still checked, so a mistake inside it is reported.
        let error = rejects_body("  for value in [1, 2] {\n    break value + \"a\"\n  }");
        assert_eq!(error.code, OPERATOR);
    }

    #[test]
    fn match_arms_must_agree() {
        let error = rejects(
            "\
fn name(n: Int) -> String {
  let value = match n {
    0 => \"zero\"
    _ => 1
  }
  \"{value}\"
}
",
        );
        assert_eq!(error.code, BRANCHES);
        assert_eq!(
            error.message,
            "this branch produces `Int`, but the other produces `String`"
        );
    }

    #[test]
    fn a_return_never_disagrees_with_a_branch() {
        accepts(
            "\
fn label(n: Int) -> String {
  match n {
    0 => return \"zero\"
    _ => \"other\"
  }
}
",
        );
    }

    #[test]
    fn checks_return_against_the_declared_return_type() {
        let error = rejects(
            "\
fn label(n: Int) -> String {
  if n == 0 {
    return 0
  }
  \"other\"
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
        assert_eq!(
            error.labels[0].message,
            "the declared return type is `String`"
        );
    }

    #[test]
    fn a_block_s_value_is_its_tail() {
        accepts_body("  let n = {\n    let base = 1\n    base + 1\n  }\n  println(\"{n}\")?");
        accepts_body("  let nothing = { }\n  println(\"{nothing}\")?");
    }

    #[test]
    fn a_function_with_no_return_type_returns_unit() {
        accepts(
            "\
fn record(var log: Vector<String>, entry: String) {
  log.push(entry)
}
",
        );
        let error = rejects("fn total() {\n  1\n}\n");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `()`, found `Int`");
        assert_eq!(
            error.labels[0].message,
            "this function declares no return type, so it returns `()`"
        );
    }

    #[test]
    fn checks_a_for_loop_s_iterable_and_binding() {
        accepts(
            "\
fn total(items: Array<Int>) -> Int {
  var sum = 0
  for item in items {
    sum += item
  }
  sum
}
",
        );
        let error = rejects(
            "\
fn total(items: Array<String>) -> Int {
  var sum = 0
  for item in items {
    sum += item
  }
  sum
}
",
        );
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `Int` and `String`");
    }

    #[test]
    fn rejects_iterating_something_that_is_not_a_sequence() {
        let error = rejects_body("  for n in 1 {\n    println(\"{n}\")?\n  }");
        assert_eq!(error.code, ITERABLE);
        assert_eq!(
            error.message,
            "`for` iterates an `Array`, a `Vector`, a `Range`, a `Set`, or a `Map`, but found `Int`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "`for` iterates a sequence; iteration order is defined by each collection type."
        );
        assert_eq!(error.help.unwrap(), "write a range, as in `0..<n`");
    }

    #[test]
    fn checks_an_assignment_against_the_place_s_type() {
        let error = rejects_body("  var n = 1\n  n = \"one\"");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn checks_a_compound_assignment_with_the_operator_s_rule() {
        accepts_body("  var n = 1\n  n += 2\n  println(\"{n}\")?");
        let error = rejects_body("  var n = 1\n  n += 1.0");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `Int` and `Float`");
    }

    #[test]
    fn a_range_takes_two_ints() {
        accepts_body("  let range = 0..<3\n  println(\"{range.length()}\")?");
        let error = rejects_body("  let range = 0..<\"three\"");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    // ------------------------------------------------- ? and await

    #[test]
    fn checks_the_question_mark_against_the_enclosing_return_type() {
        accepts(
            "\
fn double(text: String) -> Result<Int, Error> {
  let value = Int.parse(text)?
  Ok(value * 2)
}
",
        );
    }

    #[test]
    fn rejects_the_question_mark_on_a_value_that_cannot_fail() {
        let error = rejects(
            "\
fn length(text: String) -> Result<Int, Error> {
  let n = text.length()?
  Ok(n)
}
",
        );
        assert_eq!(error.code, TRY_OPERAND);
        assert_eq!(
            error.message,
            "`?` needs a `Result` or an `Option`, but found `Int`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "`expr?` returns the error from the current function."
        );
        assert_eq!(error.help.unwrap(), "`Int` cannot fail, so drop the `?`");
    }

    #[test]
    fn rejects_the_question_mark_when_the_failure_types_differ() {
        let error = rejects(
            "\
enum ParseError {
  NotANumber
}

fn double(text: String) -> Result<Int, ParseError> {
  let value = Int.parse(text)?
  Ok(value * 2)
}
",
        );
        assert_eq!(error.code, TRY_RETURN);
        assert_eq!(
            error.message,
            "`?` propagates `Error`, but this function returns `ParseError` as its failure"
        );
        assert_eq!(
            error.rule.unwrap(),
            "`expr?` returns the error from the current function, so the two failure types must be the same."
        );
        assert_eq!(
            error.help.unwrap(),
            "map the failure first, as in `expr.mapError { ... }?`, or declare this function `-> Result<_, Error>`"
        );
    }

    #[test]
    fn rejects_the_question_mark_in_a_function_that_cannot_fail() {
        let error = rejects(
            "\
fn double(text: String) -> Int {
  Int.parse(text)? * 2
}
",
        );
        assert_eq!(error.code, TRY_RETURN);
        assert_eq!(
            error.message,
            "`?` needs a function that returns a `Result`, but this one returns `Int`"
        );
        assert_eq!(
            error.help.unwrap(),
            "declare this function `-> Result<Int, Error>`, or handle the `Err` with `unwrapOr`"
        );
    }

    #[test]
    fn the_question_mark_unwraps_an_option_inside_an_option() {
        accepts(
            "\
fn shout(text: String) -> Option<String> {
  let word = text.words().get(0)?
  Some(\"{word}!\")
}
",
        );
        let error = rejects(
            "\
fn shout(text: String) -> String {
  let word = text.words().get(0)?
  \"{word}!\"
}
",
        );
        assert_eq!(error.code, TRY_RETURN);
        assert_eq!(
            error.message,
            "`?` on an `Option` needs a function that returns an `Option`, but this one returns `String`"
        );
    }

    #[test]
    fn map_error_replaces_the_failure_type() {
        accepts(
            "\
enum ParseError {
  NotANumber(String)
}

fn parseOrFail(text: String) -> Result<Int, ParseError> {
  Int.parse(text).mapError { ParseError.NotANumber(text) }
}
",
        );
        let error = rejects(
            "\
enum ParseError {
  NotANumber(String)
}

fn parseOrFail(text: String) -> Result<Int, ParseError> {
  Int.parse(text).mapError { text }
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "expected `Result<Int, ParseError>`, found `Result<Int, String>`"
        );
    }

    #[test]
    fn map_error_also_takes_a_callback_of_the_error() {
        accepts(
            "\
fn keep(text: String) -> Result<Int, Error> {
  Int.parse(text).mapError(fn(error) { error })
}
",
        );
    }

    #[test]
    fn calling_an_async_function_produces_a_task_that_await_settles() {
        accepts(
            "\
async fn load() -> Int {
  1
}

async fn run() -> Int {
  await load()
}
",
        );
        let error = rejects(
            "\
async fn load() -> Int {
  1
}

async fn run() -> Int {
  load()
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `Task<Int>`");
    }

    #[test]
    fn rejects_awaiting_something_that_is_not_a_task() {
        let error = rejects_body("  let n = await 1");
        assert_eq!(error.code, AWAIT_OPERAND);
        assert_eq!(error.message, "`await` needs a task, but found `Int`");
        assert_eq!(
            error.help.unwrap(),
            "call an `async fn`, or spawn the work into a task scope, and await that handle"
        );
    }

    #[test]
    fn rejects_the_question_mark_on_a_task() {
        let error = rejects(
            "\
async fn load() -> Result<Int, Error> {
  Ok(1)
}

async fn run() -> Result<Int, Error> {
  let value = load()?
  Ok(value)
}
",
        );
        assert_eq!(error.code, TRY_OPERAND);
        assert_eq!(
            error.message,
            "`?` needs a `Result` or an `Option`, but found `Task<Result<Int, Error>>`"
        );
        assert_eq!(
            error.help.unwrap(),
            "settle the task first, as in `task.await()?`"
        );
    }

    #[test]
    fn a_scope_spawns_tasks_that_carry_the_block_s_value() {
        accepts(
            "\
async fn run() -> Result<Int, Error> {
  scope tasks {
    let first = tasks.spawn { 1 }
    let value = await first
    Ok(value)
  }
}
",
        );
    }

    // ------------------------------------------------- `Shared`

    /// The Language Card's own example: mutable state wrapped in a `Shared`,
    /// reached through a scoped `lock`.
    const METRICS: &str = "\
struct Metrics {
  requests: Int
  failures: Int
}

impl Metrics {
  fn record(var self, failed: Bool) {
    self.requests += 1
    if failed {
      self.failures += 1
    }
  }
}
";

    #[test]
    fn a_lock_gives_its_closure_the_wrapped_type_and_carries_its_result() {
        accepts(&format!(
            "{METRICS}
fn run() -> Int {{
  let metrics = Shared(Metrics(requests: 0, failures: 0))
  metrics.lock(fn(var value) {{
    value.record(true)
  }})
  metrics.lock(fn(value) {{
    value.requests
  }})
}}
"
        ));
    }

    #[test]
    fn a_lock_result_has_the_closure_s_type() {
        let error = rejects(&format!(
            "{METRICS}
fn run() -> String {{
  let metrics = Shared(Metrics(requests: 0, failures: 0))
  metrics.lock(fn(value) {{
    value.requests
  }})
}}
"
        ));
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    /// The closure's parameter type is derived from what the `Shared` wraps,
    /// so the closure sees that type and nothing else.
    #[test]
    fn a_lock_closure_takes_the_wrapped_type() {
        let error = rejects(&format!(
            "{METRICS}
fn run() -> Int {{
  let metrics = Shared(Metrics(requests: 0, failures: 0))
  metrics.lock(fn(value) {{
    value.attempts
  }})
}}
"
        ));
        assert_eq!(error.code, UNKNOWN_FIELD);
        assert_eq!(error.message, "`Metrics` has no field `attempts`");
    }

    /// A `Shared<Vector<T>>` would let a vector be reached from two tasks,
    /// which is what the sentence naming `Shared` forbids.
    #[test]
    fn a_shared_vector_is_refused_where_the_type_is_written() {
        let error = rejects_body("  let counts: Shared<Vector<Int>> = Shared(Vector.of(1))");
        assert_eq!(error.code, TASK_SAFETY);
        assert_eq!(
            error.message,
            "`Shared` cannot wrap a `Vector<Int>`, which cannot cross a task boundary"
        );
        assert!(error.rule.unwrap().contains("A vector cannot cross"));
    }

    #[test]
    fn a_shared_vector_is_refused_where_it_is_constructed() {
        let error = rejects_body("  let counts = Shared(Vector.of(1))");
        assert_eq!(error.code, TASK_SAFETY);
    }

    #[test]
    fn a_shared_of_an_array_of_vectors_names_the_vector() {
        let error = rejects_body("  let counts: Shared<Array<Vector<Int>>> = Shared([])");
        assert_eq!(
            error.message,
            "`Shared` cannot wrap `Array<Vector<Int>>`: the `Vector<Int>` in it cannot cross a task boundary"
        );
    }

    #[test]
    fn a_shared_does_not_conform_to_snapshot() {
        let error = rejects_body("  let counts = Shared(1)\n  let copy = counts.snapshot()");
        assert_eq!(error.message, "`Shared<Int>` does not implement `Snapshot`");
        assert!(error.rule.unwrap().contains("synchronized values"));
    }

    #[test]
    fn a_shared_has_no_operation_but_lock() {
        let error = rejects_body("  let counts = Shared(1)\n  let value = counts.get()");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Shared` has no method `get`");
    }

    // ------------------------------------------------- aliases

    #[test]
    fn expands_a_type_alias() {
        accepts(
            "\
type Transform = fn(Int) -> Int

fn apply(value: Int, transform: Transform) -> Int {
  transform(value)
}

fn run() -> Int {
  apply(1, fn(n) { n + 1 })
}
",
        );
        let error = rejects(
            "\
type Transform = fn(Int) -> Int

fn apply(value: Int, transform: Transform) -> Int {
  transform(value)
}

fn run() -> Int {
  apply(1, fn(n) { \"{n}\" })
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn rejects_a_type_alias_that_expands_to_itself() {
        let error = rejects("type Loop = Loop\n\nfn run(value: Loop) {\n}\n");
        assert_eq!(error.code, ALIAS_CYCLE);
        assert_eq!(error.message, "`Loop` expands to itself");
        assert_eq!(
            error.rule.unwrap(),
            "A type alias names an existing type; it cannot be defined in terms of itself."
        );
    }

    // ------------------------------------------------- the Host API schema
    //
    // ADR 0001's schema is one description shared by the compiler, runtime,
    // and CLI, and these are the compiler's half of reading it. The runtime's
    // half is in `cove_runtime::host`; the two check the same table against
    // the same call, and a program that gets past both has been checked
    // twice.

    #[test]
    fn a_host_call_produces_the_type_its_schema_declares() {
        // `env.get` declares `Option<String>` and `documents.read` declares
        // `Result<String, Error>`, so both are ordinary typed values here.
        accepts(
            "\
use console.println
use env.get
use documents

export fn main() -> Result<Unit, Error> {
  let port: String = env.get(\"PORT\").unwrapOr(\"8080\")
  let note: String = documents.read(\"input\")?
  println(\"{port} {note}\")?
  Ok(())
}
",
        );
    }

    #[test]
    fn a_host_call_s_result_is_checked_where_it_is_used() {
        let error = rejects(
            "\
use env.get

export fn main() -> Int {
  get(\"PORT\").unwrapOr(\"8080\") + 1
}
",
        );
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `String` and `Int`");
    }

    #[test]
    fn an_argument_a_host_operation_does_not_declare_is_rejected_at_the_call() {
        let error = rejects(
            "\
use documents

export fn main() -> Result<Unit, Error> {
  documents.read(1)?
  Ok(())
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
        assert_eq!(
            error.rule.unwrap(),
            "Types are nominal and the only implicit conversion is to `dyn Trait`: a value must otherwise already have the type its place asks for."
        );
    }

    /// The same mistake the boundary refuses, caught where it has a span.
    #[test]
    fn a_host_call_with_the_wrong_number_of_arguments_is_rejected_at_the_call() {
        let error = rejects(
            "\
use documents

export fn main() -> Result<Unit, Error> {
  documents.read(\"input\", \"extra\")?
  Ok(())
}
",
        );
        assert_eq!(error.code, ARITY);
        assert_eq!(
            error.message,
            "`documents.read` takes 1 argument, but 2 were given"
        );
        assert_eq!(
            error.help.unwrap(),
            "the Host API schema declares `documents.read(String) -> Result<String, Error>`",
            "the boundary's diagnostic for the same mistake quotes it word for word"
        );
    }

    /// `console.println("a", "b")` is one line of two parts, so a variadic
    /// operation accepts any number of arguments and checks every one of them
    /// against its one declared type.
    #[test]
    fn a_variadic_host_operation_checks_every_argument() {
        accepts(
            "\
use console

export fn main() -> Result<Unit, Error> {
  console.println()?
  console.println(\"one\", \"two\", \"three\")?
  Ok(())
}
",
        );

        let error = rejects(
            "\
use console

export fn main() -> Result<Unit, Error> {
  console.println(\"one\", 2)?
  Ok(())
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    /// `clock.timeout` declares `Any` for the work it bounds, which is not a
    /// gap in the schema but a claim: the operation's meaning does not depend
    /// on which value it was given. `Unknown` is what that claim is here.
    #[test]
    fn a_parameter_declared_any_accepts_whatever_it_is_given() {
        accepts(
            "\
use clock

export fn main() -> Result<Unit, Error> {
  clock.timeout(500ms) {
    1
  }?
  clock.every(60s, fn() {
    Ok(())
  })?
  Ok(())
}
",
        );
    }

    #[test]
    fn an_operation_the_schema_does_not_declare_is_rejected() {
        let error = rejects(
            "\
use documents

export fn main() -> Result<Unit, Error> {
  documents.write(\"input\", \"text\")?
  Ok(())
}
",
        );
        assert_eq!(error.code, UNKNOWN_HOST_OPERATION);
        assert_eq!(
            error.message,
            "host module `documents` has no operation `write`"
        );
        assert_eq!(error.help.unwrap(), "`documents` exposes `read`");
    }

    /// A host type is nominal and named the way source writes it, so a
    /// signature written in terms of one is checked like any other.
    #[test]
    fn a_host_type_is_a_type() {
        accepts(
            "\
use http

/// Answers one request.
export fn health(request: http.Request) -> http.Response {
  http.json(200, request.path)
}
",
        );

        let error = rejects(
            "\
use http

export fn health(request: http.Request) -> Int {
  http.json(200, \"ok\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `http.Response`");
    }

    /// A host type's fields come from the schema, which is the one place they
    /// are read: the boundary checks a declared type by name only.
    #[test]
    fn a_host_type_s_fields_are_typed_by_the_schema() {
        let error = rejects(
            "\
use http

export fn path(request: http.Request) -> Int {
  request.path
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");

        let missing = rejects(
            "\
use http

export fn path(request: http.Request) -> String {
  request.query
}
",
        );
        assert_eq!(missing.code, UNKNOWN_FIELD);
        assert_eq!(missing.message, "`http.Request` has no field `query`");
        assert_eq!(
            missing.help.unwrap(),
            "`http.Request` declares `method`, `path`, `body`"
        );
    }

    /// A host type that is plain data is initialized from Cove source exactly
    /// as a struct is, labels and all.
    #[test]
    fn a_host_type_is_initialized_with_the_fields_the_schema_declares() {
        accepts(
            "\
use http

/// Answers one request.
fn health(request: http.Request) -> http.Response {
  http.json(200, \"ok\")
}

/// The one route this program serves.
export fn routes() -> Array<http.Route> {
  [http.Route(method: http.Method.Get, path: \"/health\", handler: health)]
}
",
        );

        let error = rejects(
            "\
use http

export fn routes() -> Array<http.Route> {
  [http.Route(method: http.Method.Get, path: 8080, handler: 1)]
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn a_case_the_host_enum_does_not_declare_is_rejected() {
        let error = rejects(
            "\
use http

export fn method() -> http.Method {
  http.Method.Delete
}
",
        );
        assert_eq!(error.code, UNKNOWN_CASE);
        assert_eq!(error.message, "`http.Method` has no case `Delete`");
        assert_eq!(error.help.unwrap(), "`http.Method` declares `Get`, `Post`");
    }

    /// A resource handle answers the operations its kind declares, checked
    /// through the same entry the boundary dispatches them through.
    #[test]
    fn an_operation_on_a_host_resource_is_checked_against_its_kind() {
        accepts(
            "\
use http
use console.println

export fn main() -> Result<Unit, Error> {
  let server = http.listen(8080)?
  println(\"listening on :{server.port()}\")?
  server.close()?
  Ok(())
}
",
        );

        let error = rejects(
            "\
use http

export fn main() -> Result<Unit, Error> {
  let server = http.listen(8080)?
  server.handle(\"routes\")?
  Ok(())
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "expected `Array<http.Route>`, found `String`"
        );
    }

    #[test]
    fn an_operation_a_host_resource_does_not_declare_is_rejected() {
        let error = rejects(
            "\
use http

export fn main() -> Result<Unit, Error> {
  let server = http.listen(8080)?
  server.stop()?
  Ok(())
}
",
        );
        assert_eq!(error.code, UNKNOWN_HOST_OPERATION);
        assert_eq!(error.message, "`http.Server` has no operation `stop`");
        assert_eq!(
            error.help.unwrap(),
            "`http.Server` answers `port`, `handle`, `close`"
        );
    }

    #[test]
    fn a_type_a_host_module_does_not_declare_is_rejected() {
        let error = rejects(
            "\
use http

export fn handle(request: http.Payload) -> Int {
  1
}
",
        );
        assert_eq!(error.code, UNKNOWN_HOST_TYPE);
        assert_eq!(
            error.message,
            "host module `http` declares no type `Payload`"
        );
        assert_eq!(
            error.help.unwrap(),
            "`http` declares `Method`, `Request`, `Response`, `Route`, `Server`"
        );
    }

    // --------------------------------------- the four kinds of unknown

    // Each of these pins one classification by its *effect*: a recovery
    // unknown says nothing more, a dynamic boundary warns, an unconstrained
    // result is noted, and a language gap is reported. The module
    // documentation lists which construction site is which; these say what
    // the difference is worth to someone reading `cove check`.

    // ---- dynamic boundary: a host no schema describes

    /// A host module no schema describes is the one host call the checker
    /// still abstains from: an embedder that hands its module's schema over
    /// gets it checked like any other, and one that does not leaves the
    /// boundary to hold the host to its word.
    ///
    /// This pass says nothing about it. The fact belongs to the `use` that
    /// named the module — no edit to `sensors.read` can fix it, and the
    /// remedy is one thing to say however many calls a program makes — and
    /// `cove::resolve::unchecked_host` puts the warning there. What this
    /// pass owes is silence per call and a `Ty::dynamic_boundary` that says
    /// why.
    #[test]
    fn a_call_into_a_host_module_with_no_schema_is_not_reported_at_the_call() {
        let source = "\
use console.println
use sensors

export fn main() -> Result<Unit, Error> {
  let value = sensors.read(\"pressure\")
  println(\"{value + 1}\")?
  Ok(())
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
        assert!(notes_of(source).is_empty());
    }

    /// The shape an embedding is written in: a callback a host stores and
    /// calls later, registered with a module no schema describes.
    ///
    /// Nothing on this side states what the callback takes or produces, and
    /// nothing can: that is the abstention, not a gap in the program. So the
    /// unannotated parameter is not reported and the early `return` is not
    /// an error, either of which would have refused a program that runs.
    #[test]
    fn a_callback_into_a_host_module_with_no_schema_is_not_reported() {
        let source = "\
use sensors

fn run() -> Int {
  sensors.watch(fn(reading) { return 1 })
  1
}
";
        accepts(source);
        assert!(
            warnings_of(source).is_empty(),
            "{:?}",
            warnings_of(source)
                .iter()
                .map(|d| d.code.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_member_of_a_host_module_with_no_schema_read_as_a_value_is_not_reported() {
        let source = "\
use sensors

fn run() -> Int {
  let reading = sensors.latest
  1
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
    }

    #[test]
    fn a_type_from_a_host_module_with_no_schema_warns_rather_than_failing() {
        let warning = warns(
            "\
use sensors

fn handle(reading: sensors.Reading) -> Int {
  1
}
",
        );
        assert_eq!(warning.code, HOST_TYPE);
        assert_eq!(
            warning.message,
            "`sensors.Reading` comes from a host module no Host API schema describes, so values of it are unchecked"
        );
        assert_eq!(
            warning.rule.as_deref().unwrap(),
            "A Host API's types come from its schema; the checker reads the shipped schemas and any an embedder supplies."
        );
    }

    /// A host *operation* is a value, and the schema says which one.
    ///
    /// The interpreter has always bound one and called it later, so refusing
    /// the form would remove a capability the language has. Reading the
    /// schema instead turns what used to be an unknown into the operation's
    /// own function type, so a call made through the value is checked exactly
    /// as a direct call is.
    #[test]
    fn a_host_operation_read_as_a_value_has_the_type_its_schema_declares() {
        let source = "\
use console.println
use http

export fn main() -> Result<Unit, Error> {
  let get = http.fetch
  let body = get(\"https://example.com\")?
  println(\"{body}\")?
  Ok(())
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
        assert!(notes_of(source).is_empty());
    }

    /// The value is a real function type, so a call through it is checked.
    #[test]
    fn a_call_through_a_host_operation_value_is_checked() {
        let error = rejects(
            "\
use http

fn run() -> Int {
  let get = http.fetch
  get(1)
  1
}
",
        );
        assert_eq!(error.code, MISMATCH);
    }

    /// A *variadic* operation is the one this language has no function type
    /// for, so the value keeps working and the gap is a note rather than a
    /// refusal or a silence.
    #[test]
    fn a_variadic_host_operation_used_as_a_value_is_noted() {
        let source = "\
use console

fn run() -> Int {
  let write = console.println
  1
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
        let notes = notes_of(source);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].code, VARIADIC_AS_VALUE);
        assert_eq!(
            notes[0].message,
            "`console.println` is variadic, so this value has no function type here"
        );
    }

    /// A host *type* is not a value, exactly as a bare `Vector` is not, and
    /// the correction names a form the language has.
    #[test]
    fn a_host_type_read_as_a_value_is_an_error() {
        let error = rejects(
            "\
use http

fn run() -> Int {
  let route = http.Route
  1
}
",
        );
        assert_eq!(error.code, NOT_A_VALUE);
        assert_eq!(error.message, "`http.Route` is a host type, not a value");
        assert_eq!(
            error.help.unwrap(),
            "construct one, as in `http.Route(field: value)`, or call the operation that answers one"
        );
    }

    // ---- unconstrained API: a schema's `Any`

    /// `Any` in a result is the checker saying what it will not prove, so it
    /// is a note: nothing is wrong, and `--deny-warnings` has nothing to act
    /// on. What the note has to carry is the schema's own promise.
    #[test]
    fn a_host_result_declared_any_is_noted_at_the_call() {
        let source = "\
use clock

export fn main() -> Result<Unit, Error> {
  let value = clock.timeout(1s) {
    1
  }?
  Ok(())
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
        let notes = notes_of(source);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].code, UNCONSTRAINED_RESULT);
        assert_eq!(
            notes[0].message,
            "`clock.timeout` declares its result `Result<Any, Error>`, so nothing here says what this call produced"
        );
        assert_eq!(
            notes[0].help.as_deref().unwrap(),
            "whatever the program does with the result of `clock.timeout` is checked at run time and by nothing here; the Host API schema declares `clock.timeout(Duration, Any) -> Result<Any, Error>`"
        );
    }

    /// `Any` in a parameter promises to accept every value, so there is no
    /// check to skip and nothing to say. `clock.every` declares one and
    /// answers `Result<Unit, Error>`.
    #[test]
    fn a_parameter_declared_any_is_not_noted() {
        let source = "\
use clock

export fn main() -> Result<Unit, Error> {
  clock.every(1s) {
    1
  }?
  Ok(())
}
";
        accepts(source);
        assert!(notes_of(source).is_empty());
        assert!(warnings_of(source).is_empty());
    }

    /// The type an `Any` result carries is unknown, so what the program does
    /// with it afterwards is unchecked — which is exactly what the note
    /// warns a reader to expect.
    #[test]
    fn what_an_any_result_is_used_for_is_not_checked() {
        accepts(
            "\
use clock

export fn main() -> Result<Unit, Error> {
  let value = clock.timeout(1s) {
    1
  }?
  let text: String = value
  Ok(())
}
",
        );
    }

    /// The other end of the `Any` promise: a schema may declare a *field*
    /// `Any`, and reading one leaves the program holding a value no schema
    /// described, exactly as calling an `Any`-result operation does.
    /// `http.Route.handler` is the one the shipped schema declares.
    #[test]
    fn a_host_field_declared_any_is_noted_where_it_is_read() {
        let source = "\
use http

fn readHandler(route: http.Route) -> Int {
  let handler = route.handler
  1
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
        let notes = notes_of(source);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].code, UNCONSTRAINED_FIELD);
        assert_eq!(
            notes[0].message,
            "`http.Route` declares `handler` as `Any`, so nothing here says what this field holds"
        );
    }

    /// A rejected call produces nothing to say anything about, so the note
    /// about its result is not also printed: one mistake, one diagnostic.
    #[test]
    fn an_arity_error_on_an_any_result_operation_is_not_also_noted() {
        let source = "\
use clock

export fn main() -> Result<Unit, Error> {
  clock.timeout(1s, 2, 3)?
  Ok(())
}
";
        let error = rejects(source);
        assert_eq!(error.code, ARITY);
        assert!(notes_of(source).is_empty());
    }

    // ---- language gap: reported, never silent

    #[test]
    fn a_capitalized_name_no_module_declares_is_an_error() {
        let error = rejects("fn run() -> Int {\n  Sensor(1)\n  1\n}\n");
        assert_eq!(error.code, UNRESOLVED_NAME);
        assert_eq!(error.message, "cannot find `Sensor` in this scope");
        assert_eq!(
            error.help.unwrap(),
            "declare `struct Sensor` or `enum Sensor` in this module, `use <module>.Sensor` to import it, or `use <host>` and write `<host>.Sensor`"
        );
    }

    #[test]
    fn a_lowercase_name_nothing_declares_is_an_error() {
        let error = rejects("fn run() -> Int {\n  total\n}\n");
        assert_eq!(error.code, UNKNOWN_NAME);
        assert_eq!(error.message, "cannot find `total` in this scope");
        assert_eq!(
            error.rule.unwrap(),
            "A name must be a local binding, a parameter, a declaration of this module, or something `use` imports."
        );
        assert_eq!(
            error.help.unwrap(),
            "declare `let total = ...` before this expression, or `use <host>.total`"
        );
    }

    #[test]
    fn an_unknown_type_name_is_an_error() {
        let error = rejects("fn run(value: Missing) -> Int {\n  1\n}\n");
        assert_eq!(error.code, UNKNOWN_TYPE);
        assert_eq!(error.message, "`Missing` names no type this module can see");
    }

    #[test]
    fn a_type_used_as_a_value_is_an_error() {
        let error = rejects(
            "\
struct Counter { hits: Int }

fn run() -> Int {
  Counter
  1
}
",
        );
        assert_eq!(error.code, NOT_A_VALUE);
        assert_eq!(error.message, "`Counter` is a struct, not a value");
        assert_eq!(
            error.help.unwrap(),
            "construct one, as in `Counter(field: value)`, or name a value instead"
        );
    }

    #[test]
    fn a_host_module_used_as_a_value_is_an_error() {
        let error = rejects(
            "\
use console

fn run() -> Int {
  console
  1
}
",
        );
        assert_eq!(error.code, NOT_A_VALUE);
        assert_eq!(error.message, "`console` is a host module, not a value");
    }

    /// The follow-up ADR 0004 left open: a lambda's result comes from its
    /// body's value, so an early `return` produces one where the body's
    /// value is not, and nothing written says what the two must agree on.
    #[test]
    fn a_return_in_a_function_value_nothing_expects_is_an_error() {
        let error = rejects_body("  let double = fn(n: Int) { return n * 2 }\n  double(4)");
        assert_eq!(error.code, LAMBDA_RETURN);
        assert_eq!(
            error.message,
            "this function value uses `return`, but nothing says what it produces"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A `return` is checked against a stated result type: a declaration writes one, and a function value takes one from the place that holds it."
        );
    }

    #[test]
    fn a_return_in_a_function_value_the_place_types_is_checked() {
        accepts_body("  let double: fn(Int) -> Int = fn(n) { return n * 2 }\n  double(4)");
        let error =
            rejects_body("  let double: fn(Int) -> Int = fn(n) { return \"two\" }\n  double(4)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    /// An argument the checker has already abstained about is not a second
    /// place to complain: the abstention was reported where it was made.
    #[test]
    fn a_return_inside_a_body_a_schema_declared_any_is_not_reported() {
        accepts(
            "\
use clock

export fn main() -> Result<Unit, Error> {
  clock.every(1s) {
    return 1
  }?
  Ok(())
}
",
        );
    }

    #[test]
    fn a_lambda_parameter_with_no_expected_type_warns() {
        let warning = warns(&in_main(
            "  let double = fn(n) { n * 2 }\n  println(\"{double(4)}\")?",
        ));
        assert_eq!(warning.code, UNCONSTRAINED);
        assert_eq!(warning.message, "nothing says what `n` is");
        assert_eq!(
            warning.help.unwrap(),
            "write the type, as in `n: <type>`, or give this function value to a place that declares one"
        );
    }

    #[test]
    fn an_empty_array_literal_with_no_expected_type_warns() {
        let warning = warns(&in_main(
            "  let empty = []\n  println(\"{empty.length()} {empty.isEmpty()}\")?",
        ));
        assert_eq!(warning.code, UNCONSTRAINED);
        assert_eq!(warning.message, "nothing says what this empty array holds");
        assert_eq!(
            warning.help.unwrap(),
            "write the type on the place that holds it, as in `let items: Array<Int> = []`"
        );
    }

    #[test]
    fn an_empty_array_literal_the_place_types_does_not_warn() {
        accepts_body("  let empty: Array<Int> = []\n  println(\"{empty.length()}\")?");
        assert!(warnings_of(&in_main(
            "  let empty: Array<Int> = []\n  println(\"{empty.length()}\")?"
        ))
        .is_empty());
    }

    #[test]
    fn a_bare_none_with_no_expected_type_warns() {
        let warning = warns(&in_main(
            "  let missing = None\n  println(\"{missing.isNone()}\")?",
        ));
        assert_eq!(warning.code, UNCONSTRAINED);
        assert_eq!(
            warning.message,
            "nothing says what this `None` is an `Option` of"
        );
    }

    #[test]
    fn a_none_the_place_types_does_not_warn() {
        assert!(warnings_of(&in_main(
            "  let missing: Option<Int> = None\n  println(\"{missing.isNone()}\")?"
        ))
        .is_empty());
    }

    // ---- recovery: everything has already been said

    #[test]
    fn an_error_inside_an_unchecked_call_is_still_reported() {
        // Abstaining about the callee never means abstaining about the
        // arguments.
        let error = rejects(
            "\
use console.println

export fn main() -> Result<Unit, Error> {
  println(1 + 1.0)?
  Ok(())
}
",
        );
        assert_eq!(error.code, OPERATOR);
    }

    /// One mistake is one diagnostic, however far the unknown it produced
    /// travels: `rejects` insists on exactly one error, and every operation
    /// on the recovered value below would have had something to say.
    #[test]
    fn a_recovery_unknown_is_reported_once_however_far_it_spreads() {
        let error = rejects(
            "\
fn run(value: Missing) -> Int {
  value.field.other().length() + 1
}
",
        );
        assert_eq!(error.code, UNKNOWN_TYPE);
    }

    /// An argument of a call that was just rejected is given to a place this
    /// pass has nothing to say about, so the gaps inside it are not reported
    /// as if they were the program's second mistake.
    #[test]
    fn the_arguments_of_a_rejected_call_are_not_reported_again() {
        let source = "\
fn run() -> Int {
  missing([], None, fn(n) { n })
  1
}
";
        let error = rejects(source);
        assert_eq!(error.code, UNKNOWN_NAME);
        assert!(warnings_of(source).is_empty());
    }

    /// A body given to a place a schema declared `Any` is a body with an
    /// answer of its own, made at the call: the note says exactly that
    /// nothing about this value was proved. Its own value is not asked a
    /// second time to state a type nothing outside it stated either.
    #[test]
    fn a_gap_in_the_value_of_a_body_a_schema_declared_any_is_not_reported() {
        let source = "\
use clock

export fn main() -> Result<Unit, Error> {
  clock.timeout(1s) {
    []
  }?
  Ok(())
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
    }

    // -------------------------------- unknowns that must not escape
    //
    // `Unknown::Placeholder` claims to reach no type a program observes, and
    // `Checker::expr` and `Checker::declare` assert it in debug builds. These
    // pin the two places that used to break the claim, each of which used to
    // check clean and then be wrong at run time.

    /// A struct's type parameter that no field mentions used to become a
    /// placeholder, which compares equal to everything: `needsString` below
    /// wants a `Tagged<String>` and used to accept a `Tagged<_>`.
    #[test]
    fn a_struct_type_parameter_nothing_settles_is_reported() {
        let source = "\
struct Tagged<T> { n: Int }

fn needsString(t: Tagged<String>) -> Int { t.n }

fn run() -> Int {
  let p = Tagged(n: 1)
  needsString(p)
}
";
        accepts(source);
        let warning = warns(source);
        assert_eq!(warning.code, UNCONSTRAINED);
        assert_eq!(warning.message, "nothing says what `T` is in `Tagged<T>`");
    }

    /// The place holding the value settles it, whether it is an annotation
    /// or the parameter of the call it is given to.
    #[test]
    fn a_struct_type_parameter_the_place_states_is_settled() {
        for body in [
            "  let p: Tagged<String> = Tagged(n: 1)\n  needsString(p)",
            "  needsString(Tagged(n: 1))",
        ] {
            let source = format!(
                "\
struct Tagged<T> {{ n: Int }}

fn needsString(t: Tagged<String>) -> Int {{ t.n }}

fn run() -> Int {{
{body}
}}
"
            );
            accepts(&source);
            assert!(warnings_of(&source).is_empty(), "{source}");
        }
    }

    /// `Result.mapError`'s callback produces whatever its body produces, so
    /// the expectation states its parameters and leaves its result open. An
    /// early `return` in it therefore has nothing to agree with — which used
    /// to check clean and then fail at run time, because the placeholder
    /// result propagated into the `Err` binding's type.
    #[test]
    fn a_return_in_a_map_error_callback_is_reported() {
        let error = rejects(
            "\
fn attempt() -> Result<Int, Error> { Ok(1) }

fn run() -> Int {
  let r = attempt().mapError { return 42 }
  match r {
    Ok(v) => v
    Err(e) => e.length()
  }
}
",
        );
        assert_eq!(error.code, LAMBDA_RETURN);
    }

    /// Without the `return` the checker gets the type right, and says so
    /// about what is done with it.
    #[test]
    fn a_map_error_callback_that_ends_with_its_value_types_the_failure() {
        let error = rejects(
            "\
fn attempt() -> Result<Int, Error> { Ok(1) }

fn run() -> Int {
  let r = attempt().mapError { 42 }
  match r {
    Ok(v) => v
    Err(e) => e.length()
  }
}
",
        );
        assert_eq!(error.code, UNKNOWN_METHOD);
    }

    /// A function value given to a place that is not a function type is a
    /// mismatch like any other. `Checker::expr` hands the expectation to
    /// `Checker::lambda` rather than checking the result against it, because
    /// a lambda reads the expectation to type its parameters — and the check
    /// that skipped used to be skipped for good, so the value was silently
    /// accepted.
    #[test]
    fn a_function_value_given_to_a_place_that_is_not_one_is_a_mismatch() {
        for source in [
            "fn run() -> Int {\n  let x: Int = fn(n: Int) { n }\n  x\n}\n",
            "fn run() -> Int {\n  let x: Int = fn(n: Int) { return n }\n  x\n}\n",
        ] {
            let error = rejects(source);
            assert_eq!(error.code, MISMATCH, "{source}");
        }
    }

    /// A placeholder is the one unknown that must never be observable, and
    /// it is now a value rather than a convention, so the difference can be
    /// asked about.
    #[test]
    fn only_a_placeholder_answers_that_it_must_not_escape() {
        assert!(Ty::placeholder().holds_placeholder());
        assert!(!Ty::recovery().holds_placeholder());
        assert!(!Ty::dynamic_boundary().holds_placeholder());
        assert!(!Ty::unconstrained().holds_placeholder());
        // And it is found however deeply it is buried, which is what makes
        // the assertions in `expr` and `declare` worth having.
        assert!(Ty::Array(Box::new(Ty::Option(Box::new(Ty::placeholder())))).holds_placeholder());
        assert!(Ty::func(false, vec![Ty::Int], Ty::placeholder()).holds_placeholder());
        // Every other kind is one the checker has already accounted for, so
        // a form given to a place typed by one adds nothing by complaining.
        assert!(Ty::recovery().is_accounted_for());
        assert!(Ty::dynamic_boundary().is_accounted_for());
        assert!(Ty::unconstrained().is_accounted_for());
        assert!(!Ty::placeholder().is_accounted_for());
    }

    // ------------------- a gap a sibling or a branch settles is no gap

    /// `[[], [1]]` is an `Array<Array<Int>>`: the sibling says what the
    /// empty literal holds, so nothing was left unproved and nothing is
    /// reported. The element type is the joined one, not `Array<_>`.
    #[test]
    fn an_empty_array_a_sibling_settles_is_not_reported() {
        let source = "\
fn run() -> Int {
  let rows = [[], [1]]
  rows.length()
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
        // The join reached inside the shared shape, so the elements are
        // still checked from here on.
        let error = rejects(
            "\
fn run() -> Int {
  let rows = [[], [1]]
  let first: Array<String> = rows[0]
  1
}
",
        );
        assert_eq!(error.code, MISMATCH);
    }

    #[test]
    fn a_none_a_sibling_settles_is_not_reported() {
        let source = "\
fn run() -> Int {
  let values = [None, Some(1)]
  values.length()
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
    }

    #[test]
    fn a_none_the_other_branch_settles_is_not_reported() {
        let source = "\
fn run() -> Int {
  let value = if true { None } else { Some(1) }
  value.unwrapOr(0)
}
";
        accepts(source);
        assert!(warnings_of(source).is_empty());
    }

    /// Branches that genuinely disagree are still one diagnostic, not one
    /// per branch: the probe only supplies an expectation when the two
    /// already agree.
    #[test]
    fn branches_that_disagree_are_still_reported_once() {
        let error = rejects(
            "\
fn run() -> Int {
  let value = if true { 1 } else { \"two\" }
  1
}
",
        );
        assert_eq!(error.code, BRANCHES);
    }

    // ------------------------------------------------- entry shape

    fn config_with_entry(entry: &str) -> Config {
        let mut runs = BTreeMap::new();
        runs.insert(
            "run".to_string(),
            crate::config::RunConfig {
                entry: entry.to_string(),
                allow: Vec::new(),
                fuel: None,
                deadline: None,
                max_host_calls: None,
                max_tasks: None,
                trace: None,
                generates: None,
            },
        );
        Config {
            runs,
            ..Config::default()
        }
    }

    #[track_caller]
    fn entry_errors(source: &str) -> Vec<Diagnostic> {
        diagnostics_with(source, config_with_entry("main.main"))
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    #[test]
    fn accepts_both_entry_shapes() {
        assert!(
            entry_errors("export fn main() -> Result<Unit, Error> {\n  Ok(())\n}\n").is_empty()
        );
        assert!(entry_errors(
            "export fn main(args: Array<String>) -> Result<Unit, Error> {\n  Ok(())\n}\n"
        )
        .is_empty());
        assert!(entry_errors("export fn main() {\n}\n").is_empty());
    }

    #[test]
    fn rejects_an_entry_with_two_parameters() {
        let errors = entry_errors(
            "export fn main(args: Array<String>, extra: Int) -> Result<Unit, Error> {\n  Ok(())\n}\n",
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ENTRY);
        assert_eq!(errors[0].message, "entry `main.main` declares 2 parameters");
        assert_eq!(
            errors[0].rule.as_deref().unwrap(),
            "An entry function takes either no parameters or one `Array<String>` of process arguments."
        );
        assert_eq!(
            errors[0].help.as_deref().unwrap(),
            "write `fn main()` or `fn main(args: Array<String>)`"
        );
    }

    #[test]
    fn rejects_an_entry_whose_parameter_is_not_the_process_arguments() {
        let errors =
            entry_errors("export fn main(count: Int) -> Result<Unit, Error> {\n  Ok(())\n}\n");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ENTRY);
        assert_eq!(
            errors[0].message,
            "entry `main.main` takes `Int`, but the host passes `Array<String>`"
        );
        assert_eq!(
            errors[0].help.as_deref().unwrap(),
            "write `fn main(args: Array<String>)`"
        );
    }

    #[test]
    fn rejects_an_entry_whose_result_the_host_cannot_report() {
        let errors = entry_errors("export fn main() -> Int {\n  1\n}\n");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ENTRY);
        assert_eq!(
            errors[0].message,
            "entry `main.main` returns `Int`, which the host cannot report"
        );
        assert_eq!(
            errors[0].rule.as_deref().unwrap(),
            "The host reports an entry's failure through its `Err`, so an entry returns `()` or a `Result`."
        );
    }

    // --------------------------------------------- the whole repository

    /// Every program in the repository, checked the way the CLI checks one.
    ///
    /// A package that exists to pin a check-time failure must fail; every
    /// other package must check with no errors at all. That is the
    /// acceptance bar for this pass, and it keeps itself honest: a new
    /// example or end-to-end case joins it by existing.
    ///
    /// Most such packages are named `fail_...`. Two are not, and are named
    /// here rather than renamed: `fn_labels` and `type_struct` were cases
    /// that ran, printed, and then failed at run time until ADR 0021 made
    /// their last line a check-time error, and their names say what they are
    /// about — every accepted call form, and every accepted struct form —
    /// rather than what the last line of each does.
    #[test]
    fn every_program_in_the_repository_checks() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut packages = vec![root.join("examples"), root.join("tests/e2e")];
        let mut nested: Vec<PathBuf> = std::fs::read_dir(root.join("tests/e2e"))
            .expect("the end-to-end suite exists")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.join("cove.toml").is_file())
            .collect();
        nested.sort();
        packages.append(&mut nested);
        assert!(
            packages.len() > 8,
            "expected to find every package, found {packages:?}"
        );

        let mut failures = Vec::new();
        for directory in &packages {
            let name = directory
                .file_name()
                .expect("a package directory has a name")
                .to_string_lossy()
                .into_owned();
            let must_fail =
                name.starts_with("fail_") || matches!(name.as_str(), "fn_labels" | "type_struct");

            let mut sources = SourceMap::new();
            let package = match crate::package::load(directory, &mut sources) {
                Ok(package) => package,
                Err(diagnostics) => {
                    if !must_fail {
                        failures.push(format!(
                            "{name}: does not load: {}",
                            render_all(&sources, &diagnostics)
                        ));
                    }
                    continue;
                }
            };
            let program = match resolve(&package) {
                Ok(program) => program,
                Err(diagnostics) => {
                    if !must_fail {
                        failures.push(format!(
                            "{name}: does not resolve: {}",
                            render_all(&sources, &diagnostics)
                        ));
                    }
                    continue;
                }
            };
            let errors: Vec<Diagnostic> = check(&package, &program)
                .into_iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            match (must_fail, errors.is_empty()) {
                (false, false) => failures.push(format!(
                    "{name}: does not type-check: {}",
                    render_all(&sources, &errors)
                )),
                (true, true) => {
                    failures.push(format!("{name}: was expected to fail, but it checks"))
                }
                _ => {}
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    // ---------------------------------------------------- import environment

    const LEVELS: &str = "\
/// Supported logging levels.
export enum LogLevel {
  Debug
  Info
}

/// Validated configuration.
export struct Config {
  port: Int
  level: LogLevel
}

/// A pair of ports.
export struct Pair<T> {
  first: T
  second: T
}

/// The shape a handler has.
export type Handler = fn(Int) -> String

impl Config {
  /// The port, as text.
  export fn describe(self) -> String {
    \"{self.port}\"
  }
}

/// Loads configuration.
export fn load() -> Config {
  Config(port: 8080, level: LogLevel.Debug)
}

fn secret() -> Int {
  1
}
";

    #[test]
    fn the_checker_sees_an_imported_struct_s_fields() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.load\n\n/// Entry point.\nexport fn main() -> Int {\n  load().port\n}\n",
            ),
        ]);
    }

    #[test]
    fn a_field_an_imported_struct_does_not_declare_is_rejected() {
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.load\n\n/// Entry point.\nexport fn main() -> Int {\n  load().host\n}\n",
            ),
        ]);
        assert_eq!(error.code, UNKNOWN_FIELD);
        // The type is named by the module that declares it.
        assert!(error.message.contains("levels.Config"));
    }

    #[test]
    fn an_imported_struct_s_field_keeps_its_type() {
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.load\n\n/// Entry point.\nexport fn main() -> String {\n  load().port\n}\n",
            ),
        ]);
        assert_eq!(error.code, MISMATCH);
        assert!(error.message.contains("Int"));
    }

    #[test]
    fn an_imported_struct_is_initialized_and_checked_like_a_declared_one() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.Config\nuse levels.LogLevel\n\n/// Entry point.\nexport fn main() -> Config {\n  Config(port: 1, level: LogLevel.Info)\n}\n",
            ),
        ]);
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.Config\nuse levels.LogLevel\n\n/// Entry point.\nexport fn main() -> Config {\n  Config(port: \"1\", level: LogLevel.Info)\n}\n",
            ),
        ]);
        assert_eq!(error.code, MISMATCH);
    }

    #[test]
    fn an_imported_function_s_arguments_are_checked() {
        let error = rejects_modules(&[
            (
                "greet",
                "/// Greets by name.\nexport fn greeting(name: String) -> String {\n  name\n}\n",
            ),
            (
                "app",
                "use greet.greeting\n\n/// Entry point.\nexport fn main() -> String {\n  greeting(1)\n}\n",
            ),
        ]);
        assert_eq!(error.code, MISMATCH);
    }

    #[test]
    fn an_imported_method_is_reached_through_the_value_s_type() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.load\n\n/// Entry point.\nexport fn main() -> String {\n  load().describe()\n}\n",
            ),
        ]);
    }

    #[test]
    fn an_imported_enum_s_cases_are_checked() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.LogLevel\n\n/// Entry point.\nexport fn main(level: LogLevel) -> String {\n  match level {\n    LogLevel.Debug => \"d\"\n    LogLevel.Info => \"i\"\n  }\n}\n",
            ),
        ]);
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.LogLevel\n\n/// Entry point.\nexport fn main() -> LogLevel {\n  LogLevel.Bogus\n}\n",
            ),
        ]);
        assert_eq!(error.code, UNKNOWN_CASE);
    }

    #[test]
    fn an_imported_generic_type_keeps_its_arity_and_arguments() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.Pair\n\n/// Entry point.\nexport fn main() -> Int {\n  Pair(first: 1, second: 2).first\n}\n",
            ),
        ]);
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.Pair\n\n/// Entry point.\nexport fn main() -> Pair<Int> {\n  Pair(first: 1, second: \"2\")\n}\n",
            ),
        ]);
        assert_eq!(error.code, MISMATCH);
    }

    /// A module that exports a nominal type and not its representation:
    /// `Token.of` and `text` are the only ways in from outside.
    const OPAQUE: &str = "\
/// A token, whose representation is this module's own business.
export opaque struct Token {
  raw: String
}

/// Reads the representation from the module that declares it.
fn rawOf(token: Token) -> String {
  token.raw
}

impl Token {
  /// Builds a token.
  export fn of(raw: String) -> Token {
    Token(raw: raw)
  }

  /// The token as text.
  export fn text(self) -> String {
    rawOf(self)
  }
}
";

    /// The same interface over a different representation. A caller written
    /// against [`OPAQUE`] must not be able to tell the two apart.
    const OPAQUE_REPRESENTATION_CHANGED: &str = "\
/// A token, whose representation is this module's own business.
export opaque struct Token {
  scheme: String
  body: String
}

impl Token {
  /// Builds a token.
  export fn of(raw: String) -> Token {
    Token(scheme: \"bearer\", body: raw)
  }

  /// The token as text.
  export fn text(self) -> String {
    self.body
  }
}
";

    /// The module that declares an opaque type is unaffected by it: it
    /// writes the synthesized constructor and reads the fields as it would
    /// for any struct.
    #[test]
    fn the_declaring_module_builds_and_inspects_an_opaque_type() {
        accepts_modules(&[("auth", OPAQUE)]);
    }

    #[test]
    fn another_module_may_not_build_an_opaque_type_field_by_field() {
        for caller in [
            "use auth.Token\n\n/// Entry point.\nexport fn main() -> Token {\n  Token(raw: \"t\")\n}\n",
            "use auth\n\n/// Entry point.\nexport fn main() -> auth.Token {\n  auth.Token(raw: \"t\")\n}\n",
        ] {
            let error = rejects_modules(&[("auth", OPAQUE), ("app", caller)]);
            assert_eq!(error.code, OPAQUE_CONSTRUCTION, "{caller}");
            // The help names what the module did export instead.
            assert!(error
                .help
                .as_ref()
                .expect("the diagnostic offers a correction")
                .contains("Token.of()"));
        }
    }

    #[test]
    fn another_module_may_not_read_an_opaque_type_s_field() {
        let error = rejects_modules(&[
            ("auth", OPAQUE),
            (
                "app",
                "use auth.Token\n\n/// Entry point.\nexport fn main(token: Token) -> String {\n  token.raw\n}\n",
            ),
        ]);
        assert_eq!(error.code, OPAQUE_FIELD);
        assert!(error
            .help
            .as_ref()
            .expect("the diagnostic offers a correction")
            .contains("text()"));
    }

    /// Assignment reaches the field through the same check, so a caller
    /// cannot write what it may not read.
    #[test]
    fn another_module_may_not_assign_an_opaque_type_s_field() {
        let error = rejects_modules(&[
            ("auth", OPAQUE),
            (
                "app",
                "use auth.Token\n\n/// Entry point.\nexport fn main(var token: Token) {\n  token.raw = \"other\"\n}\n",
            ),
        ]);
        assert_eq!(error.code, OPAQUE_FIELD);
        // A write is refused in the words of a write, and corrected in
        // them: "read the value through a method" is no answer here.
        assert!(
            error.message.contains("cannot be assigned"),
            "{}",
            error.message
        );
        let help = error.help.expect("the diagnostic offers a correction");
        assert!(help.starts_with("change the value"), "{help}");
    }

    /// The refusal is the whole diagnosis: a caller that guesses at the
    /// representation is not told how close it came.
    #[test]
    fn a_refused_construction_does_not_disclose_the_fields() {
        for call in ["Token(bogus: 1)", "Token()", "Token(scheme: \"bearer\")"] {
            let caller = format!(
                "use auth.Token\n\n/// Entry point.\nexport fn main() -> Token {{\n  {call}\n}}\n"
            );
            // `rejects_modules` insists on exactly one error, which is the
            // point: no `unknown_label` naming the fields, and no
            // `missing_argument` rendering the declaring module's source.
            let error = rejects_modules(&[
                ("auth", OPAQUE_REPRESENTATION_CHANGED),
                ("app", caller.as_str()),
            ]);
            assert_eq!(error.code, OPAQUE_CONSTRUCTION, "{call}");
            for hidden in ["scheme", "body"] {
                let rendered = format!(
                    "{}{}",
                    error.message,
                    error.help.clone().unwrap_or_default()
                );
                assert!(!rendered.contains(hidden), "{call} disclosed `{hidden}`");
            }
        }
    }

    /// The help names what the declaring module published, not what the
    /// module being checked is in the middle of writing: a caller may
    /// conform a foreign opaque type to a trait of its own, and being told
    /// to call the method whose body is the error is no correction.
    #[test]
    fn the_help_names_only_the_declaring_module_s_methods() {
        let error = rejects_modules(&[
            ("auth", OPAQUE),
            (
                "app",
                "use auth.Token\n\n/// A thing with a text form.\ntrait Show {\n  /// Shows it.\n  fn show(self) -> String\n}\n\nimpl Show for Token {\n  fn show(self) -> String { self.raw }\n}\n",
            ),
        ]);
        assert_eq!(error.code, OPAQUE_FIELD);
        let help = error.help.expect("the diagnostic offers a correction");
        assert!(help.contains("text()"), "{help}");
        assert!(!help.contains("show()"), "{help}");
    }

    /// The type's name and its exported operations are what an opaque
    /// export is for, so all of them still work across the boundary.
    #[test]
    fn another_module_uses_an_opaque_type_through_its_exported_operations() {
        accepts_modules(&[
            ("auth", OPAQUE),
            (
                "app",
                "use auth.Token\n\n/// Entry point.\nexport fn main() -> String {\n  Token.of(\"t\").text()\n}\n",
            ),
        ]);
    }

    /// The point of the modifier: the representation is free to change
    /// underneath a caller that only names the type and its operations.
    #[test]
    fn an_opaque_type_s_representation_may_change_without_touching_its_callers() {
        let caller = "use auth.Token\n\n/// Entry point.\nexport fn main() -> String {\n  Token.of(\"t\").text()\n}\n";
        accepts_modules(&[("auth", OPAQUE), ("app", caller)]);
        accepts_modules(&[("auth", OPAQUE_REPRESENTATION_CHANGED), ("app", caller)]);
    }

    /// A plain `export struct` is unchanged: its representation is public,
    /// which is what every module written before `opaque` depends on.
    #[test]
    fn a_plain_exported_struct_still_exposes_its_representation() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.Config\nuse levels.LogLevel\n\n/// Entry point.\nexport fn main() -> Int {\n  Config(port: 1, level: LogLevel.Info).port\n}\n",
            ),
        ]);
    }

    #[test]
    fn an_imported_type_alias_expands() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.Handler\n\n/// Entry point.\nexport fn main(handler: Handler) -> String {\n  handler(1)\n}\n",
            ),
        ]);
    }

    /// A module imported whole makes its exports writable qualified, with
    /// the same checking a `use` of each would give.
    #[test]
    fn a_module_imported_whole_is_named_qualified() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels\n\n/// Entry point.\nexport fn main() -> levels.Config {\n  levels.load()\n}\n",
            ),
        ]);
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels\n\n/// Entry point.\nexport fn main() -> Int {\n  levels.load()\n}\n",
            ),
        ]);
        assert_eq!(error.code, MISMATCH);
    }

    #[test]
    fn a_qualified_name_a_module_does_not_export_is_rejected() {
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels\n\n/// Entry point.\nexport fn main() -> Int {\n  levels.secret()\n}\n",
            ),
        ]);
        assert_eq!(error.code, UNKNOWN_MEMBER);
        assert!(error.message.contains("not exported"));
    }

    #[test]
    fn a_qualified_name_a_module_does_not_declare_is_rejected() {
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels\n\n/// Entry point.\nexport fn main() -> Int {\n  levels.missing()\n}\n",
            ),
        ]);
        assert_eq!(error.code, UNKNOWN_MEMBER);
        assert!(error.message.contains("declares no `missing`"));
    }

    /// Two modules may each declare a `Config`, and the checker must not
    /// confuse them: a declaration is known by the module that declares it.
    #[test]
    fn two_modules_declaring_one_name_are_different_types() {
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.load\n\n/// This module's own `Config`.\nexport struct Config {\n  port: Int\n}\n\n\
                 /// Entry point.\nexport fn main() -> Config {\n  load()\n}\n",
            ),
        ]);
        assert_eq!(error.code, MISMATCH);
        assert!(error.message.contains("levels.Config"));
    }

    /// A type reached only as an imported function's result still has its
    /// fields, even though this module could not write its name.
    #[test]
    fn a_type_reached_without_importing_it_still_has_its_fields() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.load\n\n/// Entry point.\nexport fn main() -> String {\n  load().describe()\n}\n",
            ),
        ]);
        let error = rejects_modules(&[
            ("levels", LEVELS),
            (
                "app",
                "use levels.load\n\n/// Entry point.\nexport fn main() -> String {\n  load().level\n}\n",
            ),
        ]);
        assert_eq!(error.code, MISMATCH);
        assert!(error.message.contains("levels.LogLevel"));
    }

    /// The transitive case: a module that imports a module that imports a
    /// third still sees one identity for the third's type.
    #[test]
    fn a_type_keeps_one_identity_through_two_imports() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "middle",
                "use levels.load\nuse levels.Config\n\n/// Reloads.\nexport fn reload() -> Config {\n  load()\n}\n",
            ),
            (
                "app",
                "use middle.reload\nuse levels.Config\n\n/// Entry point.\nexport fn main() -> Config {\n  reload()\n}\n",
            ),
        ]);
    }

    /// A diamond needs no special treatment: importing a module runs none
    /// of its code, and both sides of the diamond name the same declaration.
    #[test]
    fn a_type_keeps_one_identity_through_a_diamond() {
        accepts_modules(&[
            ("levels", LEVELS),
            (
                "left",
                "use levels.load\nuse levels.Config\n\n/// Loads.\nexport fn fromLeft() -> Config {\n  load()\n}\n",
            ),
            (
                "right",
                "use levels.load\nuse levels.Config\n\n/// Loads.\nexport fn fromRight() -> Config {\n  load()\n}\n",
            ),
            (
                "app",
                "use left.fromLeft\nuse right.fromRight\n\n/// Entry point.\nexport fn main() -> Int {\n  fromLeft().port + fromRight().port\n}\n",
            ),
        ]);
    }

    // ------------------------------------------ conformances across modules

    const DISPLAY: &str = "\
/// Renders itself.
export trait Display {
  /// The full form.
  fn describe(self) -> String

  /// A short form, defaulting to the full one.
  fn label(self) -> String { self.describe() }
}

/// Renders anything that conforms.
export fn render<T: Display>(value: T) -> String {
  value.label()
}
";

    const BOOKING: &str = "\
/// A booking.
export struct Booking {
  id: Int
}
";

    /// A conformance declared where the type is, for an imported trait: the
    /// bound it satisfies is checked in a third module that imports both.
    #[test]
    fn a_bound_is_satisfied_by_a_conformance_to_an_imported_trait() {
        let booking = format!(
            "use display.Display\n\n{BOOKING}\nimpl Display for Booking {{\n  \
             /// The full form.\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n"
        );
        accepts_modules(&[
            ("display", DISPLAY),
            ("booking", &booking),
            (
                "app",
                "use display.render\nuse booking.Booking\n\n\
                 /// Entry point.\nexport fn main() -> String {\n  render(Booking(id: 1))\n}\n",
            ),
        ]);
    }

    /// And the reverse: the conformance is declared where the trait is, for
    /// an imported type.
    #[test]
    fn a_bound_is_satisfied_by_a_conformance_to_an_imported_type() {
        let display = format!(
            "use booking.Booking\n\n{DISPLAY}\nimpl Display for Booking {{\n  \
             /// The full form.\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n"
        );
        accepts_modules(&[
            ("booking", BOOKING),
            ("display", &display),
            (
                "app",
                "use display.render\nuse booking.Booking\n\n\
                 /// Entry point.\nexport fn main() -> String {\n  render(Booking(id: 1))\n}\n",
            ),
        ]);
    }

    /// A trait method supplied by a conformance in another module is a
    /// method of the type, reachable wherever that conformance is visible.
    #[test]
    fn a_conformance_method_declared_elsewhere_is_a_method_of_the_type() {
        let display = format!(
            "use booking.Booking\n\n{DISPLAY}\nimpl Display for Booking {{\n  \
             /// The full form.\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n"
        );
        accepts_modules(&[
            ("booking", BOOKING),
            ("display", &display),
            (
                "app",
                "use display.Display\nuse booking.Booking\n\n\
                 /// Entry point.\nexport fn main(value: Booking) -> String {\n  value.describe()\n}\n",
            ),
        ]);
    }

    #[test]
    fn a_type_that_conforms_nowhere_does_not_satisfy_an_imported_bound() {
        let error = rejects_modules(&[
            ("display", DISPLAY),
            ("booking", BOOKING),
            (
                "app",
                "use display.render\nuse booking.Booking\n\n\
                 /// Entry point.\nexport fn main() -> String {\n  render(Booking(id: 1))\n}\n",
            ),
        ]);
        assert_eq!(error.code, UNSATISFIED_BOUND);
        assert!(error.message.contains("booking.Booking"));
        assert!(error.message.contains("display.Display"));
    }

    /// `dyn` names an imported trait through a `use` of the trait itself,
    /// and the conversion consults the same conformances a bound does.
    #[test]
    fn dyn_names_an_imported_trait() {
        let booking = format!(
            "use display.Display\n\n{BOOKING}\nimpl Display for Booking {{\n  \
             /// The full form.\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n"
        );
        accepts_modules(&[
            ("display", DISPLAY),
            ("booking", &booking),
            (
                "app",
                "use display.Display\nuse booking.Booking\n\n\
                 /// Entry point.\nexport fn main() -> String {\n  \
                 let shown: dyn Display = Booking(id: 1)\n  shown.label()\n}\n",
            ),
        ]);
    }

    #[test]
    fn a_trait_neither_declared_nor_imported_is_not_a_trait() {
        let error = rejects_modules(&[
            ("display", DISPLAY),
            (
                "app",
                "/// Entry point.\nexport fn main() -> Int {\n  1\n}\n\n\
                 /// Renders.\nfn show<T: Display>(value: T) -> String {\n  \"x\"\n}\n",
            ),
        ]);
        assert_eq!(error.code, UNKNOWN_TRAIT);
    }

    /// Two modules may each declare a `Display`, and a `dyn` of one is not a
    /// `dyn` of the other.
    #[test]
    fn two_modules_declaring_one_trait_name_are_different_traits() {
        let booking = format!(
            "use display.Display\n\n{BOOKING}\nimpl Display for Booking {{\n  \
             /// The full form.\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n"
        );
        let error = rejects_modules(&[
            ("display", DISPLAY),
            ("booking", &booking),
            (
                "app",
                "use booking.Booking\n\n\
                 /// This module's own `Display`, unrelated to `display`'s.\n\
                 trait Display {\n  /// The full form.\n  fn describe(self) -> String\n}\n\n\
                 /// Entry point.\nexport fn main() -> Int {\n  \
                 let shown: dyn Display = Booking(id: 1)\n  1\n}\n",
            ),
        ]);
        assert_eq!(error.code, MISMATCH);
        assert!(error.message.contains("dyn Display"));
    }

    /// A conformance's signature is checked in the module that declares the
    /// conformance, against the trait it imported.
    #[test]
    fn a_conformance_to_an_imported_trait_must_match_its_signature() {
        let booking = format!(
            "use display.Display\n\n{BOOKING}\nimpl Display for Booking {{\n  \
             /// The full form.\n  fn describe(self) -> Int {{\n    1\n  }}\n}}\n"
        );
        let error = rejects_modules(&[("display", DISPLAY), ("booking", &booking)]);
        assert_eq!(error.code, CONFORMANCE_SIGNATURE);
    }

    #[test]
    fn a_name_neither_declared_nor_imported_is_still_unresolved() {
        let error = rejects_modules(&[
            (
                "greet",
                "/// Greets.\nexport fn greeting() -> String {\n  \"hi\"\n}\n",
            ),
            (
                "app",
                "/// Entry point.\nexport fn main() -> String {\n  greeting()\n}\n",
            ),
        ]);
        assert_eq!(error.code, UNKNOWN_NAME);
    }

    fn render_all(sources: &SourceMap, diagnostics: &[Diagnostic]) -> String {
        diagnostics
            .iter()
            .map(|d| cove_diag::render(sources, d))
            .collect::<Vec<_>>()
            .join("")
    }
}
