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

import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { instantiate } from "./cove.mjs";

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

// ---- a program that compiles, runs, and prints -------------------------

const hello = `use console.println

/// Returns a greeting for \`name\`.
export fn greeting(name: String) -> String {
  "Hello, {name}!"
}

export fn main() -> Result<Int, Error> {
  println(greeting("browser"))?
  Ok(21 * 2)
}
`;

console.log("a program that compiles and runs:");
const compiled = cove.compile(hello);
check("compile ok", compiled.ok, true);
check("no diagnostics", compiled.diagnostics.length, 0);
check("has a disassembly", typeof compiled.ir === "string" && compiled.ir.length > 0, true);
check("names the entry", compiled.ir.includes("playground.main"), true);

const ran = cove.run(hello);
check("outcome", ran.outcome, "success");
check("printed", ran.stdout, "Hello, browser!\n");
check("answered", JSON.stringify(ran.answer), (held) => held.includes('"value":42'));
check("counted instructions", ran.instructions > 0, true);
check("counted fuel", ran.fuel > 0, true);
check("answered the disassembly too", ran.ir, compiled.ir);
console.log(`\n  --- the disassembly the page shows ---\n${compiled.ir.trimEnd()}\n  ---\n`);

// ---- a program that does not compile ----------------------------------

console.log("a program that does not parse:");
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
