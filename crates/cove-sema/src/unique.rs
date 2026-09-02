//! The conservative local uniqueness proof `Vector.freeze()` needs.
//!
//! [ADR 0001](../../../docs/adr/0001-mvp-language-design.md) has said from
//! the beginning what this pass is:
//!
//! > `Vector.freeze()` consumes a vector with uniquely owned storage and
//! > returns an `Array<T>` in O(1). **The compiler only performs
//! > conservative, local uniqueness checking for this explicit transition.**
//! > If uniqueness cannot be proved, `toArray()` creates an independent O(n)
//! > immutable array.
//!
//! Until [issue #240](https://github.com/myuon/cove/issues/240) that sentence
//! described nobody: the tree-walking interpreter counted `Rc` handles at run
//! time and refused there, which is a different thing with a different
//! failure time and a different blast radius. It also cannot be carried
//! anywhere else. A handle in the linear-memory machine is a word, words are
//! not counted, and the sharing bit that could have carried the answer went
//! out with the copy-on-write design. So the choice was to give up the O(1)
//! transition or to establish uniqueness where the language always said it
//! was established — in the compiler — and this is the second one.
//!
//! # What is proved
//!
//! For one `freeze()` call, that the vector it consumes is reached through
//! exactly one place, and that the place is not read again afterwards. The
//! four conditions are #240's own list:
//!
//! - it **originates at a locally known creation** — `Vector.of(...)`,
//!   `array.toVector()`, `vector.snapshot()`, or a field initialised by one
//!   in a struct literal this body wrote;
//! - it has **not been copied to another live place** — no `let`/`var` binds
//!   it, no assignment writes it anywhere else;
//! - it has **not escaped** — no closure captures it, no `return` carries it
//!   out, nothing stores it in another value, and no call it is passed to can
//!   keep it;
//! - it is **consumed** by the `freeze()` and **not used afterward**.
//!
//! One more condition is not on that list and belongs to a static pass rather
//! than a dynamic one: the site must not be somewhere that **runs twice**. A
//! `freeze()` in a loop body or a closure body, over storage created outside
//! it, would consume on the first turn what the second turn would find gone,
//! so the binding has to be created inside the same region the site is in.
//!
//! # What is deliberately not treated as an escape, and why each is safe
//!
//! Four positions read a place without keeping the handle. Treating them as
//! escapes would refuse most of the corpus for nothing:
//!
//! - **a method call's receiver.** `items.push(n)` and `items.length()` write
//!   and read through the handle; neither stores it. The exception is a
//!   *declared* method whose result can reach a `Vector`, which may be a
//!   getter handing the field back — that is an escape.
//! - **a string interpolation operand.** `"{items}"` formats the value and
//!   keeps nothing.
//! - **a `for` loop's iterable.** Iterating reads elements out; the sequence
//!   is not retained.
//! - **a by-value call argument**, when the call has no way to keep it. This
//!   is the one that needs an argument rather than an observation, and the
//!   argument is the language's own: a `Vector` is not task-safe, so it
//!   cannot be put in a `Shared` or carried into a `spawn`, and the Host API
//!   boundary materialises a `Value`, so a host cannot hold one either. The
//!   ways out of a callee are therefore its result, a `var` parameter, and
//!   another operand it could write into. So `firstFree(seed, w, h, cells)`
//!   — whose result is an `Int` and whose other operands hold no vector —
//!   keeps nothing, and `into.push(cells)` does. A call whose result can
//!   reach a `Vector`, a call with a `var` argument, and a call where some
//!   *other* operand can reach a `Vector` are all escapes.
//!
//! # The one obligation that crosses a call
//!
//! A builder's `finish` is the shape that made a purely intraprocedural pass
//! insufficient:
//!
//! ```cove
//! fn finish(var self) -> Router {
//!   Router(routes: self.routes.freeze())
//! }
//! ```
//!
//! `self.routes` does not originate here, so this body cannot prove anything
//! about it — and refusing would refuse `examples/values` and
//! `examples/callbacks`, both of which are demonstrating the language's own
//! rule. What the body *can* do is state the condition it needs and make its
//! callers prove it. So a method that freezes a path rooted at `var self`
//! becomes a method that **demands a uniquely owned receiver**, and every
//! call to it is checked by the same local proof, on the receiver place, at
//! the call site. `fresh.finish()` on a draft this body built passes;
//! `original.finish()` after `var alias = original` does not.
//!
//! The demand is deliberately narrow — a `var self` receiver of a method
//! written in a plain `impl` block, and nothing else. That is the only
//! declaration form whose every call site the checker resolves precisely
//! ([`Facts::target`]), so it is the only one where the obligation cannot be
//! lost. A `freeze()` rooted at an ordinary parameter, at a captured name, or
//! at the receiver of a trait method is refused rather than propagated,
//! because a call through a bound or through `dyn` names no declaration and
//! there would be nowhere to discharge it.
//!
//! There is one way an obligation can be lost, and it is worth naming rather
//! than leaving implicit: a call whose receiver type the checker declined to
//! settle records no target, so a `finish()` reached through a value of
//! unknown type is not checked. That needs a receiver the checker abstained
//! about — a Host API result a schema declared `Any` — reaching a method of a
//! declared type, and no program in the corpus does it. Closing it would mean
//! refusing every unresolved call that shares a name with a demanding method,
//! which is a diagnostic about a coincidence of names; it is left open, and
//! written down here, until a program asks for it.
//!
//! # What it refuses that the oracle admits
//!
//! Conservative means this list is not empty, and the diagnostic's job is to
//! make each entry legible rather than mysterious. The ones that showed up
//! while this was written:
//!
//! - **storage a call produced.** `var log = freshVector()` then
//!   `log.freeze()` is refused: the initialiser is a call, and whether its
//!   answer is fresh is a fact about another body. Proving it would be an
//!   "answers unaliased storage" summary, which nothing in the corpus asks
//!   for yet.
//! - **storage an assignment brought in.** `log = lines` gives `log` whatever
//!   the caller is holding, and this refuses `log.freeze()` afterwards —
//!   correctly, in that case.
//! - **a `var` parameter that is not `self`.** The obligation only travels
//!   back through a method receiver, so `fn take(var v: Vector<Int>) { ... v.freeze() }`
//!   is refused.
//! - **a name a pattern or a `for` bound.** `match maybe { Some(v) => v.freeze() }`
//!   has no creation to point at.
//!
//! Every one of them has the same correction, and the diagnostic gives it:
//! `toArray()`, which copies in O(n) and asks nothing.
//!
//! # This is not a borrow checker
//!
//! It answers one question about one method. There is no sharing bit, no
//! reference count, no copy-on-write and no runtime table; a program that
//! this pass cannot prove is not a program that is wrong, it is a program
//! that pays `toArray()`'s O(n) copy instead. The diagnostic says so, and
//! naming the alias that defeated the proof is most of what it is for.

