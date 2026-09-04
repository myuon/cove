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

console.log(failures === 0 ? "\nall checks passed" : `\n${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
