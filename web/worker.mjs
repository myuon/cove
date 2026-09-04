// The thread a Cove program runs on.
//
// Execution is here and not on the page's thread for one reason: a Cove
// program can loop, and wasm cannot be interrupted from outside itself. A run
// on the main thread would freeze the tab -- no repaint, no Stop button, no
// way back. Here the worst it can do is occupy a worker the page can throw
// away, which is what the page's Stop button does.
//
// The module is instantiated once, on the first message, and kept: loading
// three megabytes of wasm per keystroke is not a thing to do. A worker that
// is terminated loses it, and the page builds another.

import { load } from "./cove.mjs";

let cove = null;

self.onmessage = async ({ data }) => {
  const { seq, kind, source, fuel, deadlineMs } = data;
  try {
    if (cove === null) {
      cove = await load();
      self.postMessage({ seq, ready: true });
    }
    const answer =
      kind === "compile" ? cove.compile(source) : cove.run(source, { fuel, deadlineMs });
    self.postMessage({ seq, kind, answer });
  } catch (error) {
    // A trap reaches here as a `RuntimeError`. Nothing in the runtime is
    // supposed to trap -- a `spawn` is refused, a limit is reported, and a
    // deadline fires -- so this arm is a bug report and says so rather than
    // pretending the run had an outcome.
    self.postMessage({
      seq,
      kind,
      failed: `${error}`,
    });
  }
};
