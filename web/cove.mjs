// The JavaScript half of `crates/cove-wasm`'s C ABI.
//
// One module, no dependencies, no build step. It is loaded by three callers
// that fetch the bytes three different ways -- the worker, the node check,
// and anything else that embeds the playground -- which is why `instantiate`
// takes the bytes rather than a URL.
//
// The ABI it speaks is documented in `crates/cove-wasm/src/abi.rs`. In short:
// `cove_alloc`/`cove_free` for buffers, and an answer is a little-endian
// `u32` length followed by that many bytes of UTF-8 JSON.

/// The one import the module needs: a monotonic clock in milliseconds.
///
/// Without it the module does not instantiate, deliberately. `cove-runtime`'s
/// `wallclock` module is where that choice is argued: a run's deadline is
/// enforced against this function, and a default that never advanced would
/// have given every run a deadline that silently never fires.
function imports() {
  return {
    cove: {
      cove_now_millis: () =>
        typeof performance === "object" ? performance.now() : Date.now(),
    },
  };
}

/// Instantiates the module from `bytes` and answers something to call.
///
/// Bytes and not a `Response`: `WebAssembly.instantiateStreaming` refuses
/// anything not served as `application/wasm`, and `python3 -m http.server`
/// does not know that type. A playground that would not load behind the
/// server its own README recommends is not worth the one copy this costs.
export async function instantiate(bytes) {
  const { instance } = await WebAssembly.instantiate(bytes, imports());
  return new Cove(instance);
}

/// One instantiated playground.
///
/// Not reusable across a hung run: there is no way to interrupt wasm from
/// outside it, so stopping a run means discarding the worker that holds one
/// of these. `index.html` does exactly that.
class Cove {
  #exports;

  constructor(instance) {
    this.#exports = instance.exports;
  }

  /// Checks and lowers `source`.
  ///
  /// Answers `{ok, diagnostics, ir}`.
  compile(source) {
    return this.#call((ptr, len) => this.#exports.cove_compile(ptr, len), source);
  }

