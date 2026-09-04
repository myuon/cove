//! Recording a run so that a page can scrub through it.
//!
//! [`cove_runtime::Debugger`] is asked before every instruction, and
//! `cove debug` answers that question by *blocking on stdin inside the
//! callback*: the machine calls the debugger and never the other way round,
//! because the dispatch loop holds a `std::thread::scope` borrow that cannot
//! leave the call that made it. `crates/cove-runtime/src/vm/debug.rs` argues
//! that at length, and nothing here changes it.
//!
//! # Why this records instead of stepping
//!
//! A Web Worker cannot block waiting for a message from the page. There is
//! no synchronous `receive()`; `onmessage` is delivered by the event loop,
//! and an event loop that is inside a wasm call is an event loop that is not
//! running. So the shape `cove debug` uses — stop the machine, ask a person,
//! resume — has no browser spelling. The one construction that would give it
//! one is `Atomics.wait` on a `SharedArrayBuffer`, and a `SharedArrayBuffer`
//! is only constructible on a cross-origin-isolated page, which needs the
//! server to send `Cross-Origin-Opener-Policy: same-origin` and
//! `Cross-Origin-Embedder-Policy: require-corp`. GitHub Pages sends neither
//! and cannot be made to.
//!
//! So the direction is inverted a second time. This [`Debugger`] does not
//! ask anything: it *writes down* what it saw, the run goes to completion (or
//! to its fuel, or to its deadline), the worker hands the whole recording to
//! the page in one message, and the page scrubs through it. Nothing blocks,
//! nothing is shared, and the timeline runs backwards as readily as forwards
//! — which for reading a program is better than stepping, because "what did
//! `n` hold two lines ago" is a question live stepping answers only by
//! starting again.
//!
//! This is not a refusal of live stepping forever. It is what is possible
//! without COOP/COEP. An embedder that serves those two headers could keep
//! this file and add a second `Debugger` that waits on a `SharedArrayBuffer`,
//! and the machine side would not change at all.
//!
//! # What a moment holds
//!
//! One captured stop — a *moment* — is what the four panes need and nothing
//! else:
//!
//! - **Source**: the 1-based line and column the instruction was written at,
//!   and the instruction's span as a pair of UTF-16 offsets. The page marks
//!   that span in the editor itself rather than in a second copy of the text,
//!   so it needs an end and not only a start, and it needs both counted the
//!   way a JavaScript string is indexed. `crates/cove-wasm/src/highlight.rs`
//!   argues UTF-16 at length; the same argument holds here, and an em dash in
//!   a comment above the marked line is enough to make it matter.
//! - **Instructions**: an index into a shared table of disassembled
//!   functions, and the pc inside it.
//! - **Runtime**: the backtrace, innermost first — each frame's function,
//!   pc, line and every local in scope there with its rendered value.
//! - **Memory**: every heap object named by a `ref` word of one of those
//!   locals, rendered with its fields, and on each local the addresses of
//!   the words that named them.
//!
//! Plus the bookkeeping a timeline needs: the instruction count, the task,
//! the frame depth, and *why* this instruction was captured.
//!
//! The disassembly is in a table beside the moments rather than inside each
//! one. A recording of a loop is hundreds of moments in one function, and
//! repeating that function's instructions in each of them was measured to be
//! most of the answer. Interning is where this format's compression comes
//! from; see [`crate::debug_json`] for why the answer is still one blob.
//!
//! # What is captured, and what is bounded
//!
//! **The policy is `cove debug`'s line-change rule**, widened by one clause.
//! A stop is captured when it is the first, when the frame depth differs
//! from the last captured moment's — a call or a return — when the task
//! differs, or when the instruction was written outside the byte range of
//! the last captured moment's source line. The byte range is compared rather
//! than the line number for the reason `Session::line_mode` gives: a range
//! check is two comparisons and a line number is a binary search, and this
//! runs at every instruction.
//!
//! The depth clause is the widening, and it is there because
//! `Session::misses` names its absence as a defect: a callee whose body is
//! written on the line that calls it is stepped *over* rather than into,
//! because the line did not change. A recording that skipped a whole call
//! would give the Runtime pane a backtrace that jumped. Everything else in
//! that list still applies here, unchanged — a loop written on one line is
//! one moment per turn only because the depth or the callee changes, a
//! statement written across several lines produces several moments in
//! evaluation order so the line number can go backwards, and a moment is at
//! the first instruction carrying a new line, which is inside the expression
//! rather than at the statement's start, so a name assigned on that line
//! still shows its old value.
//!
//! **Three bounds, and each loses something nameable.**
//!
//! 1. [`MOMENTS`] moments, the *first* N rather than the last. Past it the
//!    recorder stops capturing and the run *keeps going*, so the outcome,
//!    the output and the answer are still the real ones — the recording is a
//!    prefix of the timeline and says so with `truncated`. First and not
//!    last because a ring would give a timeline that does not begin at the
//!    entry, and because only a prefix lets the recorder go quiet: once full
//!    it answers from an [`AtomicBool`] without taking its lock, which is
//!    what lets a long run reach its own end rather than its deadline.
//!    *What is lost is the end of a long run.* The number of moments that
//!    were dropped is deliberately not reported, because counting them means
//!    keeping the per-instruction check alive for the whole run, which is
//!    the cost this bound exists to stop paying.
//! 2. [`BYTES`] of rendered recording. Each moment is rendered to JSON as it
//!    is captured, so this bound is exact rather than estimated, and it is
//!    the one that holds when the moments are few and enormous — a deep
//!    stack of frames full of long strings. *What is lost is the same end of
//!    the same timeline*, and `truncated` says which bound stopped it.
//! 3. [`FRAMES`] frames per moment and [`OBJECTS`] objects per moment.
//!    Without these two the first bound would not bound memory at all: a
//!    thousand moments of a recursion a thousand deep is a million frames.
//!    A moment records its true `depth`, so a pane can say how many frames
//!    it is not showing. *What is lost is the outer end of a deep backtrace,
//!    and the heap past the thirty-second object a frame's locals named.*
//!
//! A recording that silently truncated would be worse than one that did not
//! exist, so every one of these reports itself: `truncated` names the bound,
//! `kept` counts what is there, and `depth` exceeds `frames.length` exactly
//! when frames were dropped.
//!
//! # What it costs the run
//!
//! A mutex acquisition per instruction, as `cove debug` pays, plus a span
//! and depth comparison. What it does not pay is the rendering: a backtrace
//! renders every local of every frame, and that happens only at a captured
//! moment.
//!
//! Measured under node against the release wasm, on a counting loop of
//! fourteen million instructions:
//!
//! | | |
//! | --- | ---: |
//! | `cove_run` | 119 ms |
//! | `cove_debug`, 1024 moments | 186 ms |
//! | `cove_debug`, 16384 moments | 326 ms |
//!
//! Two things are in that table. A recorded run of a program that overran
//! its bound early costs **1.6x** a plain one — that is the quiet path, the
//! relaxed load and the branch, for the fourteen million instructions after
//! the recording filled. And the fifteen thousand extra captured moments
//! cost 144 ms between the second row and the third, which is about **9 µs
//! per moment**: the rendering, and the price of asking for a longer
//! recording rather than of being watched at all.
//!
//! The third row also says the two bounds are calibrated against each other
//! rather than one of them being decoration. [`MOST_MOMENTS`] moments of
//! that loop render to 3.3 MB, just under [`BYTES`]; a program with deeper
//! frames or longer strings reaches the byte bound first, which is what it
//! is for.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cove_diag::{FileId, SourceMap, Span};
use cove_runtime::{Call, Debugger, Resume, Stop};

