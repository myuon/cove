//! The standard library: Cove source embedded in the compiler binary.
//!
//! `cove_schema::builtins::STANDARD_LIBRARY` names which builtin methods have
//! moved out of Rust and into Cove, and points at the module each one lives
//! in. This module is where that module's source
//! actually is: `crates/cove-sema/std/*.cove`, read into the binary with
//! `include_str!` so a checked program never depends on a file existing on
//! disk at some path relative to the running `cove`.
//!
//! # This is the precompile boundary
//!
//! Nothing about the standard library changes between runs, so this is the
//! one function that changes when it is warm enough to matter — to answer a
//! cached checked [`Program`](crate::resolve::Program) or cached IR instead
//! of parsing from scratch — and every caller stays as it is.
//!
//! The first step of that has been measured and taken: [`attach`] parses the
//! library **once per process** for each [`FileId`](cove_diag::FileId) it
//! starts at, and answers every later call that starts at the same one with
//! a copy of the same trees. A host that compiles many packages in one
//! process — the edge server of `examples/edge`, which compiles one per
//! tenant — was spending about 5.6 ms of a 15 ms deploy parsing 8,900 lines
//! it had already parsed, and the tenant's own share of the front end was
//! too small to measure. A `cove` command compiles once, so it parses once,
//! as it always did.
//!
//! What is cached is only what a parse produces, and a parse is a function
//! of the text and the [`FileId`](cove_diag::FileId) its spans carry and
//! nothing else, so a copy is the tree a fresh parse would have built. That
//! is why the key is the starting id: the trees cannot be moved to other ids
//! without rewriting every span in them.
//!
//! Resolving and checking the library — the next 8 ms — is not a cache in
//! this function either. Both run over the whole package at once, so what is
//! kept of them is the library's per-module share, by `Compiler::compile`,
//! and a package links against it: see `crate::library` and
//! [ADR 0083](../../../docs/adr/0083-the-standard-library-is-checked-once-per-process.md).
//! It recognises the library a package holds by the function bodies this
//! function shares, which is one more reason a copy here must share them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use cove_diag::{Diagnostic, SourceMap};

use crate::package::{Module, Unit};
use cove_syntax::ast::{Expr, StrPart};

use crate::typeck::Ty;

/// One embedded standard-library source file and the module it belongs to.
struct StdSource {
    /// Dotted module name, such as `"std.array"`.
    module: &'static str,
    /// A path to show in a diagnostic or a stack trace. Never read from
    /// disk: the text beside it is what is actually parsed.
    path: &'static str,
    /// The file's contents, embedded at compile time.
    text: &'static str,
}