use std::collections::{BTreeMap, BTreeSet};

use cove_diag::{Diagnostic, FileId, Span};
use cove_syntax::ast::{
    Arg, Block, Expr, ExprKind, Ident, ItemKind, Param, Pattern, PatternKind, StmtKind, StrPart,
    Type, TypeKind,
};

use crate::facts::Facts;
use crate::resolve::Program;
use crate::typeck::Ty;

/// A `freeze()` whose receiver's storage could not be proved uniquely owned.
pub const NOT_UNIQUE: &str = "cove::unique::not_unique";

/// A read of a vector a `freeze()` has already consumed.
pub const USED_AFTER_FREEZE: &str = "cove::unique::used_after_freeze";

/// The one sentence this pass enforces, on every diagnostic it raises.
const RULE: &str =
    "`freeze()` consumes a vector whose storage the compiler can prove is uniquely owned here, \
     and returns an immutable array in O(1).";

/// One declaration, as the demand table names it.
type FnKey = (String, Option<String>, String);

/// A place: the binding it is rooted at and the fields read off it.
///
/// `self.guests` is `{ root: "self", binding: None, fields: ["guests"] }`.
/// `binding` is the index of the `let`, `var` or pattern that introduced the
/// root, which is what tells two `var parts` in two match arms apart; a root
/// this body did not bind — a parameter, `self`, a declaration of the module
/// — has none, and is compared by name.
///
/// Two places overlap when one is a prefix of the other, which is exactly
/// when writing through either is observable through the other.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Place {
    root: String,
    binding: Option<usize>,
    fields: Vec<String>,
}

impl Place {
    fn overlaps(&self, other: &Place) -> bool {
        self.binding == other.binding
            && self.root == other.root
            && self.fields.iter().zip(&other.fields).all(|(a, b)| a == b)
    }

    /// The place as a reader wrote it.
    fn text(&self) -> String {
        let mut out = self.root.clone();
        for field in &self.fields {
            out.push('.');
            out.push_str(field);
        }
        out
    }
}

/// One read of a place inside a body.
#[derive(Clone, Debug)]
struct Read {
    place: Place,
    span: Span,
    /// What the reader should be told this position was, when the handle
    /// outlives the expression that read it.
    retained: Option<&'static str>,
    /// How many closure bodies deep the read is. A read deeper than its
    /// binding's own depth is a capture, whatever position it is in.
    depth: usize,
}

/// A binding this body introduces.
#[derive(Clone, Debug)]
struct Local<'a> {
    name: &'a str,
    span: Span,
    /// The initialiser of a `let` or `var`. A `for` binding, a match arm's
    /// pattern and a closure parameter have none: what they name came from
    /// somewhere this body cannot see the creation of.
    init: Option<&'a Expr>,
    /// The loop and closure bodies this binding sits inside.
    regions: Vec<Span>,
    depth: usize,
}

/// Something that consumes a place: a `freeze()`, or a call to a method that
/// demands a uniquely owned receiver.
#[derive(Clone, Debug)]
struct Consume {
    place: Place,
    /// The whole expression, which is what a diagnostic points at.
    span: Span,
    /// The loop and closure bodies this site sits inside.
    regions: Vec<Span>,
    /// Whether this site is the operand of a `return`, so that nothing
    /// written after it in the source runs after it.
    terminal: bool,
    /// `None` for a `freeze()`; the callee, for a demanded receiver.
    through: Option<String>,
}

/// A call this body makes to a method written in an `impl` block.
#[derive(Clone, Debug)]
struct MethodCall {
    target: FnKey,
    /// The receiver place, when the receiver is one.
    receiver: Option<Place>,
    span: Span,
    regions: Vec<Span>,
    terminal: bool,
}

/// Everything one body says about places.
#[derive(Default)]
struct Scan<'a> {
    locals: Vec<Local<'a>>,
    reads: Vec<Read>,
    /// Places an assignment writes, and where.
    writes: Vec<(Place, Span)>,
    freezes: Vec<Consume>,
    calls: Vec<MethodCall>,
}

/// One body to analyse.
struct Body<'a> {
    key: Option<FnKey>,
    file: FileId,
    /// Parameter names, receiver excluded.
    params: Vec<&'a str>,
    /// `Some(is_var)` when this body has a receiver.
    receiver: Option<bool>,
    /// Whether a `var self` here can carry an obligation back to its callers:
    /// a method of a plain `impl` block, whose every call site the checker
    /// resolves to this declaration by name.
    receiver_may_demand: bool,
    block: &'a Block,
}

/// The named types whose values can reach a `Vector`.
///
/// A struct or enum is one when a field or a payload names a `Vector`, or
/// names another such type — so `World`, whose fields are `Array`s, is not,
/// and `BookingDraft`, whose `guests` is a `Vector`, is. Keyed by the type's
/// own name without its module, because two modules' types of one name are
/// merged here and merging in this direction only ever refuses more.
type Bearing = BTreeSet<String>;

