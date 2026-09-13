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
//!   `array.toVector()`, `vector.snapshot()`, a field initialised by one in a
//!   struct literal this body wrote, or a call to a declared function this
//!   pass has *proved* answers freshly built storage (see `Freshness`, below);
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
//! - **storage a call produced, when the callee does not visibly build it.**
//!   `var log = hand(mine)` then `log.freeze()` is refused whenever `hand`'s
//!   answer is anything but a construction written where it is answered — a
//!   parameter, a local, a literal over either. This entry used to say that
//!   *every* call was refused, and `Freshness` is what changed it: a
//!   declared function whose every answering expression builds its value on
//!   the spot is proved to answer fresh storage, so `StringBuilder.withCapacity(16)`
//!   is a creation where `freshVector()` used to be a dead end. What is still
//!   refused is the interesting half, and the reason it is refused is written
//!   there rather than here.
//! - **storage an assignment brought in.** `log = lines` gives `log` whatever
//!   the caller is holding, and this refuses `log.freeze()` afterwards —
//!   correctly, in that case.
//! - **a `var` parameter that is not `self`.** The obligation only travels
//!   back through a method receiver, so `fn take(var v: Vector<Int>) { ... v.freeze() }`
//!   is refused.
//! - **a name a pattern or a `for` bound.** `match maybe { Some(v) => v.freeze() }`
//!   has no creation to point at.
//!
//! A vector has one correction for all of them and the diagnostic gives it:
//! `toArray()`, which copies in O(n) and asks nothing. A `ByteBuffer` has no
//! copying conversion to offer, so its correction is to build the builder in
//! the body that finishes it — which is the thing `Freshness` made possible
//! to say, because `StringBuilder.withCapacity(n)` now counts as building one.
//! See `Transition::correction`.
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
///
/// Written once for both transitions, because there is one rule: a consuming
/// transition takes the storage, so the compiler has to be able to prove here
/// that nothing else is holding it. [`Transition`] is what supplies the name and
/// the correction, which are the two parts that differ.
const RULE: &str =
    "A consuming transition — `Vector.freeze()`, `ByteBuffer.finish()` — takes storage whose \
     unique ownership the compiler can prove here, and answers an immutable value in O(1).";

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
    /// Which transition this is, and what a reader should try instead.
    transition: Transition,
}

/// What consumed the storage, as a diagnostic has to name it.
///
/// Three cases rather than two, because a demanded receiver is not a transition
/// a reader wrote: `draft.finish()` consumes `draft.guests` because
/// `BookingDraft.finish` freezes it, and pointing at `freeze()` would point into
/// a body the reader is not looking at.
#[derive(Clone, Debug)]
enum Transition {
    /// `Vector.freeze()`, written here.
    Freeze,
    /// `ByteBuffer.finish()`, written here.
    Finish,
    /// A call to a declared method that consumes through its own receiver,
    /// named `Type.method`.
    Through(String),
}

impl Transition {
    /// How the diagnostic's first sentence names what took the storage.
    fn named(&self) -> &str {
        match self {
            Transition::Freeze => "freeze()",
            Transition::Finish => "finish()",
            Transition::Through(callee) => callee,
        }
    }

    /// What to do instead, which is the whole reason a refusal here is not a
    /// dead end.
    ///
    /// A vector has a copying conversion and a buffer does not: ADR 0052 leaves
    /// an explicit copying conversion for a non-unique builder to a later
    /// decision, and until there is one the correction is to build the string
    /// where it is finished. So the two say different things, and a shared
    /// sentence recommending `toArray()` would be advice a `StringBuilder` has
    /// no way to take.
    fn correction(&self) -> &'static str {
        match self {
            Transition::Finish => {
                "create the builder in the body that finishes it, and drop any other handle to it \
                 before the call"
            }
            Transition::Freeze => {
                "call `toArray()` on the vector instead, which copies the elements in O(n) and \
                 asks nothing about who else is holding it"
            }
            // A demanded receiver may be carrying either kind of owner — the
            // demand is a field path and this side does not know what type is
            // at the end of it — so this says the part that is true of both and
            // names the vector's copying conversion as the vector's own.
            Transition::Through(_) => {
                "create the value in the body that consumes it, and drop any other handle to it \
                 before the call; for a `Vector`, `toArray()` copies the elements in O(n) and \
                 asks nothing"
            }
        }
    }
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
    /// Every expression this body can answer with: the tail of its own block,
    /// and the operand of every `return` written anywhere inside it.
    ///
    /// This is what [`answers_fresh`] reads, and it is collected *over*
    /// rather than *under*: a `return` inside a lambda or a local `fn`
    /// returns from that closure and not from this body, and it is recorded
    /// here anyway. Over-collecting only ever adds an expression that has to
    /// be a construction, so the summary refuses more; missing one would let
    /// a body answer something the summary never looked at, which is the
    /// direction that is unsound.
    answers: Vec<&'a Expr>,
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

/// The named types whose values can reach a linear owner.
///
/// An *owner* is a value whose copy is an alias to shared growable storage and
/// whose storage a consuming transition takes: a `Vector`, which
/// `Vector.freeze()` consumes, and a `ByteBuffer`, which
/// [ADR 0052](../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
/// `finish()` consumes. The two are one ownership discipline and this pass owes
/// them one proof, so nothing here distinguishes them.
///
/// A struct or enum is here when a field or a payload names an owner, or names
/// another such type — so `World`, whose fields are `Array`s, is not;
/// `BookingDraft`, whose `guests` is a `Vector`, is; and `StringBuilder`, whose
/// `buffer` is a `ByteBuffer`, is. Keyed by the type's own name without its
/// module, because two modules' types of one name are merged here and merging
/// in this direction only ever refuses more.
type Bearing = BTreeSet<String>;

