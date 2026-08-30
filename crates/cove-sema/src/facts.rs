//! What the checker worked out about each expression.
//!
//! The type checker settles a type for every expression it walks and, until
//! now, threw each one away as soon as the surrounding form had been checked
//! against it. Everything downstream that needed one had to work it out
//! again — and a pass that walks a tree without the checker's tables cannot,
//! so it guessed from the shape of the source instead. ADR 0019 states the
//! rule this module exists to make keepable: the lowering reads the
//! checker's answers rather than recomputing them, so the two cannot
//! disagree.
//!
//! # Recording is not deciding
//!
//! Nothing here participates in checking. A fact is written after the
//! checker has settled it and is read by nobody during the walk, so adding
//! one changes no diagnostic. That is the property this table is worth
//! having only if it keeps.
//!
//! # Keys are dense integers, so a lookup is a load
//!
//! An expression is named by the file it was parsed from and its
//! [`ExprId`], which [`cove_syntax::number::number_unit`] hands out as
//! `0..n` over one file with no gaps. Both halves are therefore an index
//! into a `Vec` rather than a hash of anything, which is what lets the
//! checker afford a push per expression.
//!
//! # An unknown is an answer
//!
//! [`Facts::ty`] answers `None` only for an expression the checker never
//! walked. An expression it walked and could say nothing about answers
//! `Some(`[`Ty::Unknown`]`(..))`, because "the checker abstained" and "I
//! never asked" are different facts and a consumer specialising on one must
//! not act on the other.
//!
//! # A name a call resolves is not an expression the checker types
//!
//! The table is total over a function's expressions with one exception, and
//! it is the checker's shape rather than an omission here. A callee is
//! walked only when the call goes through a value: `f(1)` where `f` is a
//! binding evaluates `f`, and so does a call through a field holding a
//! closure. A callee that instead *names* a declaration — a function, a
//! struct being initialized, an enum case, a type's associated function, or
//! a method reached through a receiver — is resolved against the checker's
//! tables and never given a type, because several of those have none to be
//! given: `Point` in `Point(x: 0.0)` names a type, not a value.
//!
//! So `ty` answers `None` for those, and that answer carries the
//! distinction rather than losing it: a callee with a recorded type is a
//! call through a value, and a callee without one is a call to a
//! declaration — which [`Facts::target`] then names.
//!
//! # A declaration's boundary is a fact too, and it is a small one
//!
//! An expression is not the only thing the checker settles. It also resolves
//! every declaration's signature — what each parameter is, what the receiver
//! is, what comes back — and a consumer that has to know where a parameter
//! lives, or which stack an answer comes back on, is asking about the
//! signature rather than about any expression inside the body. Re-deriving
//! it from the source would be the same mistake in a new place: a `->
//! module.Thing` written in one module and read in another is a name whose
//! meaning only the checker holds.
//!
//! [`Facts::signature`] is that table, and it is keyed differently from the
//! expression tables on purpose — see [`Facts::signature`] for why a hash is
//! the right shape here and the wrong one there.

use std::collections::HashMap;

use cove_diag::{FileId, Span};
use cove_syntax::ast::ExprId;

use crate::typeck::Ty;

/// The declaration a call resolved to, named the way the package names it.
///
/// A method call is written against a value, and which declaration it
/// reaches is decided by that value's type — which only the checker knows.
/// A pass reading the source alone can do no better than match the method's
/// name, and a name is not unique: two types may declare one, and a declared
/// type and a builtin may share one. Recording the answer is what turns that
/// guess into a lookup.
///
/// The three parts name a declaration exactly. `module` is the module whose
/// `impl` block writes it, spelled out even when the call is in that same
/// module, so a target read anywhere in the package means the same thing.
/// `type_name` is the type's bare name within that module, and `method` is
/// the name as declared.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MethodTarget {
    /// The module that declares the type the method is written on.
    pub module: String,
    /// The type's name inside `module`, without the module qualifier.
    pub type_name: String,
    /// The method's own name.
    pub method: String,
}