use crate::json;

/// How many moments a recording keeps by default.
///
/// A thousand moments is more of a program than a person will scrub through
/// in one sitting, and at the sizes measured in `web/README.md` it is a
/// recording a page can hold without noticing. A caller may ask for fewer,
/// or for more up to [`MOST_MOMENTS`].
pub const MOMENTS: usize = 1_024;

/// The most moments a caller may ask for.
///
/// A ceiling and not a suggestion: the whole point of the first bound is
/// that a page cannot ask for an unbounded recording, and a limit a caller
/// chooses is not a limit if the caller may choose infinity.
pub const MOST_MOMENTS: usize = 16_384;

/// The most rendered recording a run may accumulate, in bytes.
///
/// Four mebibytes of JSON is roughly what a browser will `postMessage` and
/// `JSON.parse` without a visible pause. It is a second bound and not a
/// replacement for the first, because the two fail on different programs:
/// this one catches a few enormous moments, and [`MOMENTS`] catches many
/// small ones.
pub const BYTES: usize = 4 << 20;

/// Frames captured per moment, innermost first.
pub const FRAMES: usize = 16;

/// Heap objects captured per moment.
pub const OBJECTS: usize = 32;

/// What the capture rule is, in one line, carried in the answer so that a
/// page can show it without hard-coding it.
const POLICY: &str = "the first instruction, every call and return, and the first instruction written on a new source line";