/// Every file the standard library is made of.
///
/// One file per receiver, which is why there are several holding one
/// function each: `Array` and `Vector` cannot share a body without a bound
/// the language does not have, so they do not share a file either. A method
/// migrating out of Rust adds a function to an existing file or a new file
/// here, and an entry to `cove_schema::builtins::STANDARD_LIBRARY` pointing
/// at it.
static SOURCES: &[StdSource] = &[
    StdSource {
        module: "std.array",
        path: "std/array.cove",
        text: include_str!("../std/array.cove"),
    },
    StdSource {
        module: "std.vector",
        path: "std/vector.cove",
        text: include_str!("../std/vector.cove"),
    },
    StdSource {
        module: "std.map",
        path: "std/map.cove",
        text: include_str!("../std/map.cove"),
    },
    StdSource {
        module: "std.set",
        path: "std/set.cove",
        text: include_str!("../std/set.cove"),
    },
    StdSource {
        module: "std.string",
        path: "std/string.cove",
        text: include_str!("../std/string.cove"),
    },
    StdSource {
        module: "std.option",
        path: "std/option.cove",
        text: include_str!("../std/option.cove"),
    },
    StdSource {
        module: "std.result",
        path: "std/result.cove",
        text: include_str!("../std/result.cove"),
    },
    StdSource {
        module: "std.int",
        path: "std/int.cove",
        text: include_str!("../std/int.cove"),
    },
    StdSource {
        module: "std.float",
        path: "std/float.cove",
        text: include_str!("../std/float.cove"),
    },
    StdSource {
        module: "std.duration",
        path: "std/duration.cove",
        text: include_str!("../std/duration.cove"),
    },
    // The one file here that is not a receiver's methods moved out of Rust: it
    // declares a *type*. `StringBuilder` is the public name ADR 0052 leaves to
    // the standard library — "the standard library chooses the public builder
    // name; that name does not appear in IR or VM dispatch" — over the
    // `ByteBuffer` intrinsic the VM does know. So it binds nothing in
    // `cove_schema::builtins::STANDARD_LIBRARY`, and a program reaches it by
    // importing the module like any other.
    //
    // **`std.string` is now one of those programs**, and it is the first
    // module in this list to `use` another: ADR 0064's `fromCodePoint`
    // migration needed somewhere to build a one-character `String`, and the
    // answer was the builder that already existed rather than a new `core.*`.
    // Nothing about the order of this list changed to allow it — a package is
    // parsed and then resolved, so `std.string` may name a module attached
    // after it — and nothing may go the other way: `std.stringbuilder` names no
    // `std.string` function and must not, because a cycle between two
    // standard-library modules is not something this list is ordered to
    // prevent.
    StdSource {
        module: "std.stringbuilder",
        path: "std/stringbuilder.cove",
        text: include_str!("../std/stringbuilder.cove"),
    },
    // The second file that is not a receiver's methods, and the first with no
    // export at all: `std.dynamic.equals` is what `==` answers for two erased
    // values (ADR 0068), and its only caller is the lowering, which resolves it
    // by name the way it resolves `std.int`'s private `renderInto`. It binds
    // nothing in `cove_schema::builtins::STANDARD_LIBRARY` and a program cannot
    // reach it.
    StdSource {
        module: "std.dynamic",
        path: "std/dynamic.cove",
        text: include_str!("../std/dynamic.cove"),
    },
    // The third that is not a receiver's methods, and the first that is a
    // library in the ordinary sense: a value type and the functions over it,
    // which a program imports as `std.json` (ADR 0097). It binds nothing in
    // `cove_schema::builtins::STANDARD_LIBRARY`, and nothing in the lowering
    // names it, so it is on [`IMPORTED`].
    StdSource {
        module: "std.json",
        path: "std/json.cove",
        text: include_str!("../std/json.cove"),
    },
    // The second library in that ordinary sense, and the second module on
    // [`IMPORTED`]: an opaque `Html` and the `fill` that puts a path in every
    // hole of a template (ADR 0099). Like `std.json` it binds nothing in
    // `cove_schema::builtins::STANDARD_LIBRARY` and nothing in the lowering
    // names it — but unlike `std.json` it could be written nowhere else, since
    // the `core.dynamic*` intrinsics `fill` reads its data with are a
    // standard-library privilege (ADR 0069).
    StdSource {
        module: "std.html",
        path: "std/html.cove",
        text: include_str!("../std/html.cove"),
    },
];

/// The embedded text of the standard-library file [`attach`] adds as `path`,
/// such as `"std/array.cove"`, or `None` for a path it does not add.
///
/// For `cove_ir::serial` ([ADR 0077](../../../docs/adr/0077-cove-fmt-is-covefmt.md)):
/// an image of a lowered program names a library file by its path rather than
/// carrying a second copy of text the binary reading it already holds here.
pub fn text_of(path: &str) -> Option<&'static str> {
    SOURCES
        .iter()
        .find(|source| source.path == path)
        .map(|source| source.text)
}

/// Every module name the standard library declares.
///
/// This is `cove_sema::package::load`'s and `Compiler::compile`'s way of
/// asking "is the standard library here?" without parsing anything: a
/// package that already has a module by one of these names either loaded it
/// from `attach` or collides with it, and either way the answer does not
/// require a parse.
pub fn module_names() -> &'static [&'static str] {
    // If a later file adds a second module, dedupe here rather than asking
    // every caller to.
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: Vec<&'static str> = SOURCES.iter().map(|source| source.module).collect();
        names.dedup();
        names
    })
}

/// Whether `module` is one of the standard library's modules.
///
/// This is the one question the privilege of calling a core intrinsic is
/// decided by: the checker resolves `core.byteLength(text)` only in a module
/// this answers `true` for, the lowering emits the intrinsic's instructions
/// only there, and the oracle executes it only there — see
/// `cove_schema::builtins::CORE_INTRINSICS`. It reads [`module_names`], the
/// same list `package::load` and `Compiler::compile` ask, so a module is
/// privileged by being the standard library's rather than by any mark in its
/// source.
pub fn is_library_module(module: &str) -> bool {
    module_names().contains(&module)
}

/// The standard-library modules a program has only by importing them.
///
/// Every other module here is part of the language: `cove_schema` binds a
/// builtin method to it, or the lowering names it — an interpolation's
/// appends are `std.stringbuilder`'s, `==` on two erased values is
/// `std.dynamic`'s. A module on this list is neither. It is a library in the
/// ordinary sense, and a program that does not `use` it does not have it.
///
/// What that changes is a whole-package lowering, `cove_ir::lower`, which
/// lowers every declaration the package holds. It lowers this list's
/// declarations only where the rest of the package reaches them, so a module
/// a program never imports does not appear in its listing, its layouts, or
/// the corpus surveys that count every program's functions (ADR 0097). A
/// sliced lowering — what `cove run` and `cove test` do — never lowered them
/// unreached in the first place.
///
/// `std.html` is named by the lowering, but only to assemble an `html"…"`
/// literal ([`LiteralTag`]), which a module can write only where it imports
/// `std.html`. So a program that never writes one still never reaches it,
/// and it stays here: the lowering's call is a round's `wanted` like any call
/// the graph did not draw, and adds the module's functions only to a package
/// that holds such a literal.
const IMPORTED: &[&str] = &["std.json", "std.html"];