/// A declaration's boundary, as the checker resolved it for that body.
///
/// Every type here is the one the checker held while it walked *this*
/// declaration's body, not a re-derivation from the source: an annotation is
/// resolved once, against the module it was written in, and this is that
/// answer rather than a second reading of it.
///
/// The three parts are kept apart the way a declaration keeps them. `params`
/// is in declaration order and holds one entry per written parameter;
/// `receiver` is the type of `self` and is `Some` only for a method, so a
/// consumer that has to place arguments knows the receiver comes first
/// without inferring it from a count. `ret` is `Ty::Unit` for a declaration
/// with no `->`, because a function with no declared return type returns
/// `()` and that is a settled type rather than an absence.
///
/// # Three kinds of declaration have one
///
/// A function or a method, written with a `fn`; a struct, whose initializer
/// `Point(x: 0.0)` is a call the checker synthesizes a signature for out of
/// the fields, so `params` is the field types in declaration order; and one
/// case of an enum, whose `params` is its payload types. The last two are
/// what publishes a declared type's *shape* — the only other thing that knows
/// it is the lowering, which turns it into slot numbers and keeps no names.
///
/// A struct's and a case's types are the declaration's own, so a generic
/// declaration records the `Ty::Param` it was written with; a consumer
/// holding a *use* completes them with `Ty::instantiate`.
#[derive(Clone, Debug, PartialEq)]
pub struct Signature {
    /// The type of `self`, for a method, and nothing for a free function.
    pub receiver: Option<Ty>,
    /// The declared parameters, in declaration order, receiver excluded.
    ///
    /// # Which question this answers
    ///
    /// **What a call supplies**, not what the callee's binding holds. The
    /// two are the same type for every parameter shape but one, and the
    /// exception is worth stating because a consumer that reads this field
    /// as the second question gets a wrong answer silently.
    ///
    /// A variadic parameter is recorded as its **element** type, because a
    /// call site passes elements: `fn count(items: Int...)` records `Int`
    /// here. Inside the body `items` is the `Array<Int>` the callee made of
    /// them, and nothing in this struct says so. A consumer that needs the
    /// binding's type reads `variadic` off the declaration's own `Param` and
    /// wraps — which is what `cove_ir::lower` does, pinned by its
    /// `a_variadic_parameter_of_ints_is_still_a_value_slot`, after having
    /// first asked this field and been told `Int`.
    ///
    /// A `var` parameter needs no such note: it names the caller's storage,
    /// and the type of that storage is the type recorded here. What a `var`
    /// changes is where the binding lives, not what it holds, and this
    /// struct records no marking at all.
    pub params: Vec<Ty>,
    /// What a call to this declaration answers.
    pub ret: Ty,
}

/// What the checker worked out about each expression.
///
/// One of these covers a whole package: every file the checker walked,
/// within each file every expression it settled something about, and the
/// boundary of every declaration it resolved. It is published on
/// [`Program`](crate::resolve::Program), which is what a consumer of a
/// checked package already holds.
#[derive(Debug, Default)]
pub struct Facts {
    /// Indexed by [`FileId`]. A file the checker never walked is an empty
    /// entry rather than a missing one, because the index has to stay the
    /// id.
    files: Vec<FileFacts>,
    /// One entry per declared function, method, struct, and enum case, keyed
    /// by the file it was written in and the start offset of its
    /// declaration.
    ///
    /// A hash where the expression tables are a `Vec`, and the difference is
    /// a difference in size rather than a change of mind. A declaration's
    /// span has no dense numbering to index by — it is a byte offset into a
    /// file, and the offsets of a file's declarations are sparse across its
    /// whole length — so a dense table would be one slot per byte. What
    /// makes a hash affordable anyway is that there is one entry per
    /// declaration rather than one per expression, and it is read twice per
    /// function at lowering time, once for the function's own boundary and
    /// once at each call site that names it. Nothing on a hot path reads it
    /// at all.
    signatures: HashMap<(FileId, u32), Signature>,
}

/// Everything recorded about one file, indexed by [`ExprId`].
///
/// The two tables are separate rather than one table of pairs because they
/// are populated at different densities: a type is recorded for every
/// expression, and a target for the few that are calls to a declared
/// method. Kept together, the sparse half would cost a slot per expression.
#[derive(Debug, Default)]
struct FileFacts {
    types: Vec<Option<Ty>>,
    targets: Vec<Option<MethodTarget>>,
}

impl Facts {
    /// The type of the expression, if the checker settled one.
    ///
    /// `None` means this expression was never walked — a body the checker
    /// stopped before reaching, or a tree that was never part of a checked
    /// package. It never means the checker was unsure: see the module docs.
    pub fn ty(&self, file: FileId, id: ExprId) -> Option<&Ty> {
        self.files
            .get(file.0 as usize)?
            .types
            .get(id.0 as usize)?
            .as_ref()
    }