/// A `reach` that covers any function: [`Stop::code`] clamps to the code's
/// own ends, so this asks for all of it without needing to know its length.
///
/// Half of `usize` and not `u32::MAX`, which is what this said first. On a
/// 64-bit host the two are the same; on `wasm32-unknown-unknown` a `usize`
/// is 32 bits, `Stop::code` computed `pc + reach + 1`, and `u32::MAX + 1`
/// wrapped to zero — so every function was disassembled as the empty range
/// `0..pc`, a different range at every pc, and the interning that keys on a
/// function's length made a fresh entry for each. It answered correctly
/// under `cargo test` and wrongly in the browser, which is precisely the
/// class of bug `web/check.mjs` exists to catch, and it is the one it
/// caught.
///
/// `Stop::code` saturates both ends now, so `u32::MAX` would work too. This
/// stays as it is because a caller should not have to know that: half of
/// `usize` cannot overflow for any pc naming an instruction actually held in
/// memory, on any width.
const WHOLE: usize = usize::MAX / 2;

/// A [`Debugger`] that writes down what it saw instead of asking what to do.
pub struct Recorder {
    sources: Arc<SourceMap>,
    limit: usize,
    /// Whether a bound has been reached, read before the lock is taken.
    ///
    /// The fast path out. Once this is set the recording will not grow
    /// again, so the per-instruction question is one relaxed load and a
    /// branch rather than a mutex acquisition — measured at 1.6x a plain
    /// run over the fourteen million instructions after a recording filled,
    /// which is what lets such a run reach its own end rather than its
    /// deadline.
    full: AtomicBool,
    kept: Mutex<Kept>,
}

/// Everything one recording holds, behind one lock.
///
/// One lock and not several for the reason `cove debug`'s session gives: a
/// spawned task's machine asks the same debugger from that task's own
/// thread. The playground refuses `spawn`, so in practice there is one
/// asker; the lock is what makes that a fact about the environment rather
/// than an assumption in this file.
#[derive(Default)]
struct Kept {
    /// Whether a file's text is all ASCII, remembered after the first look.
    ///
    /// A UTF-16 offset is the byte offset when it is, which turns the two
    /// conversions a moment needs per frame into nothing at all. When it is
    /// not, the prefix is counted, and the cost is why this cache exists: a
    /// recording is up to [`MOST_MOMENTS`] moments of up to [`FRAMES`] frames
    /// and each frame carries a span.
    ascii: Vec<(FileId, bool)>,
    /// Each moment, already rendered to JSON.
    ///
    /// Rendered at capture rather than at the end so that [`BYTES`] is a
    /// measurement and not a guess.
    moments: Vec<String>,
    functions: Vec<Function>,
    bytes: usize,
    /// Which bound stopped the recording, if one did.
    truncated: Option<&'static str>,
    /// Where the last captured moment was, for the line-change rule.
    last: Option<Place>,
}