/// A string-literal tag: the module that gives `tag"…"` its meaning, and the
/// functions of that module the lowering assembles one with.
/// [ADR 0101](../../../docs/adr/0101-a-page-is-markup-with-an-expression-in-every-hole.md).
///
/// There is one, `html`, and this table is where that is so. The checker asks
/// [`literal_tag`] whether a tag means anything, and the lowering and the
/// tree-walking oracle call the functions named here and no others — so a
/// second tag would be a second row, and a tag a module declares for itself
/// would be a lookup in place of [`literal_tag`]. What escaping *means* is
/// the module's Cove source; this names only where to find it.
///
/// Every function here is the module's own and not exported: nothing but the
/// assembly of a literal calls them, and a module-private declaration of the
/// standard library is one the lowering may name ([`reaches_private`]).
#[derive(Debug)]
pub struct LiteralTag {
    /// The name written against the quote.
    pub tag: &'static str,
    /// The module the tag must name where it is written.
    pub module: &'static str,
    /// The type the literal answers, declared by `module`.
    pub ty: &'static str,
    /// `(buffer: ByteBuffer, text: String)`: appends text that is not
    /// markup, escaped.
    pub escape: &'static str,
    /// `(buffer: ByteBuffer, piece: <ty>)`: appends a value of the module's
    /// type as it is.
    pub markup: &'static str,
    /// `(buffer: ByteBuffer, pieces: Array<<ty>>)`: the same, for every
    /// element in order.
    pub each: &'static str,
    /// `(said: String) -> <ty>`: the finished text as the module's type.
    pub wrap: &'static str,
    /// `(buffer: ByteBuffer, name: String, value: String)`: an attribute
    /// whose whole value is a hole, `name={expr}` ([`Laid::Attribute`]):
    /// `name` — the whitespace in front of it included — then `="`, `value`
    /// escaped, and `"`. [ADR 0103](../../../docs/adr/0103-in-html-a-line-break-is-spacing-and-an-attribute-value-is-one-hole.md).
    pub attribute: &'static str,
    /// `(buffer: ByteBuffer, name: String, on: Bool)`: the same for a `Bool`
    /// hole, which is `name` alone when `on` and nothing at all otherwise.
    pub flag: &'static str,
}

impl LiteralTag {
    /// The literal's type, qualified as the checker names a struct:
    /// `std.html.Html`.
    pub fn qualified_type(&self) -> String {
        format!("{}.{}", self.module, self.ty)
    }

    /// How a hole whose checked type is `ty` is appended: in whole when the
    /// type says it is already the module's markup, escaped otherwise.
    ///
    /// The type is the one the checker settled for the hole as written, in
    /// the declaration's own terms — a type parameter is a type parameter
    /// whatever an instantiation makes it — so the lowering and the oracle
    /// decide every hole the same way.
    pub fn piece(&self, ty: Option<&Ty>) -> TaggedPiece {
        let markup = |ty: &Ty| matches!(ty, Ty::Struct(name, args) if args.is_empty() && self.is_markup(name));
        match ty {
            Some(ty) if markup(ty) => TaggedPiece::Markup,
            Some(Ty::Array(element)) if markup(element) => TaggedPiece::EachMarkup,
            Some(Ty::Str) => TaggedPiece::Text,
            _ => TaggedPiece::Rendered,
        }
    }

    /// How an attribute hole, `name={expr}`, whose checked type is `ty` is
    /// written: ADR 0103's table. A `Bool` is there or not, an `Option` is
    /// there for `Some` and its payload is the value, markup is refused, and
    /// anything else is the value — a `String` as it is, the rest rendered.
    pub fn attribute(&self, ty: Option<&Ty>) -> AttributeValue {
        let value = |ty: Option<&Ty>| match self.piece(ty) {
            TaggedPiece::Markup | TaggedPiece::EachMarkup => None,
            piece => Some(piece),
        };
        match ty {
            Some(Ty::Bool) => AttributeValue::Flag,
            Some(Ty::Option(inner)) => {
                value(Some(inner)).map_or(AttributeValue::Markup, AttributeValue::Optional)
            }
            ty => value(ty).map_or(AttributeValue::Markup, AttributeValue::Value),
        }
    }