/// Checks every `freeze()` in `program`.
///
/// The answer is one diagnostic per site that could not be proved, and one
/// per read of a vector a proved site already consumed.
pub fn check(program: &Program, facts: &Facts) -> Vec<Diagnostic> {
    let bearing = vector_bearing(program);
    let bodies = bodies(program);
    let scans: Vec<Scan<'_>> = bodies
        .iter()
        .map(|body| scan(body, facts, &bearing))
        .collect();

    // Which methods demand a uniquely owned receiver, to a fixpoint: a
    // `finish` that freezes `self.routes` demands `routes`, and a method that
    // calls `finish` on `self.builder` demands `builder.routes` in turn.
    let mut demands: BTreeMap<FnKey, BTreeSet<Vec<String>>> = BTreeMap::new();
    loop {
        let mut changed = false;
        for (body, scanned) in bodies.iter().zip(&scans) {
            let (Some(key), Some(true), true) =
                (&body.key, body.receiver, body.receiver_may_demand)
            else {
                continue;
            };
            for consumed in consumptions(scanned, &demands) {
                if consumed.place.root != "self" || consumed.place.binding.is_some() {
                    continue;
                }
                changed |= demands
                    .entry(key.clone())
                    .or_default()
                    .insert(consumed.place.fields.clone());
            }
        }
        if !changed {
            break;
        }
    }

    let mut diagnostics = Vec::new();
    for (body, scanned) in bodies.iter().zip(&scans) {
        for consumed in consumptions(scanned, &demands) {
            prove(body, scanned, &demands, &consumed, &mut diagnostics);
        }
    }
    diagnostics.sort_by_key(|diagnostic| {
        diagnostic
            .primary
            .map(|span| (span.file.0, span.start))
            .unwrap_or((u32::MAX, u32::MAX))
    });
    diagnostics
}

/// Every place a body consumes: its `freeze()` sites, and every call to a
/// method that demands a uniquely owned receiver.
fn consumptions(
    scanned: &Scan<'_>,
    demands: &BTreeMap<FnKey, BTreeSet<Vec<String>>>,
) -> Vec<Consume> {
    let mut out = scanned.freezes.clone();
    for call in &scanned.calls {
        let (Some(paths), Some(receiver)) = (demands.get(&call.target), &call.receiver) else {
            continue;
        };
        for fields in paths {
            let mut place = receiver.clone();
            place.fields.extend(fields.iter().cloned());
            out.push(Consume {
                place,
                span: call.span,
                regions: call.regions.clone(),
                terminal: call.terminal,
                through: Some(format!(
                    "{}.{}",
                    call.target.1.clone().unwrap_or_default(),
                    call.target.2
                )),
            });
        }
    }
    out
}

// --- the proof -------------------------------------------------------------

/// Proves one consumption, or says what defeated it.
fn prove(
    body: &Body<'_>,
    scanned: &Scan<'_>,
    demands: &BTreeMap<FnKey, BTreeSet<Vec<String>>>,
    consumed: &Consume,
    out: &mut Vec<Diagnostic>,
) {
    let place = consumed.place.text();
    let opening = match &consumed.through {
        None => {
            format!("`freeze()` cannot prove that `{place}` holds the only handle to its storage")
        }
        Some(callee) => format!(
            "`{callee}()` consumes `{place}`, and this call cannot prove that it holds the only \
             handle to its storage"
        ),
    };
    let refuse = |span: Span, message: String, out: &mut Vec<Diagnostic>| {
        out.push(
            Diagnostic::error(NOT_UNIQUE, opening.clone())
                .at(consumed.span)
                .label(span, message)
                .rule(RULE)
                .help(
                    "call `toArray()` on the vector instead, which copies the elements in O(n) \
                     and asks nothing about who else is holding it",
                ),
        );
    };

    // Where the root comes from. A binding this body made is provable here; a
    // `var self` that a plain `impl` method received carries the obligation on
    // to its callers; anything else is outside what a local pass can see.
    let (declared, depth) = match consumed.place.binding.map(|at| &scanned.locals[at]) {
        Some(local) => {
            let Some(init) = local.init else {
                refuse(
                    local.span,
                    format!(
                        "`{}` is bound to a value this function did not create, so its storage \
                         may already have another handle",
                        local.name
                    ),
                    out,
                );
                return;
            };
            if !establishes(init, &consumed.place.fields) {
                refuse(
                    init.span,
                    format!(
                        "`{}` is initialised from a value this function did not create, so its \
                         storage may already have another handle",
                        consumed.place.text()
                    ),
                    out,
                );
                return;
            }
            (local.regions.clone(), local.depth)
        }
        None => {
            let carried = consumed.place.root == "self"
                && body.receiver == Some(true)
                && body.receiver_may_demand
                && body
                    .key
                    .as_ref()
                    .and_then(|key| demands.get(key))
                    .is_some_and(|paths| paths.contains(&consumed.place.fields));
            if !carried {
                let from_caller = body.params.contains(&consumed.place.root.as_str())
                    || (consumed.place.root == "self" && body.receiver.is_some());
                refuse(
                    consumed.span,
                    if from_caller {
                        format!(
                            "`{}` comes from this function's caller, and only a `var self` \
                             receiver of a method written in a plain `impl` block can carry the \
                             obligation back to it",
                            consumed.place.root
                        )
                    } else {
                        format!(
                            "`{}` is not a binding this function creates, so where its storage \
                             came from is not a local fact",
                            consumed.place.root
                        )
                    },
                    out,
                );
                return;
            }
            // The obligation left this body for its call sites, which this
            // same pass checks. What stays here is that the body itself
            // neither copies the handle nor reads it afterwards.
            (Vec::new(), 0)
        }
    };

    // A site inside a loop or a closure body runs more than once unless the
    // storage is created inside it too, and a second turn would take storage
    // the first already took.
    if let Some(repeated) = consumed
        .regions
        .iter()
        .find(|span| !declared.contains(span))
    {
        refuse(
            *repeated,
            "this may run more than once, and a second turn would consume storage the first one \
             already took"
                .to_string(),
            out,
        );
        return;
    }

    // Written somewhere else: the place no longer names what its initialiser
    // created.
    if let Some((written, span)) = scanned
        .writes
        .iter()
        .find(|(written, _)| written.overlaps(&consumed.place))
    {
        refuse(
            *span,
            format!(
                "`{}` is assigned here, so the storage it names at the consumption is not the \
                 storage it was created with",
                written.text()
            ),
            out,
        );
        return;
    }

    // Copied to another live place, or escaped.
    if let Some((read, why)) = scanned.reads.iter().find_map(|read| {
        if !read.place.overlaps(&consumed.place) || read.span == consumed.span {
            return None;
        }
        if read.depth > depth {
            return Some((read, "is captured by a closure"));
        }
        read.retained.map(|why| (read, why))
    }) {
        refuse(
            read.span,
            format!("`{}` {why} here", read.place.text()),
            out,
        );
        return;
    }

    // Consumed, and therefore not usable afterward. A site a `return` carries
    // out of the function has nothing after it to check.
    if consumed.terminal {
        return;
    }
    for read in &scanned.reads {
        if read.place.overlaps(&consumed.place) && read.span.start >= consumed.span.end {
            out.push(
                Diagnostic::error(
                    USED_AFTER_FREEZE,
                    format!(
                        "`{}` is read after its storage was consumed",
                        read.place.text()
                    ),
                )
                .at(read.span)
                .label(
                    consumed.span,
                    match &consumed.through {
                        None => "`freeze()` took the storage here".to_string(),
                        Some(callee) => format!("`{callee}()` took the storage here"),
                    },
                )
                .rule(RULE)
                .help(
                    "read the `Array` the transition answered, or call `toArray()` instead, which \
                     copies the elements in O(n) and leaves the vector usable",
                ),
            );
        }
    }
}