/// Checks every `freeze()` in `program`.
///
/// The answer is one diagnostic per site that could not be proved, and one
/// per read of a vector a proved site already consumed.
pub fn check(program: &Program, facts: &Facts) -> Vec<Diagnostic> {
    let bearing = owner_bearing(program);
    let bodies = bodies(program);
    let scans: Vec<Scan<'_>> = bodies
        .iter()
        .map(|body| scan(body, facts, &bearing))
        .collect();

    // What each declaration answers, before anything asks where a binding's
    // storage came from: an initialiser that is a call to a function in this
    // set is a creation, and `prove` below is what reads that.
    let fresh = answers_fresh(program, &bodies, &scans, facts, &bearing);

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
            prove(
                body,
                scanned,
                facts,
                &demands,
                &fresh,
                &consumed,
                &mut diagnostics,
            );
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
                transition: Transition::Through(format!(
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
    facts: &Facts,
    demands: &BTreeMap<FnKey, BTreeSet<Vec<String>>>,
    fresh: &Freshness<'_>,
    consumed: &Consume,
    out: &mut Vec<Diagnostic>,
) {
    let place = consumed.place.text();
    let named = consumed.transition.named();
    let opening = match &consumed.transition {
        Transition::Through(callee) => format!(
            "`{callee}()` consumes `{place}`, and this call cannot prove that it holds the only \
             handle to its storage"
        ),
        _ => {
            format!("`{named}` cannot prove that `{place}` holds the only handle to its storage")
        }
    };
    let refuse = |span: Span, message: String, out: &mut Vec<Diagnostic>| {
        out.push(
            Diagnostic::error(NOT_UNIQUE, opening.clone())
                .at(consumed.span)
                .label(span, message)
                .rule(RULE)
                .help(consumed.transition.correction()),
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
            if !establishes(init, &consumed.place.fields, facts, body.file, fresh) {
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
                    format!(
                        "`{}{}` took the storage here",
                        consumed.transition.named(),
                        match &consumed.transition {
                            Transition::Through(_) => "()",
                            _ => "",
                        }
                    ),
                )
                .rule(RULE)
                .help(match &consumed.transition {
                    Transition::Finish => {
                        "read the `String` the finish answered; a finished buffer holds nothing"
                    }
                    _ => {
                        "read the `Array` the transition answered, or call `toArray()` instead, \
                         which copies the elements in O(n) and leaves the vector usable"
                    }
                }),
            );
        }
    }
}

// --- what a declared function may be trusted to answer ---------------------

/// The declarations this package's *bodies* prove answer fresh storage, and
/// the tables a call site needs to name one.
///
/// # Derived, not declared, and the difference is the whole justification
///
/// [`creates`] says who may *claim* freshness: `cove-schema`, about a
/// builtin, and nobody else. A declared `fn` is deliberately not on that
/// list, because a claim is unchecked — nothing would verify that a Cove
/// function annotated "answers fresh storage" really does, and a wrong claim
/// is a consuming transition proved over storage another holder survived
/// with. That boundary stays exactly where it is.
///
/// This is the other thing: the pass *proves* it from the body. No
/// declaration says anything, no annotation exists to be written, and a
/// function is in [`Freshness::answers`] only because this pass read its
/// answering expressions and found every one of them to be a construction.
/// A body that is edited to answer something else leaves the set on the next
/// compile, with nothing to update — which is what a derived summary buys
/// over a declared one.
///
/// # Why the rule is narrow on purpose
///
/// An answering expression counts only when it is *syntactically* a
/// construction: a builtin fresh call, a call to a function already in this
/// set, or a struct literal whose every argument is itself a construction or
/// cannot reach an owner at all. `return x` naming a variable is not a
/// construction, whatever `x` holds.
///
/// That restriction is what makes the summary sound without an escape
/// analysis. An expression that builds its value on the spot cannot be
/// aliased by anything else in the body, because there was no value to alias
/// until the answer was built — so there is nothing to search the body for.
/// The moment a name is admitted the question changes into "did anything else
/// in this body get a handle on what that name holds", which is a whole
/// analysis and not a syntactic test. Widening this is a soundness decision;
/// see the negative tests, each of which would start proving if a bare
/// `return x` were accepted.
struct Freshness<'a> {
    /// Declarations proved to answer freshly created storage.
    answers: BTreeSet<FnKey>,
    /// Every declared struct's bare name, so a `Call` through a plain `Ident`
    /// can be told from a call to a function of the same shape.
    ///
    /// A name that is in both this set and `functions` is read as neither: see
    /// [`constructs`], which refuses rather than guessing which declaration a
    /// bare name reaches.
    structs: BTreeSet<&'a str>,
    /// Free functions by their bare name, and every declaration that name
    /// could reach.
    ///
    /// A free call records no [`Facts::target`] — the checker names only
    /// method and associated-function targets — so this side has to resolve
    /// the name itself, and a name may be declared by more than one module.
    /// The list is therefore every candidate, and a call resolves as fresh
    /// only when *all* of them are in `answers`: an ambiguity that would have
    /// to guess refuses instead.
    functions: BTreeMap<&'a str, Vec<FnKey>>,
    /// What a struct literal's argument is measured against.
    bearing: &'a Bearing,
}

impl Freshness<'_> {
    /// Whether a call to a declared function of this bare name answers fresh.
    fn by_name(&self, name: &str) -> bool {
        self.functions
            .get(name)
            .is_some_and(|keys| keys.iter().all(|key| self.answers.contains(key)))
    }
}