    fn is_markup(&self, name: &str) -> bool {
        name.strip_prefix(self.module)
            .and_then(|rest| rest.strip_prefix('.'))
            == Some(self.ty)
    }
}

/// What [`LiteralTag::piece`] answers for one hole of a tagged literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaggedPiece {
    /// The module's own type: appended as it is, by [`LiteralTag::markup`].
    Markup,
    /// An `Array` of it: each element appended as it is, by
    /// [`LiteralTag::each`].
    EachMarkup,
    /// A `String`: escaped straight into the buffer, by
    /// [`LiteralTag::escape`].
    Text,
    /// Anything else: rendered exactly as a plain literal renders it, and
    /// that text escaped.
    Rendered,
}

/// What [`LiteralTag::attribute`] answers for an attribute hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeValue {
    /// A `Bool`: the name alone, or nothing, by [`LiteralTag::flag`].
    Flag,
    /// An `Option`: nothing for `None`, and for `Some` its payload written
    /// as [`AttributeValue::Value`] writes a value whose piece is this one.
    Optional(TaggedPiece),
    /// Anything else: by [`LiteralTag::attribute`], over the value itself
    /// when the piece is [`TaggedPiece::Text`] and over its rendering when
    /// it is [`TaggedPiece::Rendered`].
    Value(TaggedPiece),
    /// Markup — the module's type, an `Array` of it, or an `Option` of
    /// either — which an attribute's value cannot be: refused by the checker
    /// as `cove::type::markup_in_attribute`.
    Markup,
}

/// One piece of a tagged literal as its assembly lays it out: ADR 0101's
/// text and holes, with ADR 0103's attribute holes told apart.
#[derive(Clone, Copy, Debug)]
pub enum Laid<'a> {
    /// Literal text, appended as it is. In front of an attribute hole it is
    /// the text with the attribute's whitespace, name and `=` taken off.
    Text(&'a str),
    /// A hole, appended as [`LiteralTag::piece`] says.
    Hole(&'a Expr),
    /// `name={expr}`: a hole that is an attribute's whole value, written as
    /// [`LiteralTag::attribute`] says. `name` is the attribute's name with
    /// the whitespace written in front of it, exactly as in the source.
    Attribute { name: &'a str, hole: &'a Expr },
}

/// Where a position in a literal's text is, as far as [`lay_out`] reads
/// markup: between tags, inside a tag, or inside a quoted value in a tag.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Within {
    Text,
    Tag,
    Quoted(u8),
}

/// Whether `byte` may be part of an attribute's name, as ADR 0103 reads one.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
}

/// `parts` laid out for assembly under a markup tag, ADR 0103.
///
/// The text is read just far enough to know whether a hole is **inside a
/// tag**: `<` followed by an ASCII letter, `/` or `!` opens one, `>` closes
/// it, and inside it `"` and `'` open and close a quoted value, within which
/// `>` is text. A hole changes none of that. A hole is an attribute's whole
/// value when it is inside a tag and outside quotes, the text in front of it
/// ends in whitespace, a name and `=`, and the text after it begins with
/// whitespace, `>` or `/`. Every other hole — between tags, inside quotes,
/// against another hole — is a [`Laid::Hole`], exactly as ADR 0101 lowers
/// it.
///
/// That is all the HTML the compiler knows: no element names, no nesting, and
/// nothing is refused for the way it is written.
pub fn lay_out(parts: &[StrPart]) -> Vec<Laid<'_>> {
    let mut within = Within::Text;
    let mut laid = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        let hole = match part {
            StrPart::Text(text) => {
                let bytes = text.as_bytes();
                for (at, &byte) in bytes.iter().enumerate() {
                    within = match within {
                        Within::Text
                            if byte == b'<'
                                && bytes.get(at + 1).is_some_and(|next| {
                                    next.is_ascii_alphabetic() || matches!(next, b'/' | b'!')
                                }) =>
                        {
                            Within::Tag
                        }
                        Within::Tag if byte == b'>' => Within::Text,
                        Within::Tag if matches!(byte, b'"' | b'\'') => Within::Quoted(byte),
                        Within::Quoted(quote) if byte == quote => Within::Tag,
                        unchanged => unchanged,
                    };
                }
                laid.push(Laid::Text(text));
                continue;
            }
            StrPart::Interpolation(hole) => hole,
        };
        let followed = matches!(
            parts.get(index + 1),
            Some(StrPart::Text(after))
                if after.bytes().next().is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
        );
        let attribute = match (within, followed, laid.last_mut()) {
            (Within::Tag, true, Some(Laid::Text(before))) => attribute_name(before).map(|at| {
                let text: &str = before;
                *before = &text[..at];
                &text[at..text.len() - 1]
            }),
            _ => None,
        };
        laid.push(match attribute {
            Some(name) => Laid::Attribute { name, hole },
            None => Laid::Hole(hole),
        });
    }
    laid
}