/// One disassembled function, interned across the moments that are in it.
struct Function {
    name: String,
    /// How many instructions it has, which is what tells two functions of
    /// the same qualified name apart when it can.
    len: u32,
    json: String,
}

/// The last captured moment's place, as the per-instruction check reads it.
struct Place {
    file: FileId,
    /// The byte range of the source line, so the check is a comparison
    /// rather than a search.
    from: u32,
    to: u32,
    depth: usize,
    task: u64,
}

impl Recorder {
    /// A recorder that keeps at most `moments` moments of a run of `sources`.
    ///
    /// `moments` is clamped into `1..=`[`MOST_MOMENTS`]; zero asks for the
    /// default, which is a bound and not the absence of one.
    pub fn new(sources: Arc<SourceMap>, moments: usize) -> Recorder {
        let limit = match moments {
            0 => MOMENTS,
            asked => asked.min(MOST_MOMENTS),
        };
        Recorder {
            sources,
            limit,
            full: AtomicBool::new(false),
            kept: Mutex::new(Kept::default()),
        }
    }

    /// The recording, as the JSON object [`crate::debug_json`] puts under
    /// `debug`.
    ///
    /// ```json
    /// {"moments":[...],"functions":[...],"kept":int,"limit":int,
    ///  "bytes":int,"truncated":"moments"|"bytes"|null,
    ///  "frames":int,"objects":int,"policy":string}
    /// ```
    pub fn json(&self) -> String {
        let kept = self.held();
        let moments = format!("[{}]", kept.moments.join(","));
        let functions = format!(
            "[{}]",
            kept.functions
                .iter()
                .map(|function| function.json.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        json::object([
            ("moments", moments),
            ("functions", functions),
            ("kept", kept.moments.len().to_string()),
            ("limit", self.limit.to_string()),
            ("bytes", kept.bytes.to_string()),
            ("truncated", json::or_null(kept.truncated.map(json::string))),
            ("frames", FRAMES.to_string()),
            ("objects", OBJECTS.to_string()),
            ("policy", json::string(POLICY)),
        ])
    }

    /// The recording, whether or not a panic poisoned the lock.
    ///
    /// A poisoned lock here means a moment's rendering panicked, and the
    /// moments captured before it are still exactly what they were. Losing
    /// them as well would turn one bug into no recording at all.
    fn held(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().unwrap_or_else(|held| held.into_inner())
    }
}

impl Debugger for Recorder {
    fn at(&self, stop: &Stop<'_>) -> Resume {
        // Before the lock: a full recording has nothing left to decide.
        if self.full.load(Ordering::Relaxed) {
            return Resume::Go;
        }
        let mut kept = self.held();
        if let Some(why) = kept.wanted(stop) {
            if kept.moments.len() >= self.limit {
                kept.truncated = Some("moments");
            } else if kept.bytes >= BYTES {
                kept.truncated = Some("bytes");
            }
            if kept.truncated.is_some() {
                self.full.store(true, Ordering::Relaxed);
            } else {
                let moment = kept.capture(stop, &self.sources, why);
                kept.bytes += moment.len();
                kept.moments.push(moment);
            }
        }
        // Never `Halt`. A recording that ended the run would answer a
        // question about a program with a program that did not finish, and
        // the outcome, the output and the answer beside the recording would
        // all be about a run nobody asked for.
        Resume::Go
    }
}

impl Kept {
    /// The per-instruction question: is this instruction a moment, and why?
    ///
    /// It reads three things off the stop — the span, the depth and the task
    /// — all of them copies the machine already had, and compares them
    /// against integers. It allocates nothing and reads no source.
    fn wanted(&self, stop: &Stop<'_>) -> Option<&'static str> {
        let Some(last) = &self.last else {
            return Some("entry");
        };
        let depth = stop.depth();
        if stop.task() != last.task {
            return Some("task");
        }
        if depth > last.depth {
            return Some("call");
        }
        if depth < last.depth {
            return Some("return");
        }
        let span = stop.span();
        let same = span.file == last.file && last.from <= span.start && span.start < last.to;
        (!same).then_some("line")
    }

    /// Whether `file` holds nothing but ASCII, looked up once per file.
    fn ascii(&mut self, sources: &SourceMap, file: FileId) -> bool {
        if let Some((_, held)) = self.ascii.iter().find(|(id, _)| *id == file) {
            return *held;
        }
        let held = sources.get(file).text.is_ascii();
        self.ascii.push((file, held));
        held
    }

    /// `span` as the pair of UTF-16 offsets a page slices its own string by.
    fn utf16(&mut self, sources: &SourceMap, span: Span) -> (usize, usize) {
        let ascii = self.ascii(sources, span.file);
        let text = &sources.get(span.file).text;
        (
            at_utf16(text, span.start, ascii),
            at_utf16(text, span.end, ascii),
        )
    }

    /// One moment, rendered.
    fn capture(&mut self, stop: &Stop<'_>, sources: &SourceMap, why: &'static str) -> String {
        let span = stop.span();
        let (line, col) = at_line(sources, span);
        let (from, to) = self.utf16(sources, span);
        self.last = Some(Place {
            file: span.file,
            from: line_from(sources, span),
            to: line_to(sources, span),
            depth: stop.depth(),
            task: stop.task(),
        });

        let depth = stop.depth();
        let mut frames = Vec::new();
        let mut objects: Vec<(u64, String)> = Vec::new();
        // The function the moment itself is in, which is the innermost
        // frame's. It is repeated out of the frames because the Instructions
        // pane follows the timeline whether or not a reader has selected a
        // frame, and `null` for the one stop with no frame at all.
        let mut top = None;
        for at in 0..depth.min(FRAMES) {
            let Some(call) = stop.frame(at) else { break };
            let function = self.intern(stop, sources, at, &call);
            top.get_or_insert(function);
            let locals = call
                .locals()
                .iter()
                .map(|local| {
                    let refs = local
                        .words()
                        .iter()
                        // Only a `ref` word names a heap object — it is the
                        // one representation the collector treats as a root
                        // — so this asks about the words that can answer
                        // rather than about every word of every local.
                        .filter(|word| word.holds() == "ref" && word.raw() != 0)
                        .filter_map(|word| remember(stop, &mut objects, word.raw()))
                        .collect::<Vec<_>>();
                    json::object([
                        ("name", json::string(local.name())),
                        ("value", json::string(local.value())),
                        ("at", local.at().to_string()),
                        ("width", local.width().to_string()),
                        ("refs", json::array(refs)),
                    ])
                })
                .collect::<Vec<_>>();
            let (from, to) = self.utf16(sources, call.span());
            frames.push(json::object([
                ("function", function.to_string()),
                ("pc", call.pc().to_string()),
                ("line", at_line(sources, call.span()).0.to_string()),
                // A selected frame moves the editor's mark to that frame's
                // own call site, which is the only thing that makes an outer
                // frame readable in a page with one editor rather than one
                // Source pane per frame.
                ("from", from.to_string()),
                ("to", to.to_string()),
                ("locals", json::array(locals)),
            ]));
        }

        json::object([
            ("at", stop.instructions().to_string()),
            ("task", stop.task().to_string()),
            (
                "function",
                json::or_null(top.map(|index| index.to_string())),
            ),
            ("pc", stop.pc().to_string()),
            ("line", line.to_string()),
            ("col", col.to_string()),
            ("from", from.to_string()),
            ("to", to.to_string()),
            ("depth", depth.to_string()),
            ("why", json::string(why)),
            ("frames", json::array(frames)),
            (
                "objects",
                json::array(objects.into_iter().map(|(_, json)| json)),
            ),
        ])
    }

    /// The index of `call`'s function in the shared table, disassembling it
    /// the first time it is seen.
    ///
    /// Keyed by the qualified name, with the pc checked against the length
    /// of what was disassembled. That is a heuristic and it is the best one
    /// available here: nothing public identifies a lowered function, and one
    /// generic function lowered twice produces two functions with one
    /// qualified name. The check catches the case where they differ in
    /// length; two instantiations of the same length are shown as one, whose
    /// instructions are the same modulo the layouts named in them. The pc a
    /// pane marks is right either way.
    fn intern(&mut self, stop: &Stop<'_>, sources: &SourceMap, at: usize, call: &Call) -> usize {
        let name = call.function();
        if let Some(index) = self
            .functions
            .iter()
            .position(|held| held.name == name && call.pc() < held.len)
        {
            return index;
        }
        let code = stop.code(at, WHOLE);
        let json = json::object([
            ("name", json::string(name)),
            (
                "code",
                json::array(code.iter().map(|line| {
                    json::object([
                        ("pc", line.pc().to_string()),
                        ("text", json::string(line.text())),
                        ("line", at_line(sources, line.span()).0.to_string()),
                    ])
                })),
            ),
        ]);
        self.functions.push(Function {
            name: name.to_string(),
            len: code.len() as u32,
            json,
        });
        self.functions.len() - 1
    }
}

/// Renders the object at `addr` into `objects` if it is one and there is
/// room, and answers the address as a JSON number for the local to point at.
///
/// The address is answered even when the object was already there, because a
/// local that names an object something else also names should still say so;
/// it is not answered when the word names nothing this heap holds, because
/// then it is not a reference a pane can follow.
fn remember(stop: &Stop<'_>, objects: &mut Vec<(u64, String)>, addr: u64) -> Option<String> {
    if objects.iter().any(|(held, _)| *held == addr) {
        return Some(addr.to_string());
    }
    if objects.len() >= OBJECTS {
        return None;
    }
    let object = stop.object(addr)?;
    objects.push((
        addr,
        json::object([
            ("at", addr.to_string()),
            ("name", json::string(object.name())),
            (
                "fields",
                json::array(object.fields().iter().map(|field| {
                    json::object([
                        ("name", json::string(field.name())),
                        ("value", json::string(field.value())),
                    ])
                })),
            ),
        ]),
    ));
    Some(addr.to_string())
}

/// A byte offset as a UTF-16 offset, which is what a JavaScript string is
/// indexed in.
///
/// `ascii` is the file's answer to "are the two the same number?", looked up
/// once by [`Kept::ascii`] rather than per span. Out-of-range offsets are
/// clamped and a mid-character one is walked back, because a marked span that
/// is one code unit wrong is better than a panic inside a debugger.
fn at_utf16(text: &str, at: u32, ascii: bool) -> usize {
    let mut at = (at as usize).min(text.len());
    if ascii {
        return at;
    }
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    text[..at].encode_utf16().count()
}

/// The 1-based line and column `span` starts at.
fn at_line(sources: &SourceMap, span: Span) -> (usize, usize) {
    sources.get(span.file).line_col(span.start)
}

/// The byte offset the source line holding `span` begins at.
///
/// Found by scanning outwards from the instruction's own offset, which is
/// `cove debug`'s idiom and for its reason: `SourceMap` exposes a line's
/// number and its text but not where it begins, and asking for the number
/// per instruction is the search this exists to avoid.
fn line_from(sources: &SourceMap, span: Span) -> u32 {
    let text = &sources.get(span.file).text;
    let at = (span.start as usize).min(text.len());
    text[..at].rfind('\n').map_or(0, |end| end + 1) as u32
}

/// The byte offset one past the end of that line.
fn line_to(sources: &SourceMap, span: Span) -> u32 {
    let text = &sources.get(span.file).text;
    let at = (span.start as usize).min(text.len());
    text[at..].find('\n').map_or(text.len(), |end| at + end + 1) as u32
}