/// Whether `init` creates storage this body is the only holder of, reached
/// through `fields`.
///
/// With no fields, the initialiser has to be one of the three expressions
/// that produce a vector nothing else names: `Vector.of(...)` allocates,
/// `toVector()` copies an array's elements out, `snapshot()` copies a
/// vector's. With fields, the initialiser has to be a literal this body
/// wrote, so that the named field's own initialiser can be asked the same
/// question.
fn establishes(init: &Expr, fields: &[String]) -> bool {
    let Some((first, rest)) = fields.split_first() else {
        return creates(init);
    };
    match &init.kind {
        ExprKind::Call { args, .. } => args
            .iter()
            .find(|arg| arg.label.as_ref().is_some_and(|label| label.node == *first))
            .is_some_and(|arg| establishes(&arg.value, rest)),
        _ => false,
    }
}

/// Whether this expression allocates a vector nothing else holds a handle to.
fn creates(init: &Expr) -> bool {
    let ExprKind::Call { callee, .. } = &init.kind else {
        return false;
    };
    let ExprKind::Field { base, name } = &callee.kind else {
        return false;
    };
    match name.node.as_str() {
        "of" => matches!(&base.kind, ExprKind::Ident(head) if head == "Vector"),
        "toVector" | "snapshot" => true,
        _ => false,
    }
}

// --- reading a body --------------------------------------------------------

/// The places one body reads, writes, creates and consumes.
fn scan<'a>(body: &Body<'a>, facts: &Facts, bearing: &Bearing) -> Scan<'a> {
    let mut walk = Walk {
        facts,
        bearing,
        file: body.file,
        regions: Vec::new(),
        depth: 0,
        terminal: false,
        scopes: vec![Vec::new()],
        scan: Scan::default(),
    };
    walk.block(body.block, None);
    walk.scan
}

/// A walk of one body, carrying where it is.
struct Walk<'a, 'f> {
    facts: &'f Facts,
    bearing: &'f Bearing,
    file: FileId,
    /// The loop and closure bodies enclosing the expression being walked.
    regions: Vec<Span>,
    depth: usize,
    /// Whether what is being walked is carried out of the function by a
    /// `return`.
    terminal: bool,
    /// Names in scope, innermost last, each naming a `Scan::locals` index.
    scopes: Vec<Vec<(&'a str, usize)>>,
    scan: Scan<'a>,
}

impl<'a> Walk<'a, '_> {
    /// Runs `body` with a scope of its own.
    fn scoped(&mut self, body: impl FnOnce(&mut Self)) {
        self.scopes.push(Vec::new());
        body(self);
        self.scopes.pop();
    }

    /// Introduces a binding into the innermost scope.
    fn bind(&mut self, name: &'a str, span: Span, init: Option<&'a Expr>) {
        self.scan.locals.push(Local {
            name,
            span,
            init,
            regions: self.regions.clone(),
            depth: self.depth,
        });
        let at = self.scan.locals.len() - 1;
        self.scopes
            .last_mut()
            .expect("a body is walked inside a scope")
            .push((name, at));
    }