/// Where the attribute `text` ends in begins — its whitespace, then its
/// name, then `=` — or `None` when `text` does not end in one.
fn attribute_name(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let equals = bytes.len().checked_sub(1).filter(|at| bytes[*at] == b'=')?;
    let name = bytes[..equals]
        .iter()
        .rposition(|byte| !is_name_byte(*byte))
        .map_or(0, |at| at + 1);
    let space = bytes[..name]
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |at| at + 1);
    (name < equals && space < name).then_some(space)
}

/// Every tag a string literal may carry. See [`LiteralTag`].
const LITERAL_TAGS: &[LiteralTag] = &[LiteralTag {
    tag: "html",
    module: "std.html",
    ty: "Html",
    escape: "appendEscaped",
    markup: "appendMarkup",
    each: "appendEach",
    wrap: "assembled",
    attribute: "appendAttribute",
    flag: "appendFlag",
}];

/// The tag `tag` names, or `None` when a string literal may not carry it.
///
/// **The one place a tag is accepted or refused**, ADR 0101's "the check that
/// the tag is `html` sits in one place". Whether the name also resolves to the
/// tag's module where it is written is the checker's question.
pub fn literal_tag(tag: &str) -> Option<&'static LiteralTag> {
    LITERAL_TAGS.iter().find(|known| known.tag == tag)
}

/// The tags there are, for a diagnostic that names them.
pub fn literal_tags() -> impl Iterator<Item = &'static LiteralTag> {
    LITERAL_TAGS.iter()
}

/// Whether `module` is a standard-library module a program has only by
/// importing it: the modules named in this file's `IMPORTED`, for the reason
/// written there.
pub fn is_imported_library(module: &str) -> bool {
    IMPORTED.contains(&module)
}

/// Whether module `from` may name `owner`'s declarations that are not
/// exported: only when both are the standard library's.
///
/// The standard library is one body of trusted code written as several
/// modules, and a module-private function in one of them is private to the
/// library rather than to its file: `std.dynamic`'s rendering of an erased
/// value writes an `Int`, a `Float` and a `Duration` through `std.int`'s,
/// `std.float`'s and `std.duration`'s own `renderInto`, which the lowering
/// already calls by name and which no program may call at all (issue #499's
/// decision 6). So this is a privilege on the footing of `core.*`, decided by
/// [`is_library_module`] and by nothing in the source, and it is asked
/// wherever an `export` is: a `use` of one declaration, a qualified name
/// through a module imported whole, and the call graph's edge for either. A
/// program's module is refused a library module's private declaration as it
/// always was, and a library module is refused a program's.
pub fn reaches_private(from: &str, owner: &str) -> bool {
    is_library_module(from) && is_library_module(owner)
}

/// Adds the standard library's sources to `sources` and answers the modules
/// to put in a package.
///
/// This must add to the *caller's* [`SourceMap`] rather than one of its own:
/// a [`Span`](cove_diag::Span) is an offset into whichever `SourceMap` it was
/// built against, and a diagnostic built from a span into a different map
/// than the one rendering it would point at the wrong file entirely. Calling
/// this is therefore always `attach(&mut sources)` where `sources` is the
/// same map the rest of the package's units are already in.
///
/// Adds the standard library to a package a host is composing.
///
/// This is the one call an embedder makes. `cove_sema::package::load` makes
/// it for a package read off disk; a host that composes its own — because
/// its sources are embedded, or generated, or come from somewhere that is
/// not a directory — makes it itself, and this is the whole of that step:
///
/// ```no_run
/// # use std::collections::BTreeMap;
/// # use cove_diag::SourceMap;
/// # use cove_sema::package::Module;
/// # fn f(sources: &mut SourceMap, modules: &mut BTreeMap<String, Module>) -> Result<(), Vec<cove_diag::Diagnostic>> {
/// cove_sema::stdlib::install(sources, modules)?;
/// # Ok(())
/// # }
/// ```
///
/// It is not done inside [`crate::Compiler::compile`], and that is a
/// decision rather than an omission: what `compile` is given should be a
/// package that is already whole, dependencies and all, so that what checks
/// is what the host assembled. `compile` refuses a package missing a module
/// `cove_schema::builtins::STANDARD_LIBRARY` names — see the diagnostic
/// `cove::compile::missing_stdlib`, which says to call this.
pub fn install(
    sources: &mut SourceMap,
    modules: &mut std::collections::BTreeMap<String, Module>,
) -> Result<(), Vec<Diagnostic>> {
    for (name, module) in attach(sources)? {
        modules.insert(name, module);
    }
    Ok(())
}