  /// Checks, lowers and runs `source`.
  ///
  /// Answers `{ok, diagnostics, outcome, stdout, stderr, answer, instructions,
  /// fuel}`. `fuel` and `deadlineMs` of zero mean the module's own defaults,
  /// which are bounds and not the absence of them.
  run(source, { fuel = 0, deadlineMs = 0 } = {}) {
    return this.#call(
      (ptr, len) => this.#exports.cove_run(ptr, len, fuel, deadlineMs),
      source,
    );
  }

  /// Lexes `source` and answers a colour for every part of it.
  ///
  /// Answers `{ok, spans}`, where each span is `{at, len, kind}` and the
  /// spans *tile*: the first begins at zero, each begins where the last
  /// ended, and together they cover the source. `at` and `len` count UTF-16
  /// code units, so `source.slice(at, at + len)` is the text a span is for.
  ///
  /// `kind` is one of `keyword`, `type`, `string`, `number`, `comment` and
  /// `plain`. `crates/cove-wasm/src/highlight.rs` argues why a page colours
  /// by asking the compiler's own lexer rather than by a keyword list of its
  /// own, and says what each category holds.
  ///
  /// `ok` is whether the source lexed without complaint -- false for as long
  /// as a string literal is open, which is most of the time one is being
  /// typed. The spans are a tiling either way, so a caller repaints from
  /// them rather than holding on to an older colouring of text that is no
  /// longer in the box.
  ///
  /// This is the front end's first stage and nothing after it, which is what
  /// makes it cheap enough to call on every keystroke, and what makes it safe
  /// to call on the page's own thread: the reason `run` is on a worker is
  /// that a Cove program can loop, and lexing is one pass over the text.
  lex(source) {
    return this.#call((ptr, len) => this.#exports.cove_lex(ptr, len), source);
  }

  /// Colours a disassembly and answers a tiling of it.
  ///
  /// Answers `{ok, spans}` in exactly `lex`'s shape and counted in exactly
  /// `lex`'s units, so one renderer draws both. `kind` adds `slot` -- a slot
  /// and its `Repr`, `s3:int` -- to `lex`'s six.
  ///
  /// `text` is what a `compile` or a `run` put in `ir`, handed back so that
  /// the colouring is of the text being shown. There is no lexer for a
  /// disassembly, so the module reads the line shapes `cove_ir::print`
  /// documents, and `ok` is whether every line was one of them: false means
  /// the printer has grown a line the colouring does not know, and the pane
  /// is showing plain text where that line is rather than a guess.
  /// `web/check.mjs` fails the build on a false answer for any shipped
  /// sample, which is what keeps the two in step.
  lexIr(text) {
    return this.#call((ptr, len) => this.#exports.cove_lex_ir(ptr, len), text);
  }

  /// Checks, lowers and runs `source` under a recording debugger.
  ///
  /// Answers everything `run` answers plus `debug`, which is the recording:
  /// `{moments, functions, kept, limit, bytes, truncated, frames, objects,
  /// policy}`, or `null` for a source that did not compile.
  ///
  /// `moments` of zero means the module's own default. It is a bound and not
  /// the absence of one, and it is clamped: `crates/cove-wasm/src/record.rs`
  /// says what a moment is, what it costs, and what a truncated recording
  /// loses.
  ///
  /// A debugged run is slower than a run -- the machine asks the recorder
  /// before every instruction -- so a program that finished inside the
  /// default deadline may not finish inside it here. It is reported as a
  /// `deadline` outcome beside the recording of everything up to it.
  debug(source, { fuel = 0, deadlineMs = 0, moments = 0 } = {}) {
    return this.#call(
      (ptr, len) => this.#exports.cove_debug(ptr, len, fuel, deadlineMs, moments),
      source,
    );
  }

  /// Sends `source` in, calls `entry`, and reads the answer back out.
  #call(entry, source) {
    const bytes = new TextEncoder().encode(source);
    const ptr = this.#exports.cove_alloc(bytes.length);
    // The view is taken after every call that can allocate, and never held
    // across one: a growing `WebAssembly.Memory` detaches every view over its
    // old buffer, and a view read afterwards is empty rather than wrong,
    // which is the kind of bug that shows up as a blank answer.
    new Uint8Array(this.#exports.memory.buffer, ptr, bytes.length).set(bytes);
    try {
      return this.#answer(entry(ptr, bytes.length));
    } finally {
      this.#exports.cove_free(ptr, bytes.length);
    }
  }

  /// Decodes a length-prefixed answer block and releases it.
  #answer(ptr) {
    const length = new DataView(this.#exports.memory.buffer).getUint32(ptr, true);
    // `slice` and not `subarray`: the bytes are copied out before the block
    // is freed, and before anything else can grow the memory under them.
    const payload = new Uint8Array(this.#exports.memory.buffer, ptr + 4, length).slice();
    this.#exports.cove_free(ptr, length + 4);
    return JSON.parse(new TextDecoder().decode(payload));
  }
}

/// Where a built module is, relative to a page in `web/`.
///
/// Two candidates, tried in order: a copy sitting beside the page, and the
/// artifact `cargo build` writes. The second is what makes the playground
/// need no step after the build, and it is why `web/README.md` says to serve
/// the repository root rather than `web/`.
export const WASM_URLS = [
  "./cove_wasm.wasm",
  "../target/wasm32-unknown-unknown/release/cove_wasm.wasm",
];

/// Fetches the first of `urls` that answers, and instantiates it.
export async function load(urls = WASM_URLS) {
  const refused = [];
  for (const url of urls) {
    try {
      const response = await fetch(url);
      if (!response.ok) {
        refused.push(`${url}: ${response.status}`);
        continue;
      }
      return await instantiate(await response.arrayBuffer());
    } catch (error) {
      refused.push(`${url}: ${error.message}`);
    }
  }
  throw new Error(
    `no Cove module found. Build it with\n` +
      `  cargo build -p cove-wasm --target wasm32-unknown-unknown --release\n` +
      `and serve the repository root. Tried:\n  ${refused.join("\n  ")}`,
  );
}