    /// The binding `name` resolves to here, if this body made one.
    fn resolve(&self, name: &str) -> Option<usize> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.iter().rev().find(|(bound, _)| *bound == name))
            .map(|(_, at)| *at)
    }

    /// The place an expression names, when it names one.
    fn place_of(&self, expr: &Expr) -> Option<Place> {
        match &expr.kind {
            ExprKind::Ident(name) => Some(Place {
                root: name.clone(),
                binding: self.resolve(name),
                fields: Vec::new(),
            }),
            ExprKind::Field { base, name } => {
                let mut place = self.place_of(base)?;
                place.fields.push(name.node.clone());
                Some(place)
            }
            _ => None,
        }
    }

    /// `retain` is `Some(why)` when the value this position produces outlives
    /// the expression that produced it.
    fn block(&mut self, block: &'a Block, retain: Option<&'static str>) {
        self.scoped(|walk| {
            for stmt in &block.statements {
                match &stmt.kind {
                    StmtKind::Let { name, value, .. } => {
                        walk.expr(value, Some("is copied into another binding"));
                        walk.bind(&name.node, name.span, Some(value));
                    }
                    StmtKind::Expr(expr) => walk.expr(expr, None),
                    // A local `fn` is a closure the body can call, so its own
                    // body is walked as one.
                    StmtKind::Item(item) => {
                        if let ItemKind::Fn(decl) = &item.kind {
                            walk.nested(&decl.params, &decl.body);
                        }
                    }
                }
            }
            if let Some(tail) = &block.tail {
                walk.expr(tail, retain);
            }
        });
    }

    /// A closure body: a region that may run more than once, and whose reads
    /// of an outer binding are captures.
    fn nested(&mut self, params: &'a [Param], block: &'a Block) {
        self.regions.push(block.span);
        self.depth += 1;
        let terminal = std::mem::replace(&mut self.terminal, false);
        self.scoped(|walk| {
            for param in params {
                walk.bind(&param.name.node, param.name.span, None);
            }
            walk.block(block, None);
        });
        self.terminal = terminal;
        self.depth -= 1;
        self.regions.pop();
    }

    /// A loop body, which may run more than once but captures nothing.
    fn repeated(&mut self, binding: Option<&'a Ident>, block: &'a Block) {
        self.regions.push(block.span);
        let terminal = std::mem::replace(&mut self.terminal, false);
        self.scoped(|walk| {
            if let Some(binding) = binding {
                walk.bind(&binding.node, binding.span, None);
            }
            walk.block(block, None);
        });
        self.terminal = terminal;
        self.regions.pop();
    }

    fn expr(&mut self, expr: &'a Expr, retain: Option<&'static str>) {
        // A place is read whole. Its base is part of the path rather than a
        // separate read, so the walk stops here.
        if let Some(place) = self.place_of(expr) {
            self.scan.reads.push(Read {
                place,
                span: expr.span,
                retained: retain,
                depth: self.depth,
            });
            return;
        }
        match &expr.kind {
            ExprKind::Call {
                callee,
                args,
                trailing,
                ..
            } => self.call(expr, callee, args, trailing.as_deref()),
            // Reached only when the base is not a place, as in `f().x`.
            ExprKind::Field { base, .. } => self.expr(base, None),
            ExprKind::ArrayLit(items) => {
                for item in items {
                    self.expr(item, Some("is stored in another value"));
                }
            }
            // An interpolation formats its operand and keeps nothing.
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Interpolation(inner) = part {
                        self.expr(inner, None);
                    }
                }
            }
            ExprKind::Unary { operand, .. } => self.expr(operand, None),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, None);
                self.expr(rhs, None);
            }
            ExprKind::Assign { target, value, .. } => {
                match self.place_of(target) {
                    Some(place) => self.scan.writes.push((place, expr.span)),
                    None => self.expr(target, None),
                }
                self.expr(value, Some("is copied into another place"));
            }
            ExprKind::Try(inner) | ExprKind::Await(inner) => self.expr(inner, retain),
            ExprKind::Block(block) => self.block(block, retain),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition, None);
                self.block(then_branch, retain);
                if let Some(other) = else_branch {
                    self.expr(other, retain);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, None);
                for arm in arms {
                    self.scoped(|walk| {
                        walk.pattern(&arm.pattern);
                        walk.expr(&arm.body, retain);
                    });
                }
            }
            // Iterating reads elements out; the sequence is not retained.
            ExprKind::For {
                binding,
                iterable,
                body,
            } => {
                self.expr(iterable, None);
                self.repeated(Some(binding), body);
            }
            ExprKind::While { condition, body } => {
                self.expr(condition, None);
                self.repeated(None, body);
            }
            ExprKind::Return(Some(value)) => {
                let outer = std::mem::replace(&mut self.terminal, true);
                self.expr(value, Some("is returned"));
                self.terminal = outer;
            }
            // A loop is `Unit` however it leaves, so a `break` value is
            // evaluated and discarded.
            ExprKind::Break(Some(value)) => self.expr(value, None),
            ExprKind::Lambda { params, body, .. } => self.nested(params, body),
            // A scope's body may outlive the statement that wrote it — a
            // spawned task runs inside it — so it is a closure body here, and
            // the name it binds is one of its own.
            ExprKind::Scope { name, body } => {
                self.regions.push(body.span);
                self.depth += 1;
                let terminal = std::mem::replace(&mut self.terminal, false);
                self.scoped(|walk| {
                    walk.bind(&name.node, name.span, None);
                    walk.block(body, None);
                });
                self.terminal = terminal;
                self.depth -= 1;
                self.regions.pop();
            }
            ExprKind::Range { start, end, .. } => {
                self.expr(start, None);
                self.expr(end, None);
            }
            _ => {}
        }
    }

    /// Every name a pattern binds, as a binding this body cannot see the
    /// creation of.
    fn pattern(&mut self, pattern: &'a Pattern) {
        match &pattern.kind {
            PatternKind::Binding(name) => self.bind(name, pattern.span, None),
            PatternKind::Variant { payload, .. } => {
                for inner in payload {
                    self.pattern(inner);
                }
            }
            PatternKind::Wildcard | PatternKind::Literal(_) => {}
        }
    }

    /// A call, and what each of its operands does with the handle it names.
    ///
    /// A `Vector` cannot cross a task boundary and cannot be held by a Host
    /// resource — the Language Card's task-safety rule and ADR 0017's
    /// boundary see to both — so the handle a callee is passed can only
    /// outlive the call by leaving through the callee's own result, through a
    /// `var` argument the caller can see, or by being written into another
    /// operand that is itself a container. When none of those is possible the
    /// copy dies with the call, and treating it as an escape would refuse
    /// `world.firstFree(..., creatures)` for nothing.
    fn call(
        &mut self,
        call: &'a Expr,
        callee: &'a Expr,
        args: &'a [Arg],
        trailing: Option<&'a Expr>,
    ) {
        let result_holds = self
            .facts
            .ty(self.file, call.id)
            .is_some_and(|ty| self.holds_vector(ty));
        let receiver = match &callee.kind {
            ExprKind::Field { base, name } => {
                self.method(call, base, &name.node, result_holds);
                Some(base)
            }
            _ => {
                self.expr(callee, None);
                None
            }
        };
        // Which operands could be a container the callee writes another
        // operand into.
        let containers: Vec<bool> = receiver
            .into_iter()
            .map(|base| &**base)
            .chain(args.iter().map(|arg| &arg.value))
            .map(|operand| {
                self.facts
                    .ty(self.file, operand.id)
                    .is_some_and(|ty| self.holds_vector(ty))
            })
            .collect();
        let elsewhere = |at: usize| {
            containers
                .iter()
                .enumerate()
                .any(|(j, held)| *held && j != at)
        };
        let inout = args.iter().any(|arg| arg.is_var);
        let offset = usize::from(receiver.is_some());
        for (at, arg) in args.iter().enumerate() {
            let retained = if result_holds {
                Some("escapes into a call that may answer with it")
            } else if inout {
                Some("escapes into a call that writes through a `var` argument")
            } else if elsewhere(at + offset) {
                Some("escapes into a call that may store it in another argument")
            } else {
                None
            };
            self.expr(&arg.value, retained);
        }
        if let Some(trailing) = trailing {
            self.expr(trailing, Some("is captured by a trailing closure"));
        }
    }

    /// A method call's receiver, and whether this call is a consumption.
    fn method(&mut self, call: &'a Expr, base: &'a Expr, name: &str, result_holds: bool) {
        let receiver = self.place_of(base);
        if name == "freeze" && matches!(self.facts.ty(self.file, base.id), Some(Ty::Vector(_))) {
            if let Some(place) = receiver.clone() {
                self.scan.freezes.push(Consume {
                    place,
                    span: call.span,
                    regions: self.regions.clone(),
                    terminal: self.terminal,
                    through: None,
                });
            }
        }
        let declared = self.facts.target(self.file, call.id);
        if let Some(target) = declared {
            self.scan.calls.push(MethodCall {
                target: (
                    target.module.clone(),
                    Some(target.type_name.clone()),
                    target.method.clone(),
                ),
                receiver,
                span: call.span,
                regions: self.regions.clone(),
                terminal: self.terminal,
            });
        }
        // A declared method whose result can reach a `Vector` may be handing
        // the receiver's own field back, which is a copy of the handle. Every
        // builtin that answers one — `snapshot`, `toVector` — answers a fresh
        // one, and every other position reads through the receiver without
        // keeping it.
        self.expr(
            base,
            (declared.is_some() && result_holds)
                .then_some("is handed back by a method called here"),
        );
    }

    /// Whether a value of this type can reach a `Vector`.
    fn holds_vector(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Vector(_) => true,
            Ty::Array(inner)
            | Ty::Set(inner)
            | Ty::Option(inner)
            | Ty::Task(inner)
            | Ty::Shared(inner) => self.holds_vector(inner),
            Ty::Map(key, value) | Ty::MapEntry(key, value) | Ty::Result(key, value) => {
                self.holds_vector(key) || self.holds_vector(value)
            }
            Ty::Struct(name, args) | Ty::Enum(name, args) => {
                self.bearing.contains(simple_name(name))
                    || args.iter().any(|ty| self.holds_vector(ty))
            }
            Ty::Fn(signature) => {
                signature.params.iter().any(|ty| self.holds_vector(ty))
                    || self.holds_vector(&signature.ret)
            }
            _ => false,
        }
    }
}