/// See the module doc for what this function is allowed to become without
/// its callers changing.
///
/// The library's files are added to `sources` on every call. Whether they
/// are parsed again depends on where they land: the trees parsed the first
/// time they were added at these ids are answered again, as a copy, and a
/// function declaration's body is shared rather than copied — nothing
/// writes to one after the parse numbers it.
pub fn attach(sources: &mut SourceMap) -> Result<Vec<(String, Module)>, Vec<Diagnostic>> {
    let base = sources.len() as u32;
    if let Some(parsed) = parsed_at(base) {
        for source in SOURCES {
            sources.add_library(PathBuf::from(source.path), source.text);
        }
        return Ok(parsed.to_vec());
    }
    let modules = parse(sources)?;
    remember(base, &modules);
    Ok(modules)
}

/// How many starting ids [`attach`] keeps a parse for.
///
/// Every package a host composes the same way starts the library at the
/// same id — a tenant of one file at 1 — so one entry serves a host
/// compiling thousands of packages alike; the bound is for a process that
/// compiles packages of many different file counts, which then parses as it
/// always did rather than holding a copy of the trees per count.
const PARSED_LIMIT: usize = 8;

/// The parses [`attach`] has kept, by the id the library's first file had.
type Parsed = HashMap<u32, Arc<[(String, Module)]>>;

fn parsed() -> &'static Mutex<Parsed> {
    static PARSED: OnceLock<Mutex<Parsed>> = OnceLock::new();
    PARSED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The library as parsed starting at `base`, if [`attach`] has parsed it there.
fn parsed_at(base: u32) -> Option<Arc<[(String, Module)]>> {
    parsed()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&base)
        .cloned()
}

fn remember(base: u32, modules: &[(String, Module)]) {
    let mut parsed = parsed()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if parsed.len() < PARSED_LIMIT {
        parsed.entry(base).or_insert_with(|| modules.into());
    }
}

