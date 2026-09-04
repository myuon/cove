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

// A debug run is recorded here too, and for the same reason the run is: the
// recorder is asked before every instruction, so a debugged run is slower
// than a run, and a page that did this on its own thread would be a page that
// stopped repainting for longer. What comes back is the whole recording in
// one message, which is the answer to the other half of the problem -- a
// worker cannot block waiting for the page to say "step", so it does not ask;
// `crates/cove-wasm/src/record.rs` argues that where a reader will look.

self.onmessage = async ({ data }) => {
  const { seq, kind, source, fuel, deadlineMs, moments } = data;
  try {
    if (cove === null) {
      cove = await load();
      self.postMessage({ seq, ready: true });
    }
    const answer =
      kind === "compile"
        ? cove.compile(source)
        : kind === "debug"
          ? cove.debug(source, { fuel, deadlineMs, moments })
          : cove.run(source, { fuel, deadlineMs });
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