/// Which of this package's declarations answer freshly created storage, to a
/// monotone fixpoint.
///
/// The set starts empty and only ever grows, so it terminates; a function
/// that answers a construction built out of its own recursive call is never
/// added, because the first round has nothing to add it from and no later
/// round can start it.
///
/// See [`Freshness`] for what a construction is and why nothing weaker
/// counts.
fn answers_fresh<'a>(
    program: &'a Program,
    bodies: &[Body<'a>],
    scans: &[Scan<'a>],
    facts: &Facts,
    bearing: &'a Bearing,
) -> Freshness<'a> {
    let mut fresh = Freshness {
        answers: BTreeSet::new(),
        structs: BTreeSet::new(),
        functions: BTreeMap::new(),
        bearing,
    };
    for (module_name, module) in &program.modules {
        for name in module.structs.keys() {
            fresh.structs.insert(simple_name(name));
        }
        for name in module.functions.keys() {
            fresh.functions.entry(name.as_str()).or_default().push((
                module_name.clone(),
                None,
                name.clone(),
            ));
        }
    }
    loop {
        let mut changed = false;
        for (body, scanned) in bodies.iter().zip(scans) {
            // A body the package cannot name — a trait's default, which is
            // reached through a bound or through `dyn` — has no key for a
            // call site to resolve to, so nothing could read its entry.
            let Some(key) = &body.key else { continue };
            if fresh.answers.contains(key) || scanned.answers.is_empty() {
                continue;
            }
            let proved = scanned
                .answers
                .iter()
                .all(|answer| constructs(answer, facts, body.file, &fresh));
            if proved {
                changed |= fresh.answers.insert(key.clone());
            }
        }
        if !changed {
            return fresh;
        }
    }
}

/// Whether this expression *builds* the value it answers, so that nothing
/// else can be holding the storage underneath it.
///
/// Three shapes, and [`Freshness`] says why there are no more:
///
/// - a builtin call [`creates`] recognises, such as `ByteBuffer.allocate(n)`;
/// - a call to a declared function already proved to answer fresh;
/// - a struct literal whose every argument is itself a construction, or is of
///   a type that cannot reach an owner — a scalar, a `String`, an `Array`.
///   An argument that could carry an owner and is not itself built here is
///   refused, so `Builder(buffer: given)` is not a construction however
///   `given` was obtained.
///
/// A call carrying a trailing closure is not a construction either: the
/// closure may have captured an owner, and the argument list this walks is
/// not where it would be found.
fn constructs(expr: &Expr, facts: &Facts, file: FileId, fresh: &Freshness<'_>) -> bool {
    if creates(expr, facts, file) {
        return true;
    }
    let ExprKind::Call {
        callee,
        args,
        trailing,
        ..
    } = &expr.kind
    else {
        return false;
    };
    if trailing.is_some() {
        return false;
    }
    // A method or an associated function: the checker resolved which
    // declaration this reaches and recorded it, so there is nothing to guess.
    if let Some(target) = facts.target(file, expr.id) {
        return fresh.answers.contains(&(
            target.module.clone(),
            Some(target.type_name.clone()),
            target.method.clone(),
        ));
    }
    let ExprKind::Ident(head) = &callee.kind else {
        return false;
    };
    // A callee the checker gave a type is a call *through a value* — a
    // binding holding a closure — and names no declaration at all. See the
    // `facts` module: that absence is the fact, not a gap in the table.
    if facts.ty(file, callee.id).is_some() {
        return false;
    }
    // A struct literal, unless the name is also some module's function — in
    // which case this side cannot tell which of the two the call reaches, and
    // reading a function call as a literal would check the wrong things about
    // its arguments. Nothing in the corpus spells a struct and a function the
    // same way, and the point of the guard is that nothing has to.
    if fresh.structs.contains(head.as_str()) && !fresh.functions.contains_key(head.as_str()) {
        return args.iter().all(|arg| {
            let harmless = facts
                .ty(file, arg.value.id)
                .is_some_and(|ty| !holds_owned(fresh.bearing, ty));
            harmless || constructs(&arg.value, facts, file, fresh)
        });
    }
    fresh.by_name(head)
}

/// Whether `init` creates storage this body is the only holder of, reached
/// through `fields`.
///
/// Two ways, and the first is what lets a builder be built by a function.
/// When the whole initialiser is a [`constructs`] — `StringBuilder.withCapacity(16)`,
/// `ByteBuffer.allocate(64)`, `Builder(buffer: ByteBuffer.allocate(64))` —
/// then *every* field path inside it is fresh, because the value was built
/// here out of parts that were built here: there is no field of it that some
/// other holder could have supplied. Nothing needs to be found in the
/// argument list, which is what makes `withCapacity` work where looking for a
/// labelled `buffer:` argument finds none.
///
/// Failing that, and only with a field path to follow, the initialiser has to
/// be a literal this body wrote, so that the named field's own initialiser
/// can be asked the same question.
fn establishes(
    init: &Expr,
    fields: &[String],
    facts: &Facts,
    file: FileId,
    fresh: &Freshness<'_>,
) -> bool {
    if constructs(init, facts, file, fresh) {
        return true;
    }
    let Some((first, rest)) = fields.split_first() else {
        return false;
    };
    match &init.kind {
        ExprKind::Call { args, .. } => args
            .iter()
            .find(|arg| arg.label.as_ref().is_some_and(|label| label.node == *first))
            .is_some_and(|arg| establishes(&arg.value, rest, facts, file, fresh)),
        _ => false,
    }
}