/// A type's name without the module that qualifies it.
fn simple_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Every declared type whose values can reach a `Vector`, to a fixpoint.
///
/// Read off the written types rather than the checked ones, because what is
/// wanted is one bit per declaration and the declarations are what the
/// package holds. A name is compared without its module for the reason
/// [`Bearing`] states.
fn vector_bearing(program: &Program) -> Bearing {
    let mut members: BTreeMap<&str, Vec<&Type>> = BTreeMap::new();
    for module in program.modules.values() {
        for (name, entry) in &module.structs {
            members
                .entry(simple_name(name))
                .or_default()
                .extend(entry.decl.fields.iter().map(|field| &field.ty));
        }
        for (name, entry) in &module.enums {
            members
                .entry(simple_name(name))
                .or_default()
                .extend(entry.decl.cases.iter().flat_map(|case| case.payload.iter()));
        }
    }
    let mut bearing = Bearing::new();
    loop {
        let mut changed = false;
        for (name, types) in &members {
            if bearing.contains(*name) {
                continue;
            }
            if types.iter().any(|ty| names_a_vector(ty, &bearing)) {
                bearing.insert((*name).to_string());
                changed = true;
            }
        }
        if !changed {
            return bearing;
        }
    }
}

/// Whether a written type names `Vector`, or a type already known to bear
/// one.
fn names_a_vector(ty: &Type, bearing: &Bearing) -> bool {
    match &ty.kind {
        TypeKind::Named { path, args } => {
            let name = path
                .last()
                .map(|segment| segment.node.as_str())
                .unwrap_or_default();
            name == "Vector"
                || bearing.contains(name)
                || args.iter().any(|arg| names_a_vector(arg, bearing))
        }
        TypeKind::Fn {
            params,
            return_type,
            ..
        } => {
            params.iter().any(|param| {
                param
                    .ty
                    .as_ref()
                    .is_some_and(|ty| names_a_vector(ty, bearing))
            }) || return_type
                .as_ref()
                .is_some_and(|ty| names_a_vector(ty, bearing))
        }
        TypeKind::Dyn(_) | TypeKind::Unit => false,
    }
}