    /// The declaration this call resolved to, if it resolved to one.
    ///
    /// Only a call the checker matched against a declaration written in an
    /// `impl` block answers here. A call to a builtin method, to a host
    /// operation, or through a trait bound answers `None`, because none of
    /// those names a declaration of this package.
    pub fn target(&self, file: FileId, id: ExprId) -> Option<&MethodTarget> {
        self.files
            .get(file.0 as usize)?
            .targets
            .get(id.0 as usize)?
            .as_ref()
    }

    /// The boundary of the declaration written at `decl`, if the checker
    /// resolved one.
    ///
    /// `decl` is the `FnDecl`'s own span, which is what a consumer holding a
    /// declaration already has and what makes the key need no side channel:
    /// the checker records against the same span, so a declaration found in
    /// the tree and the fact recorded about it meet without either naming
    /// the other.
    ///
    /// `decl` is a `fn` declaration's span, a struct declaration's, or one
    /// enum case's — see [`Signature`] for what each records.
    ///
    /// `None` means the checker never resolved this declaration — a body it
    /// stopped before reaching, or a tree that was never part of a checked
    /// package. As everywhere else here, it does not mean the checker was
    /// unsure: a parameter it could say nothing about is recorded as
    /// [`Ty::Unknown`], which is an answer.
    pub fn signature(&self, file: FileId, decl: Span) -> Option<&Signature> {
        self.signatures.get(&(file, decl.start))
    }

    /// Records the type the checker settled for one expression.
    ///
    /// A later record for the same id replaces an earlier one. That is what
    /// makes a probe — a walk whose diagnostics are discarded and which
    /// always precedes the real walk of the same tree — leave the real
    /// answer behind rather than its own.
    pub(crate) fn record_ty(&mut self, file: FileId, id: ExprId, ty: &Ty) {
        let Some(index) = index_of(id) else {
            return;
        };
        *slot(&mut self.file_mut(file).types, index) = Some(ty.clone());
    }

    /// Records the declaration a call resolved to.
    pub(crate) fn record_target(&mut self, file: FileId, id: ExprId, target: MethodTarget) {
        let Some(index) = index_of(id) else {
            return;
        };
        *slot(&mut self.file_mut(file).targets, index) = Some(target);
    }

    /// Records the boundary the checker resolved for one declaration.
    ///
    /// A later record for the same declaration replaces an earlier one, for
    /// the reason [`Facts::record_ty`] gives: a probe walks a tree before
    /// the real walk of it does, and what is left behind has to be the real
    /// walk's answer.
    pub(crate) fn record_signature(&mut self, file: FileId, decl: Span, signature: Signature) {
        self.signatures.insert((file, decl.start), signature);
    }

    /// Takes over everything `other` recorded.
    ///
    /// A module is checked by a checker of its own, and a file belongs to
    /// exactly one module, so in practice the two tables touch different
    /// files. Merging slot by slot rather than file by file is what keeps
    /// that an observation about the caller instead of an assumption here.
    pub(crate) fn merge(&mut self, other: Facts) {
        for (index, from) in other.files.into_iter().enumerate() {
            let into = self.file_mut(FileId(index as u32));
            merge_table(&mut into.types, from.types);
            merge_table(&mut into.targets, from.targets);
        }
        self.signatures.extend(other.signatures);
    }

    /// The entry for `file`, growing the table until the id is an index into
    /// it.
    fn file_mut(&mut self, file: FileId) -> &mut FileFacts {
        let index = file.0 as usize;
        if self.files.len() <= index {
            self.files.resize_with(index + 1, FileFacts::default);
        }
        &mut self.files[index]
    }
}

/// The index `id` names, or `None` for an id that names no position.
///
/// [`ExprId::UNSET`] is `u32::MAX` and marks a tree that was built by hand
/// rather than parsed. Treating it as an index would grow a table to four
/// billion entries, so it records nothing and reads back as nothing.
fn index_of(id: ExprId) -> Option<usize> {
    (id != ExprId::UNSET).then_some(id.0 as usize)
}

/// The slot at `index`, growing `table` until there is one.
fn slot<T>(table: &mut Vec<Option<T>>, index: usize) -> &mut Option<T> {
    if table.len() <= index {
        table.resize_with(index + 1, || None);
    }
    &mut table[index]
}