/// Adds the library's files to `sources` and parses them, as [`attach`] did
/// before it kept anything.
fn parse(sources: &mut SourceMap) -> Result<Vec<(String, Module)>, Vec<Diagnostic>> {
    let mut modules = Vec::with_capacity(SOURCES.len());
    let mut diagnostics = Vec::new();
    for source in SOURCES {
        let path = PathBuf::from(source.path);
        // As a library file, which is what makes a runtime error raised in
        // this body blame the caller's line rather than this one; see
        // `SourceMap::add_library`.
        let file = sources.add_library(path.clone(), source.text);
        match cove_syntax::parse_file(sources, file) {
            Ok(ast) => modules.push((
                source.module.to_string(),
                Module {
                    name: source.module.to_string(),
                    dir: path.clone(),
                    units: vec![Unit { file, path, ast }],
                },
            )),
            Err(errs) => diagnostics.extend(errs),
        }
    }
    if diagnostics.is_empty() {
        Ok(modules)
    } else {
        Err(diagnostics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADR 0103: an attribute's whole value is a hole.

    /// The parts of a literal written as `text{name}text…`, each hole a bare
    /// name.
    fn parts_of(written: &str) -> Vec<StrPart> {
        let mut parts = Vec::new();
        let mut rest = written;
        while let Some(open) = rest.find('{') {
            let close = open + rest[open..].find('}').expect("a closed hole");
            if open > 0 {
                parts.push(StrPart::Text(rest[..open].to_string()));
            }
            parts.push(StrPart::Interpolation(Expr {
                id: cove_syntax::ast::ExprId::UNSET,
                kind: cove_syntax::ast::ExprKind::Ident(rest[open + 1..close].to_string()),
                span: cove_diag::Span::new(cove_diag::FileId(0), 0, 0),
                parens: 0,
            }));
            rest = &rest[close + 1..];
        }
        if !rest.is_empty() {
            parts.push(StrPart::Text(rest.to_string()));
        }
        parts
    }

    /// [`lay_out`] written back as text: an attribute hole as `[name|hole]`,
    /// any other hole as `{hole}`.
    fn laid(written: &str) -> String {
        lay_out(&parts_of(written))
            .into_iter()
            .map(|laid| {
                let hole = |expr: &Expr| match &expr.kind {
                    cove_syntax::ast::ExprKind::Ident(name) => name.clone(),
                    _ => unreachable!(),
                };
                match laid {
                    Laid::Text(text) => text.to_string(),
                    Laid::Hole(expr) => format!("{{{}}}", hole(expr)),
                    Laid::Attribute { name, hole: expr } => format!("[{name}|{}]", hole(expr)),
                }
            })
            .collect()
    }

    #[test]
    fn a_hole_that_is_an_attributes_whole_value_is_an_attribute() {
        // The whitespace in front of the name goes with it, as written.
        assert_eq!(laid("<a href={x}>"), "<a[ href|x]>");
        assert_eq!(laid("<a  href={x}>"), "<a[  href|x]>");
        assert_eq!(laid("<input\tchecked={on}/>"), "<input[\tchecked|on]/>");
        assert_eq!(laid("<i x={a} y={b}>"), "<i[ x|a][ y|b]>");
        // A name is letters, digits, `-`, `_`, `:` and `.`.
        assert_eq!(
            laid("<a aria-current={c} xml:lang={l} a.b_c={d}>"),
            "<a[ aria-current|c][ xml:lang|l][ a.b_c|d]>"
        );
        // A closing tag, a declaration and an unquoted value all close.
        assert_eq!(laid("</a x={y}>"), "</a[ x|y]>");
        assert_eq!(laid("<!x y={z}>"), "<!x[ y|z]>");
        assert_eq!(laid("<a class=x href={y}>"), "<a class=x[ href|y]>");
        // A quoted value before it is closed by then.
        assert_eq!(
            laid("<a title=\"a>b\" href={y}>"),
            "<a title=\"a>b\"[ href|y]>"
        );
        assert_eq!(laid("<a title='x' href={y}>"), "<a title='x'[ href|y]>");
    }

    #[test]
    fn every_other_hole_is_a_hole() {
        // Between tags, even written `a={x}`.
        assert_eq!(laid("<p>a={x} b</p>"), "<p>a={x} b</p>");
        assert_eq!(laid("a={x} b"), "a={x} b");
        // Inside a quoted value.
        assert_eq!(laid("<p class=\"s {x}\">"), "<p class=\"s {x}\">");
        assert_eq!(laid("<p class=\" a={x} \">"), "<p class=\" a={x} \">");
        assert_eq!(laid("<p class=' a={x} '>"), "<p class=' a={x} '>");
        // Not followed by whitespace, `>` or `/`.
        assert_eq!(laid("<p class={x}y>"), "<p class={x}y>");
        assert_eq!(laid("<p class={x}{y}>"), "<p class={x}{y}>");
        assert_eq!(laid("<p class={x}"), "<p class={x}");
        // Not preceded by whitespace and a name.
        assert_eq!(laid("<p ={x}>"), "<p ={x}>");
        assert_eq!(laid("<p{a}b={x}>"), "<p{a}b={x}>");
        assert_eq!(laid("<p class=\"a\"b={x}>"), "<p class=\"a\"b={x}>");
        // Not a tag: `<` against a space or a digit, or a tag already closed.
        assert_eq!(laid("1 < 2 a={x} b"), "1 < 2 a={x} b");
        assert_eq!(laid("<2 a={x} b"), "<2 a={x} b");
        assert_eq!(laid("<p> a={x} </p>"), "<p> a={x} </p>");
        // A hole in place of the whole tag's contents, and markup between.
        assert_eq!(laid("<p {x}>{y}</p>"), "<p {x}>{y}</p>");
    }

    #[test]
    fn an_attributes_value_is_decided_by_its_type() {
        let html = literal_tag("html").expect("the html tag");
        let markup = Ty::Struct("std.html.Html".into(), Vec::new());
        let markups = Ty::Array(Box::new(markup.clone()));
        assert_eq!(html.attribute(Some(&Ty::Bool)), AttributeValue::Flag);
        assert_eq!(
            html.attribute(Some(&Ty::Str)),
            AttributeValue::Value(TaggedPiece::Text)
        );
        assert_eq!(
            html.attribute(Some(&Ty::Int)),
            AttributeValue::Value(TaggedPiece::Rendered)
        );
        assert_eq!(
            html.attribute(Some(&Ty::Option(Box::new(Ty::Str)))),
            AttributeValue::Optional(TaggedPiece::Text)
        );
        assert_eq!(
            html.attribute(Some(&Ty::Option(Box::new(Ty::Bool)))),
            AttributeValue::Optional(TaggedPiece::Rendered)
        );
        for refused in [
            markup.clone(),
            markups.clone(),
            Ty::Option(Box::new(markup)),
            Ty::Option(Box::new(markups)),
        ] {
            assert_eq!(html.attribute(Some(&refused)), AttributeValue::Markup);
        }
    }

    /// A kept parse is the parse: the same trees, with the same spans and
    /// the same expression ids, as parsing the text again at the same ids —
    /// both for a map the library is the first thing in and for one that
    /// already holds a file of the program's.
    #[test]
    fn a_kept_parse_is_what_parsing_again_would_build() {
        for before in [0, 1, 3] {
            let fresh = |sources: &mut SourceMap| {
                for i in 0..before {
                    sources.add(format!("app{i}.cove"), "");
                }
            };
            let mut first = SourceMap::new();
            fresh(&mut first);
            let kept = attach(&mut first).expect("the embedded standard library parses");
            let mut again = SourceMap::new();
            fresh(&mut again);
            let reused = attach(&mut again).expect("the embedded standard library parses");
            let mut reference = SourceMap::new();
            fresh(&mut reference);
            let parsed = parse(&mut reference).expect("the embedded standard library parses");
            assert_eq!(format!("{kept:?}"), format!("{parsed:?}"));
            assert_eq!(format!("{reused:?}"), format!("{parsed:?}"));
            assert_eq!(again.len(), reference.len());
            for (a, b) in again.files().zip(reference.files()) {
                assert_eq!((a.id, &a.path, &a.text), (b.id, &b.path, &b.text));
                assert_eq!(again.is_library(a.id), reference.is_library(b.id));
            }
        }
    }

    #[test]
    fn attaches_every_module_it_names() {
        let mut sources = SourceMap::new();
        let modules = attach(&mut sources).expect("the embedded standard library parses");
        let mut names: Vec<&str> = modules.iter().map(|(name, _)| name.as_str()).collect();
        names.sort();
        let mut expected: Vec<&str> = module_names().to_vec();
        expected.sort();
        assert_eq!(names, expected);
    }

    /// No function the schema binds to `Receiver.method` calls
    /// `Receiver.method` on its own receiver.
    ///
    /// A binding turns every `text.byteLength()` into a call to
    /// `std.string.byteLength`, *including the one inside that function*. So a
    /// body that moved out of Rust and still wrote the method it replaced —
    /// `text.byteLength()` where it meant `core.byteLength(text)` — would
    /// check, lower, and recurse until the stack ran out, on both evaluators
    /// alike, and the differential corpus would call that agreement.
    ///
    /// It reads the source of each bound body for the receiver parameter —
    /// or, for an associated function, the type's name — followed by
    /// `.method(`. That is a syntactic guard rather than a typed one, and it
    /// is conservative in the direction that matters: it cannot miss the
    /// written call, and a false alarm is a line to rephrase.
    #[test]
    fn no_bound_function_calls_the_method_it_implements() {
        let mut sources = SourceMap::new();
        let modules = attach(&mut sources).expect("the embedded standard library parses");
        for binding in cove_schema::builtins::standard_library() {
            let (_, module) = modules
                .iter()
                .find(|(name, _)| name == binding.module)
                .unwrap_or_else(|| panic!("no embedded module named `{}`", binding.module));
            for unit in &module.units {
                for item in &unit.ast.items {
                    let cove_syntax::ast::ItemKind::Fn(decl) = &item.kind else {
                        continue;
                    };
                    if decl.name.node != binding.function {
                        continue;
                    }
                    let head = match binding.kind {
                        cove_schema::builtins::StdBindingKind::Method => decl
                            .params
                            .first()
                            .map(|param| param.name.node.clone())
                            .unwrap_or_else(|| {
                                panic!(
                                    "`{}.{}` is bound as a method and takes no receiver",
                                    binding.module, binding.function
                                )
                            }),
                        cove_schema::builtins::StdBindingKind::Associated => {
                            binding.receiver.to_string()
                        }
                    };
                    let span = decl.body.span;
                    let body = &sources.get(span.file).text[span.start as usize..span.end as usize];
                    let written = format!("{head}.{}(", binding.method);
                    assert!(
                        !body.contains(&written),
                        "`{}.{}` implements `{}.{}` and calls `{written}...)` itself, which the \
                         binding resolves back to this function",
                        binding.module,
                        binding.function,
                        binding.receiver,
                        binding.method
                    );
                }
            }
        }
    }

    #[test]
    fn declares_every_function_the_schema_binds_to_it() {
        let mut sources = SourceMap::new();
        let modules = attach(&mut sources).expect("the embedded standard library parses");
        for binding in cove_schema::builtins::standard_library() {
            let (_, module) = modules
                .iter()
                .find(|(name, _)| name == binding.module)
                .unwrap_or_else(|| panic!("no embedded module named `{}`", binding.module));
            let declares = module.units.iter().any(|unit| {
                unit.ast.items.iter().any(|item| {
                    matches!(
                        &item.kind,
                        cove_syntax::ast::ItemKind::Fn(decl)
                            if decl.name.node == binding.function
                    )
                })
            });
            assert!(
                declares,
                "`{}.{}` names `{}.{}`, which that module does not declare",
                binding.receiver, binding.method, binding.module, binding.function
            );
        }
    }
}