/// Every body of the package, in one list.
///
/// A trait's default body is here once, where the trait declares it, exactly
/// as the type checker walks it — a conformance that inherits one does not
/// get a copy.
fn bodies(program: &Program) -> Vec<Body<'_>> {
    // A method name any trait declares can be reached through a bound or
    // through `dyn`, and neither names a declaration for an obligation to be
    // discharged at. Such a method may freeze what it creates, and may not
    // demand anything of its callers.
    let through_a_trait: BTreeSet<&str> = program
        .modules
        .values()
        .flat_map(|module| module.traits.values())
        .flat_map(|entry| entry.decl.methods.iter())
        .map(|method| method.name.node.as_str())
        .collect();

    let mut out = Vec::new();
    for (module_name, module) in &program.modules {
        for (name, entry) in &module.functions {
            out.push(Body {
                key: Some((module_name.clone(), None, name.clone())),
                file: entry.decl.span.file,
                params: names(&entry.decl.params),
                receiver: entry.decl.receiver.map(|receiver| receiver.is_var),
                receiver_may_demand: false,
                block: &entry.decl.body,
            });
        }
        for ((type_name, name), entry) in &module.methods {
            if entry.from_trait_default.is_some() {
                continue;
            }
            out.push(Body {
                key: Some((module_name.clone(), Some(type_name.clone()), name.clone())),
                file: entry.decl.span.file,
                params: names(&entry.decl.params),
                receiver: entry.decl.receiver.map(|receiver| receiver.is_var),
                receiver_may_demand: !through_a_trait.contains(name.as_str()),
                block: &entry.decl.body,
            });
        }
        for entry in module.traits.values() {
            for method in &entry.decl.methods {
                let Some(default) = &method.default else {
                    continue;
                };
                out.push(Body {
                    key: None,
                    file: method.span.file,
                    params: names(&method.params),
                    receiver: method.receiver.map(|receiver| receiver.is_var),
                    receiver_may_demand: false,
                    block: default,
                });
            }
        }
    }
    out
}