/// Writes every recorded entry of `from` over `into`, leaving the rest.
fn merge_table<T>(into: &mut Vec<Option<T>>, from: Vec<Option<T>>) {
    if into.len() < from.len() {
        into.resize_with(from.len(), || None);
    }
    for (slot, value) in into.iter_mut().zip(from) {
        if value.is_some() {
            *slot = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use cove_diag::SourceMap;
    use cove_syntax::ast::{
        Arg, Block, Expr, ExprKind, Item, ItemKind, MatchArm, Param, Pattern, PatternKind,
        SourceUnit, StmtKind, StrPart,
    };

    use super::*;
    use crate::compile::Compiler;
    use crate::config::Config;
    use crate::package::{Module, Package, Unit};
    use crate::resolve::resolve;
    use crate::typeck::{check_facts, Ty};

    /// A package written inline, and everything a fact about it is read
    /// against.
    struct Checked {
        sources: SourceMap,
        package: Package,
        facts: Facts,
    }

    impl Checked {
        /// The id of the file `module` was written in.
        fn file(&self, module: &str) -> FileId {
            self.package.modules[module].units[0].file
        }

        /// The tree of `module`.
        fn unit(&self, module: &str) -> &SourceUnit {
            &self.package.modules[module].units[0].ast
        }

        /// The source `expr` was written as.
        fn text(&self, module: &str, expr: &Expr) -> &str {
            let file = self.sources.get(self.file(module));
            &file.text[expr.span.start as usize..expr.span.end as usize]
        }

        /// The one expression of `module` written exactly as `source`.
        ///
        /// Naming an expression by its own text is what keeps a test about
        /// the fact rather than about the numbering: nothing here has to
        /// know which id the parser handed out.
        #[track_caller]
        fn id(&self, module: &str, source: &str) -> ExprId {
            let mut found: Vec<ExprId> = Vec::new();
            for expr in collect(self.unit(module)) {
                if self.text(module, &expr) == source {
                    found.push(expr.id);
                }
            }
            assert_eq!(
                found.len(),
                1,
                "`{source}` names {} expressions of `{module}`, and a test names one",
                found.len()
            );
            found[0]
        }

        /// The type recorded for the one expression written as `source`.
        #[track_caller]
        fn ty(&self, module: &str, source: &str) -> &Ty {
            let id = self.id(module, source);
            self.facts
                .ty(self.file(module), id)
                .unwrap_or_else(|| panic!("nothing was recorded for `{source}`"))
        }

        /// The target recorded for the one expression written as `source`.
        #[track_caller]
        fn target(&self, module: &str, source: &str) -> Option<&MethodTarget> {
            let id = self.id(module, source);
            self.facts.target(self.file(module), id)
        }

        /// The signature recorded for the declaration of `module` named
        /// `name`, which is `Type.method` for a method.
        ///
        /// The declaration is found in the tree rather than in a table of
        /// the checker's, so a test reads the fact through the same span a
        /// consumer holding a `FnDecl` would read it through.
        #[track_caller]
        fn signature(&self, module: &str, name: &str) -> &Signature {
            let mut found: Option<Span> = None;
            for item in &self.unit(module).items {
                match &item.kind {
                    ItemKind::Fn(decl) if decl.name.node == name => found = Some(decl.span),
                    ItemKind::Impl(block) => {
                        for item in &block.items {
                            if let ItemKind::Fn(decl) = &item.kind {
                                if format!("{}.{}", block.type_name.node, decl.name.node) == name {
                                    found = Some(decl.span);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            let span = found.unwrap_or_else(|| panic!("`{module}` declares no `{name}`"));
            self.facts
                .signature(self.file(module), span)
                .unwrap_or_else(|| panic!("nothing was recorded for `{name}`"))
        }

        /// A signature as `receiver | params -> ret`, written out so a test
        /// reads as the declaration does.
        #[track_caller]
        fn written(&self, module: &str, name: &str) -> String {
            let signature = self.signature(module, name);
            let params: Vec<String> = signature.params.iter().map(Ty::to_string).collect();
            let receiver = match &signature.receiver {
                Some(ty) => format!("{ty} | "),
                None => String::new(),
            };
            format!("{receiver}({}) -> {}", params.join(", "), signature.ret)
        }
    }

    /// Resolves and checks modules written inline, the way the pipeline
    /// does, so the facts under test are the ones a consumer receives.
    #[track_caller]
    fn compile(modules: &[(&str, &str)]) -> Checked {
        let (sources, package) = packaged(modules);
        let program = Compiler::new()
            .compile(&package)
            .unwrap_or_else(|errors| panic!("test package checks: {}", first(&errors)));
        Checked {
            sources,
            package,
            facts: program.facts,
        }
    }

    /// Checks modules written inline that are not expected to check, and
    /// hands back what the checker recorded anyway.
    ///
    /// A package with an error never reaches [`Program::facts`], and the
    /// facts are still the ones the check produced, because the recording
    /// and the reporting are the same walk.
    #[track_caller]
    fn check_anyway(modules: &[(&str, &str)]) -> Checked {
        let (sources, package) = packaged(modules);
        let program = resolve(&package).expect("test package resolves");
        let (_, facts) = check_facts(&package, &program, Compiler::new().host_schemas());
        Checked {
            sources,
            package,
            facts,
        }
    }

    fn packaged(modules: &[(&str, &str)]) -> (SourceMap, Package) {
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
        (sources, package)
    }

    fn first(errors: &[cove_diag::Diagnostic]) -> String {
        errors
            .iter()
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect::<Vec<_>>()
            .join("; ")
    }

    // ------------------------------------------------------------- source

    /// A type and the methods written on it, in a module of its own so that
    /// a target read from another module has a module to name.
    const GEOMETRY: &str = r#"/// A point on the plane.
export struct Point {
  x: Float
  y: Float
}

impl Point {
  /// Scales both coordinates.
  export fn scaled(self, by: Float) -> Point {
    Point(x: self.x * by, y: self.y * by)
  }

  /// How many coordinates a point has, which is always two.
  ///
  /// It is named after a builtin method on purpose: `Array` has one too,
  /// and which of the two a call reaches is decided by the receiver's type
  /// and by nothing else.
  export fn length(self) -> Int {
    2
  }

  /// The point both coordinates are measured from.
  export fn origin() -> Point {
    Point(x: 0.0, y: 0.0)
  }
}
"#;

    /// A function written the way one is written: a parameter with a
    /// default, an interpolation, an array, a struct, a method call, a
    /// `match`, a block used for its value, and a loop that assigns.
    const REPORT: &str = r#"use geometry.Point

/// Summarises a batch of readings.
export fn report(readings: Array<Int>, offset: Int = 7) -> String {
  let total = 1.5 + 2.5
  let large = offset > 3
  let label = "offset {offset}"
  let scores = [1, 2, 3]
  let start = Point.origin()
  let moved = start.scaled(by: total)
  let height = moved.y
  let chosen = match offset {
    0 => "none"
    other => label
  }
  let counted = {
    readings.length()
  }
  let both = moved.length() + counted
  var sum = 0
  for score in scores {
    sum = sum + score
  }
  "{label} {large} {height} {chosen} {both} {sum}"
}
"#;

    fn reporting() -> Checked {
        compile(&[("geometry", GEOMETRY), ("main", REPORT)])
    }

    // -------------------------------------------------------------- tests

    /// The table is total over a function, which is the property a consumer
    /// specialising on it depends on: one missing entry is one construct it
    /// silently stops specialising.
    #[test]
    fn every_expression_of_a_function_has_a_recorded_type() {
        let checked = reporting();
        let mut missing: Vec<String> = Vec::new();
        for module in ["geometry", "main"] {
            let file = checked.file(module);
            for expr in collect(checked.unit(module)) {
                if checked.facts.ty(file, expr.id).is_none() {
                    missing.push(format!("{module}: `{}`", checked.text(module, &expr)));
                }
            }
        }
        // Everything the checker types is here, and what is left is exactly
        // the names calls resolve — the two `Point` initializers, the
        // associated function and the type it is reached through, and the
        // three method callees. The list is written out rather than
        // summarised so that a form losing its type fails this, and so that
        // the exception widening is a deliberate edit rather than a silent
        // one. See the module docs for why a name has no type to record.
        assert_eq!(
            missing,
            vec![
                "geometry: `Point`",
                "geometry: `Point`",
                "main: `Point.origin`",
                "main: `Point`",
                "main: `start.scaled`",
                "main: `readings.length`",
                "main: `moved.length`",
            ]
        );
    }

    /// The two callee positions are told apart by whether a type is
    /// recorded, which is what makes the exception above readable rather
    /// than merely tolerable.
    #[test]
    fn a_callee_records_a_type_when_the_call_goes_through_a_value() {
        let source = r#"/// Doc.
fn scale(n: Int) -> Int {
  n * 3
}

/// Doc.
fn raise(n: Int) -> Int {
  n + 1
}

/// Doc.
export fn apply(n: Int) -> Int {
  let f: fn(Int) -> Int = scale
  f(n) + raise(n)
}
"#;
        let checked = compile(&[("main", source)]);
        let file = checked.file("main");

        // `scale` is given to a place, so it is evaluated and typed.
        assert!(matches!(checked.ty("main", "scale"), Ty::Fn(_)));
        // `f` names a binding, so calling it evaluates it.
        assert!(matches!(checked.ty("main", "f"), Ty::Fn(_)));
        // `raise` names a declaration, so calling it resolves a name.
        assert_eq!(checked.facts.ty(file, checked.id("main", "raise")), None);
    }

    #[test]
    fn an_int_literal_records_int() {
        assert_eq!(reporting().ty("main", "7"), &Ty::Int);
    }

    #[test]
    fn a_float_addition_records_float() {
        assert_eq!(reporting().ty("main", "1.5 + 2.5"), &Ty::Float);
    }

    #[test]
    fn a_comparison_records_bool() {
        assert_eq!(reporting().ty("main", "offset > 3"), &Ty::Bool);
    }

    #[test]
    fn a_string_interpolation_records_str() {
        assert_eq!(reporting().ty("main", "\"offset {offset}\""), &Ty::Str);
    }

    #[test]
    fn an_array_literal_records_its_element_type() {
        assert_eq!(
            reporting().ty("main", "[1, 2, 3]"),
            &Ty::Array(Box::new(Ty::Int))
        );
    }

    #[test]
    fn a_struct_field_read_records_the_field_s_type() {
        assert_eq!(reporting().ty("main", "moved.y"), &Ty::Float);
    }

    #[test]
    fn a_call_records_what_it_produces() {
        let checked = reporting();
        assert_eq!(
            checked.ty("main", "start.scaled(by: total)"),
            &Ty::Struct("geometry.Point".into(), Vec::new())
        );
    }

    #[test]
    fn a_match_records_the_type_its_arms_agree_on() {
        let source = "match offset {\n    0 => \"none\"\n    other => label\n  }";
        assert_eq!(reporting().ty("main", source), &Ty::Str);
    }

    #[test]
    fn a_block_records_the_type_of_its_tail() {
        let checked = reporting();
        assert_eq!(checked.ty("main", "readings.length()"), &Ty::Int);
        assert_eq!(
            checked.ty("main", "{\n    readings.length()\n  }"),
            &Ty::Int
        );
    }

    /// An unknown is what the checker settled, not the absence of an answer.
    /// A consumer specialises on a recorded type and leaves an unrecorded id
    /// alone, so the two have to be told apart.
    #[test]
    fn an_abstention_records_an_unknown_and_a_gap_records_nothing() {
        let source = r#"/// Doc.
export fn broken() -> Int {
  missing()
}
"#;
        let checked = check_anyway(&[("main", source)]);
        let file = checked.file("main");
        assert!(
            matches!(checked.ty("main", "missing()"), Ty::Unknown(_)),
            "the checker abstains about a call to a name it cannot find"
        );

        // An id past the end of the file names no expression of it, and a
        // file the check never saw has no entries at all. Both read as
        // "never recorded" rather than as an unknown.
        let past_end = collect(checked.unit("main")).len() as u32;
        assert_eq!(checked.facts.ty(file, ExprId(past_end)), None);
        assert_eq!(checked.facts.ty(file, ExprId::UNSET), None);
        assert_eq!(checked.facts.ty(FileId(file.0 + 1), ExprId(0)), None);
    }

    /// Every file numbers from zero, so an id alone names nothing. Reading
    /// one against the wrong file has to answer that file's expression, not
    /// this one's.
    #[test]
    fn ids_do_not_collide_across_files() {
        let checked = compile(&[
            ("counter", "/// Doc.\nexport fn count() -> Int {\n  1\n}\n"),
            (
                "greeter",
                "/// Doc.\nexport fn greet() -> String {\n  \"hi\"\n}\n",
            ),
        ]);
        assert_eq!(checked.id("counter", "1"), ExprId(0));
        assert_eq!(checked.id("greeter", "\"hi\""), ExprId(0));
        assert_eq!(checked.ty("counter", "1"), &Ty::Int);
        assert_eq!(checked.ty("greeter", "\"hi\""), &Ty::Str);
    }

    /// The receiver's type decides which declaration a call reaches, and it
    /// is the one thing a pass reading the source alone does not have.
    /// `Point` and `Array` both declare `length`.
    #[test]
    fn a_declared_method_records_its_declaration() {
        let checked = reporting();
        assert_eq!(
            checked.target("main", "moved.length()"),
            Some(&MethodTarget {
                module: "geometry".to_string(),
                type_name: "Point".to_string(),
                method: "length".to_string(),
            })
        );
        assert_eq!(
            checked.target("main", "start.scaled(by: total)"),
            Some(&MethodTarget {
                module: "geometry".to_string(),
                type_name: "Point".to_string(),
                method: "scaled".to_string(),
            })
        );
    }

    /// A builtin method belongs to no `impl` block, so there is no
    /// declaration to name and the fact is that there is none.
    #[test]
    fn a_builtin_method_records_no_target() {
        assert_eq!(reporting().target("main", "readings.length()"), None);
    }

    /// An associated function is named through its type rather than a
    /// receiver, and it is a declaration like any other.
    #[test]
    fn an_associated_function_records_its_declaration() {
        assert_eq!(
            reporting().target("main", "Point.origin()"),
            Some(&MethodTarget {
                module: "geometry".to_string(),
                type_name: "Point".to_string(),
                method: "origin".to_string(),
            })
        );
    }

    /// A method of the module being checked names that module, so a target
    /// means the same thing wherever it is read.
    #[test]
    fn a_target_names_the_declaring_module_even_from_inside_it() {
        let checked = reporting();
        assert_eq!(
            checked.target("geometry", "Point(x: self.x * by, y: self.y * by)"),
            None,
            "a struct initializer names a type, not a method"
        );
        let inside = compile(&[(
            "main",
            "/// Doc.\nexport struct Tally {\n  n: Int\n}\n\nimpl Tally {\n  /// Doc.\n  fn bumped(self) -> Tally {\n    Tally(n: self.n + 1)\n  }\n\n  /// Doc.\n  export fn twice(self) -> Tally {\n    self.bumped().bumped()\n  }\n}\n",
        )]);
        assert_eq!(
            inside.target("main", "self.bumped()"),
            Some(&MethodTarget {
                module: "main".to_string(),
                type_name: "Tally".to_string(),
                method: "bumped".to_string(),
            })
        );
    }

    /// A free function's boundary is recorded as the checker resolved it,
    /// which is what a consumer placing arguments reads instead of reading
    /// the annotations again.
    #[test]
    fn a_declarations_signature_is_recorded() {
        assert_eq!(
            reporting().written("main", "report"),
            "(Array<Int>, Int) -> String"
        );
    }

    /// A method's receiver is recorded apart from its parameters, because a
    /// call supplies it first and a consumer must not have to infer that
    /// from a count.
    #[test]
    fn a_methods_signature_records_its_receiver_apart_from_its_parameters() {
        let checked = reporting();
        assert_eq!(
            checked.written("geometry", "Point.scaled"),
            "Point | (Float) -> Point"
        );
        assert_eq!(
            checked.written("geometry", "Point.length"),
            "Point | () -> Int"
        );
        assert_eq!(
            checked.written("geometry", "Point.origin"),
            "() -> Point",
            "an associated function has no receiver"
        );
    }

    /// A declaration with no `->` returns `()`, and `()` is a type. Recording
    /// it as one is what keeps "the checker said nothing" and "the checker
    /// said `Unit`" apart here as everywhere else.
    #[test]
    fn a_declaration_with_no_return_type_records_unit() {
        let checked = compile(&[(
            "main",
            "/// Doc.\nexport fn note(what: String) {\n  let _ignored = what\n}\n",
        )]);
        assert_eq!(checked.written("main", "note"), "(String) -> ()");
    }

    // ------------------------------------------------- an independent walk

    /// Collects every expression of a unit by a walk written here rather
    /// than reused from the checker, so that an expression both of them
    /// forget cannot pass for one neither has.
    fn collect(unit: &SourceUnit) -> Vec<Expr> {
        let mut found = Vec::new();
        for item in &unit.items {
            item_exprs(item, &mut found);
        }
        found
    }

    fn item_exprs(item: &Item, found: &mut Vec<Expr>) {
        match &item.kind {
            ItemKind::Fn(decl) => {
                param_exprs(&decl.params, found);
                block_exprs(&decl.body, found);
            }
            ItemKind::Struct(_) | ItemKind::Enum(_) | ItemKind::TypeAlias(_) => {}
            ItemKind::Trait(decl) => {
                for method in &decl.methods {
                    param_exprs(&method.params, found);
                    if let Some(body) = &method.default {
                        block_exprs(body, found);
                    }
                }
            }
            ItemKind::Impl(block) => {
                for item in &block.items {
                    item_exprs(item, found);
                }
            }
        }
    }

    fn param_exprs(params: &[Param], found: &mut Vec<Expr>) {
        for param in params {
            if let Some(default) = &param.default {
                expr_exprs(default, found);
            }
        }
    }

    fn block_exprs(block: &Block, found: &mut Vec<Expr>) {
        for stmt in &block.statements {
            match &stmt.kind {
                StmtKind::Let { value, .. } => expr_exprs(value, found),
                StmtKind::Expr(value) => expr_exprs(value, found),
                StmtKind::Item(item) => item_exprs(item, found),
            }
        }
        if let Some(tail) = &block.tail {
            expr_exprs(tail, found);
        }
    }

    fn arm_exprs(arm: &MatchArm, found: &mut Vec<Expr>) {
        pattern_exprs(&arm.pattern, found);
        expr_exprs(&arm.body, found);
    }

    fn pattern_exprs(pattern: &Pattern, found: &mut Vec<Expr>) {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Binding(_) => {}
            PatternKind::Literal(value) => expr_exprs(value, found),
            PatternKind::Variant { payload, .. } => {
                for pattern in payload {
                    pattern_exprs(pattern, found);
                }
            }
        }
    }

    fn arg_exprs(args: &[Arg], found: &mut Vec<Expr>) {
        for arg in args {
            expr_exprs(&arg.value, found);
        }
    }

    fn expr_exprs(expr: &Expr, found: &mut Vec<Expr>) {
        found.push(expr.clone());
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Duration(_)
            | ExprKind::Unit
            | ExprKind::Ident(_)
            | ExprKind::Continue => {}
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Interpolation(inner) = part {
                        expr_exprs(inner, found);
                    }
                }
            }
            ExprKind::ArrayLit(items) => {
                for item in items {
                    expr_exprs(item, found);
                }
            }
            ExprKind::Field { base, .. } => expr_exprs(base, found),
            ExprKind::Call {
                callee,
                args,
                trailing,
                ..
            } => {
                expr_exprs(callee, found);
                arg_exprs(args, found);
                if let Some(trailing) = trailing {
                    expr_exprs(trailing, found);
                }
            }
            ExprKind::Unary { operand, .. } => expr_exprs(operand, found),
            ExprKind::Binary { lhs, rhs, .. } => {
                expr_exprs(lhs, found);
                expr_exprs(rhs, found);
            }
            ExprKind::Assign { target, value, .. } => {
                expr_exprs(target, found);
                expr_exprs(value, found);
            }
            ExprKind::Try(inner) | ExprKind::Await(inner) => expr_exprs(inner, found),
            ExprKind::Block(block) => block_exprs(block, found),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                expr_exprs(condition, found);
                block_exprs(then_branch, found);
                if let Some(else_branch) = else_branch {
                    expr_exprs(else_branch, found);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                expr_exprs(scrutinee, found);
                for arm in arms {
                    arm_exprs(arm, found);
                }
            }
            ExprKind::For { iterable, body, .. } => {
                expr_exprs(iterable, found);
                block_exprs(body, found);
            }
            ExprKind::While { condition, body } => {
                expr_exprs(condition, found);
                block_exprs(body, found);
            }
            ExprKind::Return(value) | ExprKind::Break(value) => {
                if let Some(value) = value {
                    expr_exprs(value, found);
                }
            }
            ExprKind::Lambda { params, body, .. } => {
                param_exprs(params, found);
                block_exprs(body, found);
            }
            ExprKind::Scope { body, .. } => block_exprs(body, found),
            ExprKind::Range { start, end, .. } => {
                expr_exprs(start, found);
                expr_exprs(end, found);
            }
        }
    }
}
