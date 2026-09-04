// Runs Cove programs through the wasm module under node, so that "it works in
// a browser" is checked rather than believed.
//
//   cargo build -p cove-wasm --target wasm32-unknown-unknown --release
//   node web/check.mjs
//
// A path may be given instead, for a module built into some other directory:
//
//   node web/check.mjs target/wasm32-unknown-unknown/debug/cove_wasm.wasm
//
// It loads through `cove.mjs`, the same module the page's worker loads
// through, so what this exercises is the loader the browser uses and not a
// second one written for the check. Exits non-zero on the first case that
// does not hold.

import { readdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { isDeepStrictEqual } from "node:util";
import { instantiate } from "./cove.mjs";
import { SAMPLES } from "./samples.mjs";

const wasm =
  process.argv[2] ??
  fileURLToPath(
    new URL("../target/wasm32-unknown-unknown/release/cove_wasm.wasm", import.meta.url),
  );

let failures = 0;

function check(name, held, want) {
  const ok = typeof want === "function" ? want(held) : held === want;
  if (!ok) {
    failures += 1;
    console.log(`  NOT OK  ${name}`);
    console.log(`          got ${JSON.stringify(held)}`);
  } else {
    console.log(`  ok      ${name}`);
  }
}

const bytes = await readFile(wasm);
const cove = await instantiate(bytes);
console.log(`loaded ${wasm} (${(bytes.length / 1e6).toFixed(2)} MB)\n`);

// ---- the samples the picker offers, and the directory they live in -----
//
// `samples.mjs` is the manifest the page's `<select>` is built from and
// `samples/` is where the programs are. The two must name the same set, and
// the comparison below is a set comparison rather than a count, because a
// count cannot tell a renamed sample from a matched pair: one file unlisted
// and one entry unbacked is still ten and ten.
//
// This is the check that makes a sample worth keeping in a file at all. It
// runs here, in CI's `wasm` job, and again in the Pages workflow *before*
// anything is staged, so a sample that stopped compiling, or stopped printing
// what it says it prints, fails the build instead of greeting a visitor with
// an error on the live site.

const directory = new URL("./samples/", import.meta.url);
const onDisk = (await readdir(directory))
  .filter((name) => name.endsWith(".cove"))
  .sort();
const listed = SAMPLES.map((sample) => sample.file).sort();

console.log("the manifest and the sample directory:");
check(
  "every file in `samples/` is listed in `samples.mjs`",
  onDisk.filter((name) => !listed.includes(name)),
  (extra) => extra.length === 0,
);
check(
  "every entry of `samples.mjs` is a file in `samples/`",
  listed.filter((name) => !onDisk.includes(name)),
  (missing) => missing.length === 0,
);
check("more than a token few", SAMPLES.length >= 8, true);
check(
  "each entry has a label, a one-line description and an expectation",
  SAMPLES.filter(
    (sample) =>
      !sample.label ||
      !sample.blurb ||
      !sample.expect ||
      typeof sample.expect.stdout !== "string" ||
      !sample.expect.answer,
  ).map((sample) => sample.file),
  (bare) => bare.length === 0,
);
check(
  "the labels are distinct, so a picker can be read",
  new Set(SAMPLES.map((sample) => sample.label)).size,
  SAMPLES.length,
);

// ---- what the editor is coloured by -----------------------------------
//
// `cove_lex` answers a *tiling* of the source: spans that between them cover
// every UTF-16 code unit of it exactly once, in order, each named with what
// the lexer decided it was. The page paints its editor by walking that list
// and slicing its own text, so a tiling that did not tile would put a colour
// on the wrong glyphs, and the check below is the one that would catch it.

const KINDS = ["keyword", "type", "string", "number", "slot", "comment", "plain"];

/// "" when `spans` tile `source`, and a description of the first violation
/// otherwise -- where it broke is the interesting half.
function tiling(source, spans) {
  let at = 0;
  for (const span of spans) {
    if (span.at !== at) return `a span begins at ${span.at}, after ${at}`;
    if (!(span.len > 0)) return `an empty span at ${span.at}`;
    if (!KINDS.includes(span.kind)) return `an unknown kind \`${span.kind}\``;
    at += span.len;
  }
  return at === source.length
    ? ""
    : `the spans cover ${at} of ${source.length} code units`;
}

/// A colouring as `[text, kind]` pairs, which is what one looks like.
///
/// `read` is the module entry point that answers the tiling: `cove.lex` for
/// source, `cove.lexIr` for a disassembly. The two answer the same shape on
/// purpose, so everything asserted of one can be asserted of the other.
function painted(source, read = (text) => cove.lex(text)) {
  const answer = read(source);
  return {
    ok: answer.ok,
    tiles: tiling(source, answer.spans),
    runs: answer.spans.map((span) => [
      source.slice(span.at, span.at + span.len),
      span.kind,
    ]),
  };
}

/// The colouring of a disassembly, whose `ok` means something stronger than
/// source's: not that the text parsed -- `cove_ir::print` wrote it and it
/// always does -- but that every line of it was a line shape that module
/// documents. A false answer is the printer having grown a line the colouring
/// does not know, which is the failure this whole section exists to catch.
const disassembled = (ir) => painted(ir, (text) => cove.lexIr(text));

// ---- every sample, compiled and run ------------------------------------
//
// The right outcome, no diagnostics at all -- a warning is a diagnostic, and
// a sample the compiler doubts is not a sample -- and what each one is
// supposed to print and answer. Asserting the output rather than the absence
// of a crash is the whole point: a sample that still runs and now prints
// something else has stopped teaching what it was written to teach.

console.log("\nevery sample the picker offers:");
const sources = new Map();
// Every category the ten samples' disassembly was cut into, gathered as the
// loop goes and asserted after it.
const kinds = new Set();
for (const sample of SAMPLES) {
  // An entry naming no file is already a failure above; reading it would be a
  // thrown `ENOENT` on top of it, which reports the same fact less clearly.
  if (!onDisk.includes(sample.file)) continue;
  const source = await readFile(new URL(sample.file, directory), "utf8");
  sources.set(sample.file, source);

  const built = cove.compile(source);
  check(`${sample.file} compiles`, built.ok, true);
  check(
    `${sample.file} has nothing to complain about`,
    built.diagnostics.map((d) => d.rendered).join("\n"),
    "",
  );

  // A sample a visitor opens and sees in one colour is a silent failure --
  // nothing about it looks broken -- so every one of them lexing, and being
  // covered end to end by what it lexed to, is asserted rather than assumed.
  const shown = painted(source);
  check(`${sample.file} lexes, so the editor can colour it`, shown.ok, true);
  check(`${sample.file} is coloured end to end`, shown.tiles, "");

  // And the other coloured pane, held to the same bar for the same reason.
  // This is the check that a change to `cove_ir::print` fails rather than
  // quietly turning the disassembly into a wall of one colour: `ok` is false
  // the moment one line of one shipped sample is a line the colouring does
  // not recognise, and these ten between them use most of the instruction
  // set. `kinds` collects what they were coloured as for the coverage
  // assertion below, which is the other half -- a tiling of the right length
  // made of the wrong categories is what a reader that had drifted would
  // produce, and `ok` alone would not notice it.
  const listing = disassembled(built.ir ?? "");
  check(`${sample.file} disassembles into lines the colouring knows`, listing.ok, true);
  check(`${sample.file}'s disassembly is coloured end to end`, listing.tiles, "");
  for (const [, kind] of listing.runs) kinds.add(kind);

  const outcome = cove.run(source);
  check(`${sample.file} runs inside this page's grants`, outcome.outcome, "success");
  check(`${sample.file} prints what it says it prints`, outcome.stdout, sample.expect.stdout);
  check(`${sample.file} answers what it says it answers`, outcome.answer, (held) =>
    isDeepStrictEqual(held, sample.expect.answer),
  );

  // The one sample whose point is the timeline. `records` says what scrubbing
  // it is supposed to show, so a rewrite that flattened the call chain or
  // stopped naming a heap object fails here rather than leaving a sample
  // whose own comment promises something it no longer does.
  if (sample.records) {
    const recorded = cove.debug(source);
    check(`${sample.file} records`, recorded.debug !== null, true);
    check(
      `${sample.file} is still worth stepping: ${sample.records.frames} frames deep`,
      Math.max(...recorded.debug.moments.map((m) => m.frames.length)),
      (deepest) => deepest >= sample.records.frames,
    );
    if (sample.records.objects) {
      check(
        `${sample.file} still has a local that names a heap object`,
        recorded.debug.moments.some((m) =>
          m.frames.some((frame) => frame.locals.some((local) => local.refs.length > 0)),
        ),
        true,
      );
    }
  }

  // The one sample that is a program from somewhere else. `sameAs` names the
  // file, from the repository root, and what ties the two together is that
  // both are *run*: the same answer, and the same instruction count.
  //
  // Comparing the text would fail on the comment, which is the one part that
  // is supposed to differ -- the benchmark's prose is about the benchmark
  // suite and the sample's is about this page. Comparing the two runs fails
  // on the part that is not supposed to differ: a turn count edited on one
  // side, a statement added to one loop, or a change to the lowering that
  // reached one file and not the other.
  //
  // The count is deliberately not pinned to a literal here. Any change to the
  // lowering moves it, and a golden that must be updated on every unrelated
  // change is a golden people learn to update without reading -- at which
  // point it stops catching the change that mattered. What is asserted
  // instead is the property the sample exists for: that the number is *large*
  // enough to be worth looking at. A rewrite that quietly shrank the loop
  // would still agree with its twin and would still fail here.
  // Moving or renaming the twin is the likeliest way for this to break, and a
  // named failure reports that better than an `ENOENT` stack trace does.
  const twin = sample.sameAs
    ? await readFile(new URL(`../${sample.sameAs}`, import.meta.url), "utf8").catch(
        () => null,
      )
    : null;
  if (sample.sameAs) {
    check(`\`${sample.sameAs}\` is where ${sample.file} says it is`, twin !== null, true);
  }
  if (twin !== null) {
    // Not a copy: two files holding the same text drift and nothing notices,
    // which is the failure the whole manifest is arranged to prevent. They
    // are the same *program* under different prose, and the runs below are
    // what says so.
    check(`${sample.file} is not a copy of \`${sample.sameAs}\``, twin === source, false);

    const also = cove.run(twin);
    check(`\`${sample.sameAs}\` runs on this page's limits too`, also.outcome, "success");
    check(
      `${sample.file} answers what \`${sample.sameAs}\` answers`,
      outcome.answer,
      (held) => isDeepStrictEqual(held, also.answer),
    );
    check(
      `${sample.file} costs what \`${sample.sameAs}\` costs`,
      outcome.instructions,
      also.instructions,
    );
    check(
      `and it is a real workload: ${outcome.instructions} instructions, ${outcome.fuel} fuel`,
      outcome.instructions > 1_000_000,
      true,
    );
  }
}

// ---- a program that compiles, runs, and prints -------------------------
//
// The first sample again, for what the samples loop does not ask: that a
// compile and a run answer the same disassembly, that it names the entry, and
// that the two counters move. It is the first sample and not a tenth program
// written here so that the disassembly printed below -- the one thing this
// check shows rather than asserts -- is a program a visitor can actually
// open.

const hello = sources.get(SAMPLES[0].file);

console.log("\na program that compiles and runs:");
const compiled = cove.compile(hello);
check("compile ok", compiled.ok, true);
check("no diagnostics", compiled.diagnostics.length, 0);
check("has a disassembly", typeof compiled.ir === "string" && compiled.ir.length > 0, true);
check("names the entry", compiled.ir.includes("playground.main"), true);

const ran = cove.run(hello);
check("outcome", ran.outcome, "success");
check("printed", ran.stdout, SAMPLES[0].expect.stdout);
check("answered", ran.answer, (held) =>
  isDeepStrictEqual(held, SAMPLES[0].expect.answer),
);
check("counted instructions", ran.instructions > 0, true);
check("counted fuel", ran.fuel > 0, true);
check("answered the disassembly too", ran.ir, compiled.ir);
console.log(`\n  --- the disassembly the page shows ---\n${compiled.ir.trimEnd()}\n  ---\n`);

// ---- the colours themselves --------------------------------------------
//
// The kinds and not only the count: a tiling of the right length made of the
// wrong categories is exactly what a highlighter that had drifted from the
// language would produce, and it is what the count would not notice.

console.log("what a program is coloured as:");
const greeting = `// a comment
export fn greet(name: String) -> String {
  let times = 2
  "Hello, {name}!"
}
`;
const painting = painted(greeting);
check("it lexes", painting.ok, true);
check("it tiles the source", painting.tiles, "");
check(
  "each piece is what the lexer called it",
  painting.runs.filter(([, kind]) => kind !== "plain"),
  (held) =>
    isDeepStrictEqual(held, [
      ["// a comment", "comment"],
      ["export", "keyword"],
      ["fn", "keyword"],
      ["String", "type"],
      ["String", "type"],
      ["let", "keyword"],
      ["2", "number"],
      ['"Hello, {name}!"', "string"],
    ]),
);

// Offsets are in UTF-16 code units because that is what a JavaScript string
// is indexed in, and two of the shipped samples hold an em dash. Counted in
// bytes, every colour after the first one would land two characters late.
const dashed = painted("// an \u2014 dash\nlet n = 1\n");
check("offsets are counted the way `String.slice` counts", dashed.runs[0], (held) =>
  isDeepStrictEqual(held, ["// an \u2014 dash", "comment"]),
);

// The state the editor is in for as long as it takes to type a string
// literal, which is why this case is not an edge one. It answers `ok: false`
// and a tiling of what it was sent; a page repaints from it rather than
// keeping an older colouring of text that has since been typed over.
console.log("\nsource that does not lex, which is what typing a string looks like:");
const half = painted('let n = 1\nlet greeting = "open');
check("it says so rather than throwing", half.ok, false);
check("and still tiles the whole of it", half.tiles, "");
check("what came before the quote keeps its colours", half.runs[0], (held) =>
  isDeepStrictEqual(held, ["let", "keyword"]),
);
check("and the open literal is a string to the end", half.runs.at(-1), (held) =>
  isDeepStrictEqual(held, ['"open', "string"]),
);

// ---- what a disassembly is coloured as ---------------------------------
//
// The other text this page shows. It has no lexer to borrow, so what colours
// it reads the line shapes `crates/cove-ir/src/print.rs` documents -- and a
// reader of a format is exactly the thing that drifts from the format, which
// is why the ten samples above are all put through it and why the pieces
// below are named rather than counted.
//
// `crates/cove-wasm/src/highlight.rs` argues why that reader is in Rust
// beside the printer and not a regular expression in `index.html`.

console.log("\nwhat a disassembly is coloured as:");
check(
  "every category the disassembly distinguishes is actually used by a sample",
  [...kinds].sort().join(" "),
  "keyword number plain slot string type",
);

const small = cove.compile(`export fn main() -> Int {
  let n = 21
  n * 2
}
`);
const lit = disassembled(small.ir);
check("it knows every line of it", lit.ok, true);
check("it tiles the disassembly", lit.tiles, "");
check(
  "each piece is what the printer wrote it as",
  lit.runs.filter(([, kind]) => kind !== "plain"),
  (held) =>
    isDeepStrictEqual(held, [
      // The header: an id, then layouts. The function's own name is a name
      // and is left plain, which is what tells it from the layouts around it.
      ["fn0", "number"],
      ["Int", "type"],
      // The frame, and a slot with the `Repr` that says what that one word
      // holds. A slot and its annotation are one piece: `s1` alone would not
      // say whether the instruction moved a word or a `Point`.
      ["frame", "keyword"],
      ["4", "number"],
      ["s0:int", "slot"],
      ["s1:int", "slot"],
      ["s2:int", "slot"],
      ["s3:int", "slot"],
      // The name a `local` binds is the source's own and is neither a layout
      // nor a callee; the range is the program counters it holds the slot
      // over, and program counters are numbers like any other.
      ["local", "keyword"],
      ["s1:Int", "slot"],
      ["1", "number"],
      ["4", "number"],
      // Then the code: a pc, an opcode, and operands.
      ["0", "number"],
      ["int", "keyword"],
      ["s1:int", "slot"],
      ["21", "number"],
      ["1", "number"],
      ["int", "keyword"],
      ["s2:int", "slot"],
      ["2", "number"],
      ["2", "number"],
      ["mul.int", "keyword"],
      ["s3:int", "slot"],
      ["s1:int", "slot"],
      ["s2:int", "slot"],
      ["3", "number"],
      ["copy", "keyword"],
      ["s0:int", "slot"],
      ["s3:int", "slot"],
      ["Int", "type"],
      ["4", "number"],
      ["return", "keyword"],
      ["s0:int", "slot"],
    ]),
);

// A string literal is one piece, spaces and all, and a callee is a name where
// the layout beside it is a type. Both are things a scanner that split on
// whitespace would get wrong, and both are in every program that prints.
const printing = disassembled(
  cove.compile(`use console.println

export fn main() -> Result<Unit, Error> {
  println("two words")?
  Ok(())
}
`).ir,
);
check("it knows every line of that one too", printing.ok, true);
check("a string literal is one piece, spaces and all", printing.runs, (held) =>
  held.some(([text, kind]) => text === '"two words"' && kind === "string"),
);
// Not `=== "console.println"`: a plain run is merged with the punctuation
// around it, which is the point of merging -- one DOM node per visible run
// rather than one per token. What is asserted is that the callee is inside a
// plain one and not a piece of its own coloured as a layout.
check(
  "a callee is a name and not a layout",
  printing.runs.filter(([text]) => text.includes("console.println")),
  (held) => held.length === 1 && held[0][1] === "plain",
);
check("and the layout beside it still is one", printing.runs, (held) =>
  held.some(([text, kind]) => text === "String" && kind === "type"),
);

// The signal itself, which is what makes all of the above a check rather than
// a description: a line the printer never wrote is left plain and reported.
const strange = disassembled("fn0 playground.main() -> Int\n  a new kind of line\n");
check("a line it does not know turns the answer false", strange.ok, false);
check("and is still tiled rather than dropped", strange.tiles, "");

// ---- a program that does not compile ----------------------------------

console.log("\na program that does not parse:");
const broken = cove.compile("export fn main() -> Int { 1 +");
check("compile refused", broken.ok, false);
check("no disassembly", broken.ir, null);
check("one error", broken.diagnostics[0].severity, "error");
console.log(`\n${broken.diagnostics[0].rendered.trimEnd()}\n`);

// ---- a program that spawns --------------------------------------------
//
// The one case that can only be checked here. On any target with threads a
// task gets one and this succeeds; in wasm there is no thread to give, and
// the runtime says so with a span instead of trapping.

console.log("a program that spawns a task:");
const spawning = `export fn main() -> Int {
  scope tasks {
    let one = tasks.spawn { 21 }
    one.await() * 2
  }
}
`;
const spawned = cove.run(spawning);
check("refused", spawned.ok, false);
check("classified as a concurrency stop", spawned.outcome, "concurrency");
check("not a trap", spawned.diagnostics.length, 1);
check(
  "says why",
  spawned.diagnostics[0].message,
  (held) => held.includes("no threads"),
);
console.log(`\n${spawned.diagnostics[0].rendered.trimEnd()}\n`);

// ---- the two bounds ----------------------------------------------------

console.log("a run past its fuel:");
const looping = `export fn main() -> Int {
  var n = 0
  while true { n = n + 1 }
  n
}
`;
const burnt = cove.run(looping, { fuel: 50_000 });
check("stopped", burnt.outcome, "fuel");
check("names the limit", burnt.diagnostics[0].message, (held) => held.includes("50000"));

// The deadline is the check that the imported clock is load-bearing. With a
// clock that never advanced this case would run until its fuel ran out and
// report `fuel`, so `deadline` here is the evidence that `performance.now()`
// is being read and compared.
console.log("\na run past its deadline:");
const started = performance.now();
const late = cove.run(looping, { fuel: 4_000_000_000, deadlineMs: 50 });
const took = performance.now() - started;
check("stopped", late.outcome, "deadline");
check("stopped near the deadline", took, (held) => held >= 40 && held < 5_000);
console.log(`          (took ${took.toFixed(0)} ms for a 50 ms deadline)`);

// ---- a capability the playground does not grant ------------------------

console.log("\na capability the playground does not grant:");
const fetching = `use http

export fn main() -> Result<http.Response, Error> {
  http.fetch("http://example.com")
}
`;
const refused = cove.run(fetching);
check("refused at the boundary", refused.outcome, "host_boundary");
check("checked, not unknown", refused.diagnostics[0].code, "cove::runtime");

// ---- a recorded run ----------------------------------------------------
//
// The other half of the playground: `cove_debug` runs the program under a
// recording debugger and answers a timeline the page scrubs through. What is
// checked here is that the recording of a *known* program holds the moments
// it should, in the order it ran them, with the locals holding what they
// held at each one -- which is the only way to find out that a moment has
// drifted from the instruction it claims to be at.
//
// `crates/cove-wasm/src/record.rs` is where the capture rule and its three
// bounds are argued.

const walked = `export fn twice(n: Int) -> Int {
  n + n
}

export fn main() -> Int {
  let one = 21
  let total = twice(one)
  total
}
`;

console.log("\na recorded run:");
const recorded = cove.debug(walked);
check("ran", recorded.outcome, "success");
check("answered", JSON.stringify(recorded.answer), (held) => held.includes('"value":42'));
check("has a recording", recorded.debug !== null, true);

const { moments, functions } = recorded.debug;
check("not truncated", recorded.debug.truncated, null);
check(
  "the moments the program ran, in order",
  moments.map((m) => m.why).join(" "),
  "entry line call line return line",
);
check(
  "each moment names the line it was written at",
  moments.map((m) => m.line).join(" "),
  "6 7 2 1 8 5",
);
check(
  "the instruction counts only go forwards",
  moments.every((m, at) => at === 0 || m.at > moments[at - 1].at),
  true,
);

// The span each moment was written at, which is what the page marks in the
// editor rather than in a second copy of the text. It is a pair of UTF-16
// offsets into the source the page already holds, so the page slices its own
// string by them; `line` says which line, and these say where on it.
const online = (text, at) => text.slice(0, at).split("\n").length;
check(
  "every moment carries a span the editor can slice",
  moments.every((m) => m.from < m.to && m.to <= walked.length),
  true,
);
check(
  "and it is on the line the moment names",
  moments.every((m) => online(walked, m.from) === m.line),
  true,
);
check(
  "every frame carries its own, so selecting one moves the mark",
  moments.every((m) =>
    m.frames.every((f) => f.from < f.to && online(walked, f.from) === f.line),
  ),
  true,
);

// Counted the way `String.prototype.slice` counts, which is the same bug the
// disassembly capture had and the reason that one is remembered: on `wasm32`
// a `usize` is 32 bits and nothing about a byte offset looks wrong until the
// source holds a character that is not one byte wide.
const dashes = `// an — dash
export fn main() -> Int {
  21 * 2
}
`;
const marked = cove.debug(dashes).debug.moments[0];
check(
  "an offset counts code units and not bytes",
  dashes.slice(marked.from, marked.to),
  "21",
);

// Both functions, disassembled once each however many moments are in them.
// This is where the format's size comes from: a loop of a thousand moments
// carries its function's instructions once, not a thousand times.
check("both functions interned", functions.length, 2);
check("named", functions.map((f) => f.name).join(" "), "playground.main playground.twice");
check(
  "the disassembly is the one `cove ir` prints",
  functions[1].code.map((line) => line.text).join(" | "),
  "add.int s2:int s0:int s0:int | copy s1:int s2:int Int | return s1:int",
);
check(
  "every moment's pc is inside its function",
  moments.every((m) => m.pc < functions[m.function].code.length),
  true,
);

// The locals, at the moments they were those values. `total` is declared
// before the call it is assigned from returns, so the moment inside `twice`
// shows it holding zero and the moment after the return shows 42. That is
// the stepping rule's stated limitation, not a defect: a moment is at the
// first instruction carrying a new line, which is inside the expression.
const inside = moments.find((m) => m.why === "call");
const after = moments.find((m) => m.why === "return");
const local = (moment, frame, name) =>
  moment.frames[frame].locals.find((held) => held.name === name)?.value;
check("the callee's parameter", local(inside, 0, "n"), "21");
check("the caller is still on the stack", inside.frames.length, 2);
check("its `total` is not assigned yet", local(inside, 1, "total"), "0");
check("and is, once the call returned", local(after, 0, "total"), "42");
check("the depth the moment records", inside.depth, 2);

// A local that names a heap object, and the object it names. This is what
// the Memory pane shows, and the address on the local is the address on the
// object, which is what lets the pane follow the name.
console.log("\na local that names a heap object:");
const strung = cove.debug(`export fn main() -> Int {
  let greeting = "hello"
  greeting.length()
}
`);
const held = strung.debug.moments.find((m) =>
  m.frames.some((frame) => frame.locals.some((local) => local.refs.length > 0)),
);
check("a local holds a reference", held !== undefined, true);
const reference = held.frames[0].locals.find((local) => local.refs.length > 0);
check("named `greeting`", reference.name, "greeting");
check("rendered", reference.value, "hello");
const object = held.objects.find((o) => o.at === reference.refs[0]);
check("the object is in the moment", object !== undefined, true);
check("a String", object.name, "String");
check("holding what it holds", JSON.stringify(object.fields), '[{"name":"text","value":"hello"}]');

// ---- a recording that hit its bound ------------------------------------
//
// The half that matters most: a recording is refused or truncated rather
// than allowed to grow without limit, it *says* it was truncated, and the
// run it was recording still reaches its own end and answers.

console.log("\na recording past its bound:");
const counting = `export fn main() -> Int {
  var n = 0
  while n < 1000 {
    n = n + 1
  }
  n
}
`;
const clipped = cove.debug(counting, { moments: 8 });
check("truncated", clipped.debug.truncated, "moments");
check("kept exactly its limit", clipped.debug.kept, 8);
check("says what the limit was", clipped.debug.limit, 8);
check("the run went on to its own end", clipped.outcome, "success");
check("and answered", JSON.stringify(clipped.answer), (answered) =>
  answered.includes('"value":1000'),
);

const asked = cove.debug(counting, { moments: 4_000_000_000 });
check("a caller cannot ask for an unbounded recording", asked.debug.limit, 16_384);

console.log("\na program that does not compile:");
const unrecorded = cove.debug("export fn main() -> Int { 1 +");
check("no recording", unrecorded.debug, null);
check("the diagnostics are still there", unrecorded.diagnostics[0].severity, "error");

// ---- what a recording costs -------------------------------------------
//
// Printed rather than asserted. A number that a change to the lowering could
// move by a hundred bytes is not a thing to fail a build over, but it is
// exactly what a person weighing "should the page ask for a recording"
// wants to see.

const sized = cove.debug(hello);
const whole = JSON.stringify(sized.debug).length;
console.log("\nwhat a recording costs:");
console.log(
  `          ${sized.debug.kept} moments of the greeting program: ` +
    `${sized.debug.bytes} B of moments, ${whole} B of recording ` +
    `(${(whole / sized.debug.kept).toFixed(0)} B per moment)`,
);
const big = cove.debug(counting);
console.log(
  `          ${big.debug.kept} moments of a thousand-turn loop: ` +
    `${big.debug.bytes} B of moments, ${JSON.stringify(big.debug).length} B of recording ` +
    `(truncated: ${big.debug.truncated})`,
);

console.log(failures === 0 ? "\nall checks passed" : `\n${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