/// The names of a declaration's parameters, in order.
fn names(params: &[Param]) -> Vec<&str> {
    params
        .iter()
        .map(|param| param.name.node.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use cove_diag::SourceMap;

    use super::*;
    use crate::package::{Module, Package, Unit};
    use crate::typeck::check;

    /// Everything `cove check` reports about one module.
    fn errors_of(source: &str) -> Vec<Diagnostic> {
        let mut sources = SourceMap::new();
        let path = PathBuf::from("main.cove");
        let file = sources.add(path.clone(), source);
        let ast = cove_syntax::parse_file(&sources, file).expect("test source parses");
        let package = Package {
            root: PathBuf::new(),
            config: crate::config::Config::default(),
            modules: BTreeMap::from([(
                "main".to_string(),
                Module {
                    name: "main".to_string(),
                    dir: PathBuf::from("main"),
                    units: vec![Unit { file, path, ast }],
                },
            )]),
        };
        let program = crate::resolve::resolve(&package).expect("test source resolves");
        check(&package, &program)
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == cove_diag::Severity::Error)
            .collect()
    }

    #[track_caller]
    fn proves(source: &str) {
        let errors = errors_of(source);
        assert!(
            errors.is_empty(),
            "expected the proof to succeed, found: {}",
            errors
                .iter()
                .map(|error| format!("{}: {}", error.code, error.message))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    #[track_caller]
    fn refuses(source: &str) -> Diagnostic {
        let mut errors = errors_of(source);
        assert_eq!(
            errors.len(),
            1,
            "expected exactly one error, found: {}",
            errors
                .iter()
                .map(|error| format!("{}: {}", error.code, error.message))
                .collect::<Vec<_>>()
                .join("; ")
        );
        errors.remove(0)
    }

    /// The shape `freeze()` was written for: build a vector, hand it over.
    #[test]
    fn a_vector_built_here_and_handed_over_is_proved() {
        proves(
            "\
fn build(upTo: Int) -> Array<Int> {
  var building = Vector.of()
  for n in 1..upTo {
    building.push(n)
  }
  building.freeze()
}
",
        );
    }

    /// A temporary holds the only handle to its own storage, so there is
    /// nothing to prove and no place to name.
    #[test]
    fn a_temporary_receiver_needs_no_proof() {
        proves("fn build() -> Int {\n  Vector.of(1, 2).freeze().length()\n}\n");
        proves("fn build(items: Array<Int>) -> Array<Int> {\n  items.toVector().freeze()\n}\n");
    }

    /// The case the corpus pins: a second binding is a second handle.
    #[test]
    fn a_second_binding_defeats_the_proof_and_the_diagnostic_names_it() {
        let error = refuses(
            "\
fn build() -> Array<Int> {
  var building = Vector.of(1, 2)
  var alias = building
  alias.push(3)
  building.freeze()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.message,
            "`freeze()` cannot prove that `building` holds the only handle to its storage"
        );
        assert_eq!(
            error.labels[0].message,
            "`building` is copied into another binding here"
        );
        assert!(
            error
                .help
                .as_deref()
                .is_some_and(|help| help.contains("toArray()")),
            "{:?}",
            error.help
        );
    }

    /// Formatting a vector and pushing onto it both read through the handle
    /// without keeping it, which is what `tests/e2e:coll_array` needs.
    #[test]
    fn interpolating_and_pushing_are_not_escapes() {
        proves(
            "\
use console.println

fn build(items: Array<Int>) -> Result<Array<Int>, Error> {
  var growable = items.toVector()
  growable.push(40)
  println(\"{growable}\")?
  Ok(growable.freeze())
}
",
        );
    }

    /// A callee that cannot keep the handle is not an escape; the three
    /// ways one can are.
    #[test]
    fn a_call_escapes_only_when_the_callee_could_keep_the_handle() {
        proves(
            "\
fn total(of: Vector<Int>) -> Int {
  of.length()
}

fn build() -> Array<Int> {
  var items = Vector.of(1, 2)
  total(items)
  items.freeze()
}
",
        );
        let answered = refuses(
            "\
fn wrap(one: Vector<Int>) -> Vector<Vector<Int>> {
  Vector.of(one)
}

fn build() -> Array<Int> {
  var items = Vector.of(1, 2)
  wrap(items)
  items.freeze()
}
",
        );
        assert_eq!(
            answered.labels[0].message,
            "`items` escapes into a call that may answer with it here"
        );
        let written = refuses(
            "\
fn fill(var into: Vector<Int>, from: Vector<Int>) {
  into.push(from.length())
}

fn build() -> Array<Int> {
  var items = Vector.of(1, 2)
  var sink = Vector.of(0)
  fill(var sink, items)
  items.freeze()
}
",
        );
        assert_eq!(
            written.labels[0].message,
            "`items` escapes into a call that writes through a `var` argument here"
        );
        let stored = refuses(
            "\
struct Sink {
  rows: Vector<Vector<Int>>
}

fn keep(one: Vector<Int>, into: Sink) {
  var rows = into.rows
  rows.push(one)
}

fn build(sink: Sink) -> Array<Int> {
  var items = Vector.of(1, 2)
  keep(items, sink)
  items.freeze()
}
",
        );
        assert_eq!(
            stored.labels[0].message,
            "`items` escapes into a call that may store it in another argument here"
        );
    }

    /// A closure that mentions the vector holds it for as long as the closure
    /// lives, which this pass cannot bound.
    #[test]
    fn a_closure_capture_defeats_the_proof() {
        let error = refuses(
            "\
fn build() -> Array<Int> {
  var items = Vector.of(1, 2)
  let count = fn() {
    items.length()
  }
  count()
  items.freeze()
}
",
        );
        assert_eq!(
            error.labels[0].message,
            "`items` is captured by a closure here"
        );
    }

    /// `freeze()` consumes, so a read afterwards is an error of its own,
    /// pointing at both ends.
    #[test]
    fn a_read_after_the_freeze_is_reported_where_it_is_written() {
        let error = refuses(
            "\
fn build() -> Int {
  var items = Vector.of(1, 2)
  let frozen = items.freeze()
  frozen.length() + items.length()
}
",
        );
        assert_eq!(error.code, USED_AFTER_FREEZE);
        assert_eq!(
            error.message,
            "`items` is read after its storage was consumed"
        );
        assert_eq!(error.labels[0].message, "`freeze()` took the storage here");
    }

    /// A second turn would find the storage gone.
    #[test]
    fn a_freeze_a_loop_runs_twice_is_refused() {
        let error = refuses(
            "\
fn build(rounds: Int) -> Int {
  var items = Vector.of(1, 2)
  var total = 0
  for _n in 0..rounds {
    total += items.freeze().length()
  }
  total
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert!(
            error.labels[0].message.contains("more than once"),
            "{}",
            error.labels[0].message
        );
    }

    /// Storage that came from somewhere this body cannot see the creation of.
    #[test]
    fn a_binding_this_body_did_not_create_is_refused() {
        let error = refuses(
            "\
fn fresh() -> Vector<Int> {
  Vector.of(1)
}

fn build() -> Array<Int> {
  var items = fresh()
  items.freeze()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert!(
            error.labels[0]
                .message
                .contains("initialised from a value this function did not create"),
            "{}",
            error.labels[0].message
        );
    }

    /// A parameter belongs to the caller, and only a `var self` receiver can
    /// carry the obligation back to one.
    #[test]
    fn an_ordinary_parameter_cannot_be_frozen() {
        let error = refuses(
            "\
fn build(var items: Vector<Int>) -> Array<Int> {
  items.freeze()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert!(
            error.labels[0]
                .message
                .contains("comes from this function's caller"),
            "{}",
            error.labels[0].message
        );
    }

    /// The builder shape: `finish` demands a unique receiver, and the call
    /// site is where that is proved.
    #[test]
    fn a_var_self_method_moves_the_obligation_to_its_callers() {
        proves(
            "\
struct Draft {
  guests: Vector<String>
}

impl Draft {
  fn add(var self, name: String) {
    self.guests.push(name)
  }

  fn finish(var self) -> Array<String> {
    self.guests.freeze()
  }
}

fn build(name: String) -> Array<String> {
  var fresh = Draft(guests: Vector.of())
  fresh.add(name)
  fresh.finish()
}
",
        );
    }

    /// The same method, called on a draft a second binding also observes.
    #[test]
    fn the_demand_is_discharged_at_the_call_site_and_can_fail_there() {
        let error = refuses(
            "\
struct Draft {
  guests: Vector<String>
}

impl Draft {
  fn finish(var self) -> Array<String> {
    self.guests.freeze()
  }
}

fn build() -> Array<String> {
  var original = Draft(guests: Vector.of(\"a\"))
  var alias = original
  alias.guests.push(\"b\")
  original.finish()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.message,
            "`Draft.finish()` consumes `original.guests`, and this call cannot prove that it \
             holds the only handle to its storage"
        );
        assert_eq!(
            error.labels[0].message,
            "`original` is copied into another binding here"
        );
    }

    /// Two arms are two bindings, however alike their names are.
    #[test]
    fn a_name_bound_in_two_arms_is_two_bindings() {
        proves(
            "\
enum Shape {
  Left
  Right
}

fn render(shape: Shape) -> Int {
  match shape {
    Shape.Left => {
      var parts = Vector.of(1)
      parts.freeze().length()
    }
    Shape.Right => {
      var parts = Vector.of(2, 3)
      parts.freeze().length()
    }
  }
}
",
        );
    }

    /// A `return` carries the site out of the function, so what is written
    /// after it is not read after it.
    #[test]
    fn a_freeze_a_return_carries_out_has_nothing_after_it() {
        proves(
            "\
fn build(early: Bool) -> Array<Int> {
  var items = Vector.of()
  if early {
    return items.freeze()
  }
  items.push(1)
  items.freeze()
}
",
        );
    }
}