/// Whether this expression allocates a vector nothing else holds a handle to.
///
/// # Freshness is a fact `cove-schema` states, not a name this pass matches
///
/// The answer used to be three method names matched against the source
/// regardless of what they were called on — `of`, `toVector`, `snapshot` —
/// which is exactly the coupling
/// [issue #270](https://github.com/myuon/cove/issues/270) named: a fourth
/// builtin that answers a fresh `Vector` needed a fourth name added here, and
/// a call that merely *shared* one of these three names — a user's own
/// `snapshot()` on an unrelated type, reached through a value of that type —
/// would have been read as fresh too, because nothing here ever asked what
/// `base` actually was.
///
/// Now the call has to *resolve* to a builtin entry `cove-schema` marks
/// [`MethodSchema::fresh`](cove_schema::builtins::MethodSchema::fresh), and
/// resolving it is what tells the two call shapes apart:
///
/// - `Vector.of(...)`: an associated function, named through the type
///   itself. There is no value to type — [`Facts::ty`] answers `None` for
///   `base` for exactly this reason (see the `facts` module) — so `base`
///   is read as a builtin type's own name instead, the same name
///   `cove_schema::is_builtin_type` uses to admit `Vector.of(...)` in the
///   first place.
/// - `array.toVector()`, `vector.snapshot()`: a method, named through a
///   receiver whose type the checker already settled and recorded. That
///   type is read off [`Facts::ty`] and turned into the builtin schema it
///   names, so `array.snapshot()` and `vector.snapshot()` are answered from
///   two different entries even though the source spells the call the same
///   way.
///
/// A call that resolves to neither — a declared function, a method of a
/// declared type, or a builtin entry the schema does not mark `fresh` —
/// answers `false`. That includes a call to a Cove-written wrapper such as
/// `std.vector.filter`, whose own `out.freeze()` this pass proves the
/// ordinary way from the `Vector.of()` a few lines above it in the same
/// body: a *caller* of `filter` never reaches this function at all, because
/// `filter`'s result is not the direct initialiser of anything `creates`
/// looks at. See `MethodSchema::fresh` for who may assert freshness and why
/// a declared `fn` is not on that list.
fn creates(init: &Expr, facts: &Facts, file: FileId) -> bool {
    let ExprKind::Call { callee, .. } = &init.kind else {
        return false;
    };
    let ExprKind::Field { base, name } = &callee.kind else {
        return false;
    };
    if let ExprKind::Ident(head) = &base.kind {
        if facts.ty(file, base.id).is_none() {
            if let Some(schema) = cove_schema::builtin(head) {
                return schema
                    .associated_function(&name.node)
                    .is_some_and(|method| method.fresh);
            }
        }
    }
    facts
        .ty(file, base.id)
        .and_then(crate::typeck::builtin_schema_of)
        .and_then(|schema| schema.method(&name.node))
        .is_some_and(|method| method.fresh)
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
    // The body's own tail is an answering expression, and it is the one the
    // walk cannot recognise on its own: every nested block's tail reaches
    // `Walk::block` the same way this one does.
    //
    // A tail that is itself a `return` answers nothing — the operand is the
    // answer, and the walk has already recorded it — so counting the `return`
    // as well would make a body written `{ return Builder.withCapacity(n) }`
    // permanently unprovable for a reason that is about punctuation.
    if let Some(tail) = &body.block.tail {
        if !matches!(tail.kind, ExprKind::Return(_)) {
            walk.scan.answers.push(tail);
        }
    }
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
                self.scan.answers.push(value);
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
            .is_some_and(|ty| self.holds_owned(ty));
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
                    .is_some_and(|ty| self.holds_owned(ty))
            })
            .collect();
        let elsewhere = |at: usize| {
            containers
                .iter()
                .enumerate()
                .any(|(j, held)| *held && j != at)
        };
        let offset = usize::from(receiver.is_some());
        for (at, arg) in args.iter().enumerate() {
            let inout = args
                .iter()
                .enumerate()
                .any(|(j, other)| other.is_var && j != at);
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
    ///
    /// Two calls consume, and they are the two transitions the language has:
    /// `Vector.freeze()`, which relabels a vector's store into an immutable
    /// `Array`, and `ByteBuffer.finish()`, which relabels a buffer's run into a
    /// `String`. ADR 0052 asks for exactly this — "finishing requires the same
    /// conservative local uniqueness proof as `Vector.freeze()`" — so a finish
    /// is recorded here as a freeze is, and everything downstream of this
    /// function treats the two identically.
    ///
    /// The receiver's *type* is asked and not the name alone, because a name is
    /// not a transition: a program's own `finish()` on a declared type is an
    /// ordinary method, and `examples/values`'s `BookingDraft.finish` is one.
    fn method(&mut self, call: &'a Expr, base: &'a Expr, name: &str, result_holds: bool) {
        let receiver = self.place_of(base);
        let consumes = match self.facts.ty(self.file, base.id) {
            Some(Ty::Vector(_)) if name == "freeze" => Some(Transition::Freeze),
            Some(Ty::ByteBuffer) if name == "finish" => Some(Transition::Finish),
            _ => None,
        };
        if let Some(transition) = consumes {
            if let Some(place) = receiver.clone() {
                self.scan.freezes.push(Consume {
                    place,
                    span: call.span,
                    regions: self.regions.clone(),
                    terminal: self.terminal,
                    transition,
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

    /// Whether a value of this type can reach a linear owner, against this
    /// walk's [`Bearing`].
    fn holds_owned(&self, ty: &Ty) -> bool {
        holds_owned(self.bearing, ty)
    }
}

/// Whether a value of this type can reach a linear owner: a `Vector` or a
/// `ByteBuffer`.
///
/// See [`Bearing`] for what an owner is and why the two are one question.
/// This is asked of every operand of every call, and what it decides is
/// whether that operand is somewhere a callee could *keep* the handle it
/// was given — so a type it answered `false` about wrongly would be a
/// consumption proved over storage another holder survived with.
///
/// A free function rather than a [`Walk`] method because the freshness
/// summary — [`answers_fresh`] — asks the same question outside any walk: a
/// struct literal's argument is harmless exactly when it cannot reach an
/// owner, and that has to be the *same* question the escape analysis asks or
/// the two would disagree about what an argument can carry.
fn holds_owned(bearing: &Bearing, ty: &Ty) -> bool {
    match ty {
        Ty::Vector(_) | Ty::ByteBuffer => true,
        Ty::Array(inner)
        | Ty::Set(inner)
        | Ty::Option(inner)
        | Ty::Task(inner)
        | Ty::Shared(inner) => holds_owned(bearing, inner),
        Ty::Map(key, value) | Ty::MapEntry(key, value) | Ty::Result(key, value) => {
            holds_owned(bearing, key) || holds_owned(bearing, value)
        }
        Ty::Struct(name, args) | Ty::Enum(name, args) => {
            bearing.contains(simple_name(name)) || args.iter().any(|ty| holds_owned(bearing, ty))
        }
        Ty::Fn(signature) => {
            signature.params.iter().any(|ty| holds_owned(bearing, ty))
                || holds_owned(bearing, &signature.ret)
        }
        _ => false,
    }
}

/// A type's name without the module that qualifies it.
fn simple_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Every declared type whose values can reach a linear owner, to a fixpoint.
///
/// Read off the written types rather than the checked ones, because what is
/// wanted is one bit per declaration and the declarations are what the
/// package holds. A name is compared without its module for the reason
/// [`Bearing`] states.
fn owner_bearing(program: &Program) -> Bearing {
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
            if types.iter().any(|ty| names_an_owner(ty, &bearing)) {
                bearing.insert((*name).to_string());
                changed = true;
            }
        }
        if !changed {
            return bearing;
        }
    }
}

/// Whether a written type names an owner — `Vector` or `ByteBuffer` — or a type
/// already known to bear one.
///
/// The *written* name and not a checked type, which is why the two owners are
/// spelled out here as strings. Nothing generic or aliased is resolved: a type
/// alias for a `ByteBuffer`, or a type parameter instantiated with one, is not
/// recognised. That is the same conservatism the `Vector` half has always had,
/// and it fails in the safe direction only for the fixpoint's own purpose —
/// what a *call site* is holding comes from [`Walk::holds_owned`], which reads
/// the checker's settled types.
fn names_an_owner(ty: &Type, bearing: &Bearing) -> bool {
    match &ty.kind {
        TypeKind::Named { path, args } => {
            let name = path
                .last()
                .map(|segment| segment.node.as_str())
                .unwrap_or_default();
            name == "Vector"
                || name == "ByteBuffer"
                || bearing.contains(name)
                || args.iter().any(|arg| names_an_owner(arg, bearing))
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
                    .is_some_and(|ty| names_an_owner(ty, bearing))
            }) || return_type
                .as_ref()
                .is_some_and(|ty| names_an_owner(ty, bearing))
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
        errors_of_package(source, false)
    }

    /// The same, with the standard library attached.
    ///
    /// Only for a test that names something the standard library declares —
    /// `std.stringbuilder.StringBuilder`. It is not the default because
    /// attaching costs eleven more modules to parse and check per test, and
    /// because a test that does not name one is asking a question about this
    /// pass and not about the package around it.
    fn errors_with_std(source: &str) -> Vec<Diagnostic> {
        errors_of_package(source, true)
    }

    fn errors_of_package(source: &str, with_std: bool) -> Vec<Diagnostic> {
        let mut sources = SourceMap::new();
        let path = PathBuf::from("main.cove");
        let file = sources.add(path.clone(), source);
        let ast = cove_syntax::parse_file(&sources, file).expect("test source parses");
        let mut modules = BTreeMap::from([(
            "main".to_string(),
            Module {
                name: "main".to_string(),
                dir: PathBuf::from("main"),
                units: vec![Unit { file, path, ast }],
            },
        )]);
        if with_std {
            for (name, module) in
                crate::stdlib::attach(&mut sources).expect("the standard library parses")
            {
                modules.insert(name, module);
            }
        }
        let package = Package {
            root: PathBuf::new(),
            config: crate::config::Config::default(),
            modules,
        };
        let program = crate::resolve::resolve(&package).expect("test source resolves");
        check(&package, &program)
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == cove_diag::Severity::Error)
            .collect()
    }

    #[track_caller]
    fn proves(source: &str) {
        report(errors_of(source));
    }

    /// [`proves`], of a source that names the standard library.
    #[track_caller]
    fn proves_with_std(source: &str) {
        report(errors_with_std(source));
    }

    #[track_caller]
    fn report(errors: Vec<Diagnostic>) {
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
        sole(errors_of(source))
    }

    /// [`refuses`], of a source that names the standard library.
    #[track_caller]
    fn refuses_with_std(source: &str) -> Diagnostic {
        sole(errors_with_std(source))
    }

    #[track_caller]
    fn sole(mut errors: Vec<Diagnostic>) -> Diagnostic {
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
    /// **A vector handed to a call as its own `var` argument is still the
    /// caller's afterwards.**
    ///
    /// A `var` argument is a write-through borrow that ends when the call
    /// returns, and the three ways a handle outlives a call are the three
    /// this module's `call` documents: the callee's result, another `var`
    /// argument the caller can see, or another operand that is a container.
    /// For the `var` argument *itself* the second route would mean writing
    /// it into itself, which needs a `Vector<T>` whose `T` is that same
    /// `Vector<T>` and is not a type Cove can write.
    ///
    /// This was refused, and the refusal cost the one pattern the language
    /// has for building a sequence: fill a `Vector` through a call, then
    /// `freeze` it. `examples/covefmt` wrote `"".join(out.toArray())` at nine
    /// sites because of it, and `toArray` copies the whole store where
    /// `freeze` re-labels it in place.
    #[test]
    fn a_var_argument_is_not_made_shared_by_being_the_var_argument() {
        proves(
            "\
fn fill(var into: Vector<Int>) {
  into.push(1)
}

fn build() -> Array<Int> {
  var items = Vector.of(1, 2)
  fill(var items)
  items.freeze()
}
",
        );
        // A *second* container beside it is the case that still escapes: the
        // callee may write that one into this one, and which way round the
        // types would allow is not something this asks.
        let beside = refuses(
            "\
fn fill(var into: Vector<Vector<Int>>, from: Vector<Int>) {
  into.push(from)
}

fn build() -> Array<Int> {
  var rows = Vector.of<Vector<Int>>()
  var items = Vector.of(1, 2)
  fill(var rows, items)
  items.freeze()
}
",
        );
        assert_eq!(
            beside.labels[0].message,
            "`items` escapes into a call that writes through a `var` argument here"
        );
    }

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
    ///
    /// The call is to a function whose answer is its own *parameter*, which is
    /// the one thing [`Freshness`] will not derive: `handed` answers storage
    /// its caller supplied, so `items` may be the second handle to it. Written
    /// with an explicit `return` because a tail expression and a `return`
    /// operand are the same position to the summary and this pins the one that
    /// is easier to get wrong.
    #[test]
    fn a_binding_this_body_did_not_create_is_refused() {
        let error = refuses(
            "\
fn handed(items: Vector<Int>) -> Vector<Int> {
  return items
}

fn build(given: Vector<Int>) -> Array<Int> {
  var items = handed(given)
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

    // -- issue #270: freshness is a schema fact, not a name this pass reads -

    /// `Vector.snapshot()` is `cove-schema`'s third `fresh` entry, and the
    /// one that needs a receiver's *settled type* rather than a bare `Ident`
    /// to resolve: unlike `Vector.of(...)`, `items.snapshot()` is reached
    /// through a value, and [`creates`] has to read that value's type off
    /// [`Facts::ty`] and turn it into the right builtin schema before it can
    /// ask whether `snapshot` is `fresh` there. Binding the result first,
    /// rather than freezing it as a temporary the way
    /// [`a_temporary_receiver_needs_no_proof`] does, is what exercises
    /// [`establishes`] rather than only the direct case.
    #[test]
    fn a_vectors_own_snapshot_is_a_fresh_primitive_result() {
        proves(
            "\
fn build(items: Vector<Int>) -> Array<Int> {
  var copy = items.snapshot()
  copy.freeze()
}
",
        );
    }

    /// The regression this issue is named for: before, `creates()` matched
    /// the method name `toVector` against *any* receiver, so a user's own
    /// method of that name — on a type `cove-schema` says nothing about —
    /// was read as fresh too. `Holder.toVector()` here hands back a field
    /// it did not just allocate, and a caller that binds and freezes it has
    /// exactly the alias the proof exists to catch: `holder` and `copy`
    /// would share `holder.items`'s storage. A non-fresh result has to stay
    /// refused now that the check is `cove-schema`-driven, the same as it
    /// was refused by luck before — this program is why "by luck" was not
    /// good enough.
    #[test]
    fn a_declared_methods_result_is_not_fresh_even_when_it_shares_a_builtins_name() {
        let error = refuses(
            "\
struct Holder {
  items: Vector<Int>
}

impl Holder {
  fn toVector(self) -> Vector<Int> {
    self.items
  }
}

fn build(holder: Holder) -> Array<Int> {
  var copy = holder.toVector()
  copy.freeze()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.message,
            "`freeze()` cannot prove that `copy` holds the only handle to its storage"
        );
    }

    /// The wrapper case the issue asks to think hardest about, in the shape
    /// `std.vector.filter` and `std.array.filter` are actually written:
    /// `var out = Vector.of(); ...; out.freeze()`. That `freeze()` is
    /// proved the ordinary local way, from the `Vector.of()` a few lines
    /// above it in the very same body — nothing here has to know `make` is
    /// a "wrapper" for its own proof to go through.
    ///
    /// What *also* holds, and did not when this test was first written, is
    /// that a wrapper whose answer is a construction is fresh to whoever calls
    /// it. `make`'s single answering expression is `Vector.of(1, 2)`, which
    /// builds the vector on the spot, so [`answers_fresh`] derives the
    /// summary and `log.freeze()` in the caller is proved from it — still
    /// without any declaration claiming anything. The two halves are here
    /// together because they are the pair a reader needs: a wrapper's own
    /// `freeze()` is proved locally, and its *result* is proved by a summary
    /// of its body.
    #[test]
    fn a_cove_wrappers_return_is_fresh_when_its_body_builds_the_answer() {
        proves(
            "\
fn make() -> Array<Int> {
  var out = Vector.of(1, 2)
  out.freeze()
}
",
        );
        proves(
            "\
fn make() -> Vector<Int> {
  Vector.of(1, 2)
}

fn build() -> Array<Int> {
  var log = make()
  log.freeze()
}
",
        );
    }

    /// The same wrapper, renamed, and answering a parameter instead of
    /// building one. `creates()` never reads a call's callee name at all once
    /// the callee is not a builtin's own `Field` — it asks whether the call
    /// *resolves* to a schema entry, and a declared function does not,
    /// whatever it is spelled. Naming it `toVector` here — a name
    /// `cove-schema` itself marks `fresh` on a different type — is
    /// deliberate: if this pass matched by name anywhere in this path, this is
    /// the program that would prove when it must not.
    ///
    /// The body answers `seed` rather than a construction, so
    /// [`answers_fresh`] declines it too, and the two reasons are independent:
    /// the name buys nothing, and neither does the shape.
    #[test]
    fn renaming_the_wrapper_changes_nothing() {
        let error = refuses(
            "\
fn toVector(seed: Vector<Int>) -> Vector<Int> {
  seed
}

fn build(given: Vector<Int>) -> Array<Int> {
  var log = toVector(given)
  log.freeze()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.labels[0].message,
            "`log` is initialised from a value this function did not create, so its storage \
             may already have another handle"
        );
    }
    // ------------------------------------------------ ADR 0052's byte buffer
    //
    // `ByteBuffer.finish()` is the second consuming transition and it is proved
    // by the same machinery, so these are the `Vector` tests above asked again
    // of a buffer. What is worth pinning is that nothing had to be added per
    // transition: the creation is trusted because `cove-schema` marks
    // `allocate` fresh, and the struct that wraps a buffer is registered as
    // bearing linear state because `names_an_owner` recognises the written type.

    /// The shape `finish()` was written for: allocate, append, hand the string
    /// over.
    #[test]
    fn a_buffer_built_here_and_finished_is_proved() {
        proves(
            "\
fn build(upTo: Int) -> String {
  var out = ByteBuffer.allocate(16)
  for n in 1..upTo {
    out.appendByte(65)
  }
  out.finish()
}
",
        );
    }

    /// A buffer passed as a `var` argument through a recursion is still the
    /// caller's to finish. This is the property ADR 0052 exists for — the owner
    /// is stable, so the callee appends to the very buffer the caller will
    /// finish — and the pass must not read the `var` argument as an escape.
    #[test]
    fn a_buffer_passed_as_a_var_argument_is_still_finishable() {
        proves(
            "\
fn fill(var out: ByteBuffer, depth: Int) {
  out.appendByte(65)
  if depth > 0 {
    fill(var out, depth - 1)
  }
}

fn build() -> String {
  var out = ByteBuffer.allocate(8)
  fill(var out, 3)
  out.finish()
}
",
        );
    }

    /// A struct whose field is a `ByteBuffer` is owner-bearing, so its `var
    /// self` method that finishes the field demands a unique receiver — and a
    /// caller that wrote the literal discharges it.
    ///
    /// This is `StringBuilder` in miniature, and it is what `names_an_owner`
    /// recognising `ByteBuffer` buys: before it did, the `Consume` recorded
    /// inside `finish` was propagated to a receiver nothing had registered as
    /// holding linear state, and the demand was recorded and then never checked.
    #[test]
    fn a_struct_wrapping_a_buffer_carries_the_demand_to_its_caller() {
        proves(
            "\
struct Builder {
  buffer: ByteBuffer
}

impl Builder {
  fn add(var self, text: String) {
    self.buffer.appendSlice(text, 0, text.byteLength())
  }

  fn finish(var self) -> String {
    self.buffer.finish()
  }
}

fn build() -> String {
  var out = Builder(buffer: ByteBuffer.allocate(16))
  out.add(\"hello\")
  out.finish()
}
",
        );
    }

    /// A second binding is a second handle, for a buffer as for a vector — and
    /// the correction is not `toArray()`, because a buffer has no copying
    /// conversion to offer.
    #[test]
    fn a_second_binding_defeats_a_finish_and_the_help_does_not_offer_to_array() {
        let error = refuses(
            "\
fn build() -> String {
  var out = ByteBuffer.allocate(8)
  var alias = out
  alias.appendByte(65)
  out.finish()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.message,
            "`finish()` cannot prove that `out` holds the only handle to its storage"
        );
        assert_eq!(
            error.labels[0].message,
            "`out` is copied into another binding here"
        );
        let help = error.help.expect("a refusal says what to do instead");
        assert!(!help.contains("toArray"), "{help}");
        assert!(help.contains("builder"), "{help}");
    }

    /// A call that could keep the buffer defeats the proof. `keep` answers a
    /// `Builder`, which bears a buffer, so the handle it was given may be in
    /// the answer.
    #[test]
    fn a_buffer_that_escapes_into_a_call_is_refused() {
        let error = refuses(
            "\
struct Builder {
  buffer: ByteBuffer
}

fn keep(out: ByteBuffer) -> Builder {
  Builder(buffer: out)
}

fn build() -> String {
  var out = ByteBuffer.allocate(8)
  let held = keep(out)
  out.finish()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.labels[0].message,
            "`out` escapes into a call that may answer with it here"
        );
    }

    /// A buffer read after it was finished is reported where the read is.
    #[test]
    fn a_read_after_the_finish_is_reported() {
        let error = refuses(
            "\
fn build() -> Int {
  var out = ByteBuffer.allocate(8)
  let text = out.finish()
  out.length()
}
",
        );
        assert_eq!(error.code, USED_AFTER_FREEZE);
        assert_eq!(
            error.message,
            "`out` is read after its storage was consumed"
        );
        assert_eq!(error.labels[0].message, "`finish()` took the storage here");
    }

    // ------------------------------------- a derived freshness summary (#0052)
    //
    // `answers_fresh` proves from a body what no declaration may claim, and the
    // negative tests below are the load-bearing half. Each one is written so
    // that it would stop refusing — and therefore fail — if the rule were
    // widened to accept a bare `return x`, which is the one widening that would
    // turn this summary from a syntactic test into a wrong escape analysis.

    /// The shape ADR 0052 wants a builder to be built in: an associated
    /// function answers one, and the caller finishes it.
    ///
    /// Three clauses of [`constructs`] at once. `Builder.withCapacity`'s answer
    /// is a struct literal; its `buffer:` argument is a builtin fresh call; its
    /// `limit:` argument is an `Int`, which cannot reach an owner and so is
    /// waved through without being a construction itself. `make` is then the
    /// fixpoint's second round: a call to something the first round added.
    #[test]
    fn an_associated_function_that_builds_one_answers_fresh() {
        proves(
            "\
struct Builder {
  buffer: ByteBuffer
  limit: Int
}

impl Builder {
  fn withCapacity(capacity: Int) -> Builder {
    Builder(buffer: ByteBuffer.allocate(capacity), limit: capacity)
  }

  fn finish(var self) -> String {
    self.buffer.finish()
  }
}

fn make() -> Builder {
  Builder.withCapacity(16)
}

fn build() -> String {
  var out = Builder.withCapacity(16)
  out.finish()
}

fn again() -> String {
  var out = make()
  out.finish()
}
",
        );
    }

    /// The standard library's own builder, finished by the caller that asked
    /// for it — the program that was refused before this summary existed, and
    /// the reason `StringBuilder` can be `opaque` at all.
    #[test]
    fn a_standard_builder_from_with_capacity_is_finishable() {
        proves_with_std(
            "\
use std.stringbuilder.StringBuilder

fn build() -> String {
  var out = StringBuilder.withCapacity(16)
  out.append(\"a\")
  out.finish()
}
",
        );
    }

    /// The same builder through a recursion that appends into it by `var`.
    ///
    /// This is ADR 0052's stable owner as a program observes it: every frame
    /// names the one builder, the run under it is replaced on the way, and the
    /// caller still holds the only handle when it finishes.
    #[test]
    fn a_standard_builder_survives_a_recursive_var_argument() {
        proves_with_std(
            "\
use std.stringbuilder.StringBuilder

fn emit(var out: StringBuilder, depth: Int) {
  out.append(\"x\")
  if depth > 0 {
    emit(var out, depth - 1)
  }
}

fn build() -> String {
  var out = StringBuilder.withCapacity(4)
  emit(var out, 3)
  out.finish()
}
",
        );
    }

    /// A function whose answer is its own parameter answers storage its caller
    /// supplied, so finishing the result would finish something the caller may
    /// still be holding.
    ///
    /// The body is exactly `return buffer`. If that counted as a construction
    /// this would prove, and the proof would be wrong.
    #[test]
    fn a_function_that_answers_a_parameter_is_not_a_creation() {
        let error = refuses(
            "\
fn handed(buffer: ByteBuffer) -> ByteBuffer {
  return buffer
}

fn build(given: ByteBuffer) -> String {
  var out = handed(given)
  out.finish()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.message,
            "`finish()` cannot prove that `out` holds the only handle to its storage"
        );
        assert_eq!(
            error.labels[0].message,
            "`out` is initialised from a value this function did not create, so its storage \
             may already have another handle"
        );
    }

    /// A struct literal is a construction only when its arguments are. `wrap`
    /// answers a literal whose `buffer:` is a parameter, so the builder it
    /// hands back is wrapped around storage the caller supplied — and the
    /// wrapper being freshly allocated says nothing about the run inside it.
    #[test]
    fn a_struct_literal_over_a_parameter_is_not_a_construction() {
        let error = refuses(
            "\
struct Builder {
  buffer: ByteBuffer
}

impl Builder {
  fn finish(var self) -> String {
    self.buffer.finish()
  }
}

fn wrap(buffer: ByteBuffer) -> Builder {
  Builder(buffer: buffer)
}

fn build(given: ByteBuffer) -> String {
  var out = wrap(given)
  out.finish()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.message,
            "`Builder.finish()` consumes `out.buffer`, and this call cannot prove that it holds \
             the only handle to its storage"
        );
        assert_eq!(
            error.labels[0].message,
            "`out.buffer` is initialised from a value this function did not create, so its \
             storage may already have another handle"
        );
    }

    /// The case that makes the syntactic rule necessary rather than merely
    /// convenient. `leaked` *does* create its buffer — and then gives a handle
    /// to `stash`, which puts it in a `Holder` that outlives the call, and only
    /// then answers the name.
    ///
    /// A rule that accepted the answer because the storage was created
    /// somewhere in the body would prove this, and the proof would be wrong:
    /// `kept` is holding the same run the caller is about to finish. A rule
    /// that requires the answer to be *built where it is answered* has nothing
    /// to search for, because nothing can have got hold of a value that did not
    /// exist until the answer was built.
    #[test]
    fn a_construction_that_passed_through_a_call_before_being_answered_is_refused() {
        let error = refuses(
            "\
struct Holder {
  buffer: ByteBuffer
}

fn stash(buffer: ByteBuffer) -> Holder {
  Holder(buffer: buffer)
}

fn leaked() -> ByteBuffer {
  var out = ByteBuffer.allocate(8)
  let kept = stash(out)
  out
}

fn build() -> String {
  var out = leaked()
  out.finish()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.labels[0].message,
            "`out` is initialised from a value this function did not create, so its storage \
             may already have another handle"
        );
    }

    /// A local `fn` shadows the module function of the same name, and the
    /// summary refuses rather than resolving the wrong one.
    ///
    /// `make` at module level answers a construction and is in the set. The
    /// `make` inside `build` is a different declaration with no key of its own —
    /// `bodies` names only what a module declares — so resolving this call to
    /// the module's entry would prove a `freeze()` over a vector `build`'s
    /// caller is holding.
    ///
    /// What stops it is not a scope table in [`constructs`] but the one line
    /// that asks whether the checker gave the *callee* a type. A local `fn` is
    /// a binding holding a closure, so its callee has one, and a call through a
    /// value names no declaration to look up — exactly the distinction the
    /// `facts` module says that absence carries. This test is here because that
    /// line reads like a formality and is not one: without it this program
    /// proves.
    #[test]
    fn a_local_fn_shadowing_a_fresh_module_function_is_refused() {
        let error = refuses(
            "\
fn make() -> Vector<Int> {
  Vector.of(1, 2)
}

fn build(given: Vector<Int>) -> Array<Int> {
  fn make(seed: Vector<Int>) -> Vector<Int> {
    seed
  }
  var log = make(given)
  log.freeze()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.labels[0].message,
            "`log` is initialised from a value this function did not create, so its storage \
             may already have another handle"
        );
    }

    /// A builder the summary proved fresh is still only as unique as the body
    /// holding it keeps it. The summary answers where the storage came from and
    /// nothing else; a second binding is still a second handle.
    #[test]
    fn an_aliased_standard_builder_is_still_refused() {
        let error = refuses_with_std(
            "\
use std.stringbuilder.StringBuilder

fn build() -> String {
  var a = StringBuilder.withCapacity(8)
  var b = a
  b.append(\"x\")
  a.finish()
}
",
        );
        assert_eq!(error.code, NOT_UNIQUE);
        assert_eq!(
            error.message,
            "`StringBuilder.finish()` consumes `a.buffer`, and this call cannot prove that it \
             holds the only handle to its storage"
        );
        assert_eq!(
            error.labels[0].message,
            "`a` is copied into another binding here"
        );
    }

    /// And a proved builder is consumed by the finish, so reading it afterwards
    /// is the second diagnostic and not silence.
    #[test]
    fn a_standard_builder_read_after_finishing_is_refused() {
        let error = refuses_with_std(
            "\
use std.stringbuilder.StringBuilder

fn build() -> Int {
  var out = StringBuilder.withCapacity(8)
  let text = out.finish()
  out.length()
}
",
        );
        assert_eq!(error.code, USED_AFTER_FREEZE);
        assert_eq!(
            error.message,
            "`out` is read after its storage was consumed"
        );
        assert_eq!(
            error.labels[0].message,
            "`StringBuilder.finish()` took the storage here"
        );
    }
}
