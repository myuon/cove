// The playground's sample programs: one entry per file in `samples/`.
//
// The programs themselves are real `.cove` files and not strings in here, for
// three reasons. `cove fmt --check` at the repository root walks `web/` like
// everything else, so a sample is formatted by the project's own formatter and
// a drifting one fails CI's dogfooding step. A file that stops parsing is a
// file, so `git diff` shows what changed rather than an edited string literal.
// And `web/check.mjs` compiles and runs every one of them through the real
// wasm module, which is what keeps a sample that stopped working from
// greeting a visitor on the live site.
//
// # This list and that directory may not disagree
//
// `check.mjs` reads `samples/`, and compares what is there against what is
// here, as a *set*: a file nothing lists fails, and an entry naming no file
// fails. A count would not catch a rename. That check runs in CI's `wasm` job
// and again in the Pages workflow before anything is staged, so neither kind
// of disagreement can reach the deployed page.
//
// `expect` is here rather than in `check.mjs` for the same reason: it belongs
// beside the sample it describes, and `check.mjs` requires every entry to
// carry one, so a sample added without an expectation fails instead of being
// run and not looked at. The page ignores it.

/// What the module's value encoding answers for an entry that returned
/// `Ok(value)`. `crates/cove-wasm`'s `run_json` documents the encoding;
/// these four build the shapes these samples answer in, so that an
/// expectation below reads as the value and not as its JSON.
const ok = (value) => ({
  type: "enum",
  name: "Result",
  case: "Ok",
  payload: [value],
});
const int = (value) => ({ type: "int", value });
const text = (value) => ({ type: "string", value });
const unit = () => ({ type: "unit" });

/// The samples, in the order the picker offers them, which is ascending in
/// what a reader has to already understand.
export const SAMPLES = [
  {
    file: "01-hello.cove",
    label: "Hello",
    blurb: "Functions, bindings, string interpolation, and the ? operator.",
    expect: {
      stdout: "Hello, browser!\n",
      answer: ok(int(42)),
    },
  },
  {
    file: "02-structs.cove",
    label: "Structs and methods",
    blurb: "An impl block, a mutating receiver, and what a copy is.",
    expect: {
      stdout: "origin (0, 0)\nnorth  (0, 3)\nwalker (4, 0)\n",
      answer: ok(text("(4, 0)")),
    },
  },
  {
    file: "03-enums.cove",
    label: "Enums and matching",
    blurb: "Cases that carry values, and a match the compiler makes you finish.",
    expect: {
      stdout:
        "circle of radius 2 covers about 12\n" +
        "3x4 rectangle covers about 12\n" +
        "a point covers about 0\n",
      answer: ok(int(24)),
    },
  },
  {
    file: "04-errors.cove",
    label: "Result, Option and ?",
    blurb: "Failure written into the type: no null, no exception.",
    expect: {
      stdout:
        "21 -> 21C\n" +
        "hot -> refused: `hot` is not a number\n" +
        "5000 -> refused: 5000 is not a temperature\n" +
        "nothing there\n",
      answer: ok(int(21)),
    },
  },
  {
    file: "05-collections.cove",
    label: "Collections",
    blurb: "An immutable Array, a Vector handle, and the four walks.",
    expect: {
      stdout:
        "4 entries recorded\n" +
        "  run took 51ms\n" +
        "  check took 30ms\n" +
        "  parse took 12ms\n" +
        "  lower took 7ms\n" +
        "over 10ms: 3 of them, first parse\n" +
        "100ms in all\n",
      answer: ok(int(100)),
    },
  },
  {
    file: "06-traits.cove",
    label: "Traits and dispatch",
    blurb: "A trait with a default method, resolved statically and by dyn.",
    expect: {
      stdout:
        "Latest: booking 41 for 2 guest(s)\n" +
        "Report\n" +
        "- booking 41 for 2 guest(s)\n" +
        "  $ receipt for booking 41: 12500c\n",
      answer: ok(
        text(
          "Report\n- booking 41 for 2 guest(s)\n" +
            "  $ receipt for booking 41: 12500c",
        ),
      ),
    },
  },
  {
    file: "07-closures.cove",
    label: "Closures",
    blurb: "Functions as values, and the snapshot a capture takes.",
    expect: {
      stdout:
        "0 stepped by ten, three times: 30\n" +
        "and by one, three times: 3\n" +
        "the last of [1, 2, 3] doubled is 6\n",
      answer: ok(int(30)),
    },
  },
  {
    file: "08-hosts.cove",
    label: "A host call",
    blurb: "files and clock, two of the five capabilities this page grants.",
    expect: {
      stdout:
        "3 notes:\n" +
        "  first.txt\n" +
        "  second.txt\n" +
        "  third.txt\n" +
        "first.txt says: the filesystem starts empty\n" +
        "the clock says 250ms went by\n",
      answer: ok(int(3)),
    },
  },
  {
    file: "09-stepping.cove",
    label: "Made to be stepped",
    blurb: "Run & record this one: nested frames, a shadowed name, an object.",
    expect: {
      stdout: "round 1, depth 10\nround 2, depth 20\ncove weighs 28\n",
      answer: ok(int(28)),
    },
    // The one sample whose point is the timeline rather than the source, so
    // `check.mjs` records it and asserts that it is still worth scrubbing: a
    // backtrace this deep, and at least one moment holding a heap object. A
    // rewrite that flattened it would fail here rather than quietly becoming
    // a sample that teaches nothing the source does not.
    records: { frames: 3, objects: true },
  },
  {
    file: "10-arithmetic.cove",
    label: "Two million turns",
    blurb: "Nothing printed: the counters and the IR are what this one shows.",
    expect: {
      stdout: "",
      answer: ok(unit()),
    },
    // The one sample that is a program from somewhere else. It is
    // `benches/arith/main.cove`'s loop under a playground's comment, and the
    // two are held together by being *run* rather than by being compared as
    // text: `check.mjs` runs both through the module and requires the same
    // answer and the same instruction count. A turn count edited in one and
    // not the other fails there. `sameAs` names that file, from the
    // repository root.
    sameAs: "benches/arith/main.cove",
  },
];
