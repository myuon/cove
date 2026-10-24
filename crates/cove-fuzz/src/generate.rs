//! A generator of Cove programs that parse and check by construction.
//!
//! The `run` target mutates real programs, and most of its mutations stop in
//! the front end: a byte flipped in a source is far more often a syntax error
//! than a new program. This generator spends the fuzzer's bytes on choices
//! instead — which statement, which operand, which boundary value — and every
//! choice it can make yields a program that type-checks, so every input
//! reaches the VM and the native tier.
//!
//! # What it concentrates on
//!
//! Issue 681 names the places a runtime's memory safety is decided: lengths,
//! indices, slices, growth and size arithmetic, for arrays, strings and byte
//! buffers. So the leaves of an `Int` expression are drawn mostly from
//! [`EDGES`] — zero, ±1, the powers of two either side of a byte, a word and a
//! half-word, `i64::MIN` and `i64::MAX` — and from a collection's own
//! `length()` and `byteLength()` plus or minus one; and the calls they are fed
//! to are the ones that take an index, a range, a capacity or a byte:
//!
//! - `Array.get`, `slice`, `indexOf`, `contains`; `Vector.of`, `filled`,
//!   `push`, `pop`, `get`, `set`, `remove`, aliasing, `toArray` and `freeze`;
//! - `String.slice`, `sliceBytes`, `byteLength`, `byteAt`, `codePointAtByte`,
//!   `chars`, `split`, `toUtf8`, `fromUtf8`, `fromUtf8Lossy`, `fromCodePoint`,
//!   over literals that mix one- to four-byte UTF-8 and a combining mark;
//! - `StringBuilder.withCapacity` (zero, negative, huge), `append`,
//!   `appendSlice` with ranges that may be backwards, past the end or inside a
//!   character, `appendByte` outside `0..=255`, and `finish` over bytes that
//!   may not be UTF-8 — `std.stringbuilder` is the one program-visible face of
//!   the standard library's `ByteBuffer`;
//! - `+ - * / %`, `floorDiv`, `floorMod`, the bit operations and shifts with
//!   counts outside `0..=63`, `toStringRadix`, `toHex` and `toDecimal` with a
//!   bad radix or a huge width, `Int.parse` and `parseRadix`;
//! - bounded `while` and `for` loops with `break`, helper functions that call
//!   earlier helpers, recursion whose depth the program computes, and closures
//!   passed to `map`, `filter`, `fold` and `sorted`.
//!
//! And, for the differential harness, the places two evaluators most easily
//! disagree:
//!
//! - **`Float`**: leaves from [`FLOAT_EDGES`] — both zeros, the subnormal and
//!   normal minima, the maximum, `2^53` and its neighbour, `±2^63`, NaN and
//!   both infinities — and arbitrary bit patterns; `+ - * / %`, negation,
//!   `abs`, `sqrt`, `round`, `min` and `max` (with NaN on either side),
//!   `exp`, `ln`, `log10`, `sin`, `cos`, `pow`; every comparison; `Int.toFloat`
//!   and `Float.toInt` out of range; `Float.parse` over [`FLOAT_TEXTS`] and
//!   over a value's own `format(17)`; `format` with a digit count in and out of
//!   `0..=17`; and a `Float` interpolated into a string, which is
//!   shortest-round-trip text;
//! - **types in combination**: `Option<Int>` and `Result<Int, Error>` built,
//!   matched, unwrapped and propagated with `?`; a struct `Pt` with an `Int`, a
//!   `Float` and a `String` field, compared with `==`, copied and then mutated
//!   through the copy; an enum `Shape` with a unit case and two payload cases,
//!   matched and compared;
//! - **closures**: `fn(Int) -> Int` values bound to names, captured by other
//!   closures, passed to helpers (`apply`, `twice`), returned from them
//!   (`adder`, `compose`), and handed to `map`;
//! - **errors**: helpers that answer `Err` or `None` on a condition, `?` in
//!   helpers and in `main` (so `main` itself answers `Err`), and every guest
//!   trap above;
//! - **side effects**: `println` and `eprintln` — the two console streams are
//!   compared separately — and, in the differential harness only, the virtual
//!   clock's `now` and `sleep`;
//! - **the collector**: a loop that builds and drops strings in a vector, so
//!   that the small-heap modes collect while the program's values are live.
//!
//! Most of those can stop the run — an overflow, a division by zero, a range
//! `appendSlice` refuses, a heap that cannot hold `Vector.filled(2^40, 0)`.
//! That is the point and is not a finding; it is why every hazard is drawn
//! only *some* of the time, so that a program also gets far enough to run the
//! rest of what it says.
//!
//! Every helper is a function of its own and `main` calls it, because the
//! outermost frame always runs on the encoded tier: an operation only `main`
//! performs is never compiled by the native tier.
//!
//! # What it does not generate, and why
//!
//! - **`Map`, `Set`, `Json`, `DynamicView`, the `std.*` library modules beyond
//!   `stringbuilder`** — each is a body of standard-library Cove that both
//!   evaluators run alike; the `run` and `differential_source` targets reach
//!   them through the corpus programs.
//! - **Traits, `impl` blocks, generics, `dyn`, opaque structs, `var`
//!   parameters, defaults and variadics** — each needs a declaration whose
//!   well-typedness is non-local, and none has a run-time representation the
//!   types above do not already exercise.
//! - **Tasks, `scope`, `spawn`, `async`, `Shared`** — a fuzzed run is granted
//!   no task (`max_tasks: 0`), and the order two tasks reach a host is the
//!   scheduler's; issue 681's priority 3 is where tasks are tested.
//! - **Hosts other than `console` and the virtual clock** — no other host is
//!   registered, so they would only ever be refused.
//! - **A `sorted` comparator that contradicts itself**, in the differential
//!   harness: the language promises some permutation, not which one, so two
//!   correct evaluators may answer differently. The `structured` target still
//!   generates it, where only a crash is a finding.
//! - **`is`, a closure compared with `==`**: identity, which a copy changes.
//! - **Unbounded loops and recursion** — every loop has a bound the program
//!   computes, and recursion takes a depth; an infinite loop would reach only
//!   the deadline, which the harness already treats as a stop.
//!
//! # Honesty
//!
//! [`crate::structured`] and [`crate::differential`](fn@crate::differential)
//! panic if a generated program does not check. That keeps this module from
//! drifting into generating programs the front end rejects — which would
//! still "pass", while testing nothing past the parser.

use std::fmt::Write as _;

use arbitrary::Unstructured;

/// The program `data` generates.
pub fn from_bytes(data: &[u8]) -> String {
    generate(&mut Unstructured::new(data))
}

/// The program `data` generates for the differential harness: the same
/// generator, with the virtual clock, and without a self-contradicting
/// `sorted` comparator (see the module documentation).
pub fn differential_from_bytes(data: &[u8]) -> String {
    let mut u = Unstructured::new(data);
    let mut g = Gen::new(&mut u);
    g.differential = true;
    g.program()
}

/// A whole program — helpers, then `export fn main() -> Result<Unit, Error>` —
/// generated from the choices `u` supplies. An exhausted `u` answers its
/// smallest choice every time, so every input ends in a finite program.
pub fn generate(u: &mut Unstructured<'_>) -> String {
    Gen::new(u).program()
}

/// `Int` values at which a bound, a size computation or an overflow check
/// has an edge.
pub const EDGES: [i64; 30] = [
    0,
    1,
    -1,
    2,
    3,
    -2,
    7,
    8,
    15,
    16,
    63,
    64,
    127,
    128,
    255,
    256,
    -128,
    -129,
    1023,
    65535,
    65536,
    i32::MAX as i64,
    i32::MAX as i64 + 1,
    u32::MAX as i64,
    1 << 32,
    1 << 40,
    1 << 53,
    i64::MAX - 1,
    i64::MAX,
    i64::MIN,
];

/// `Float` values at which rounding, conversion, comparison or printing has
/// an edge.
pub const FLOAT_EDGES: [f64; 32] = [
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.5,
    -0.5,
    1.5,
    2.5,
    -2.5,
    0.1,
    0.3,
    3.0,
    123456.789,
    f64::MAX,
    f64::MIN,
    f64::MIN_POSITIVE,
    5e-324,
    -5e-324,
    2.225073858507201e-308,
    9007199254740992.0,
    9007199254740994.0,
    4503599627370495.5,
    9.223_372_036_854_776e18,
    -9.223_372_036_854_776e18,
    9.223372036854777e18,
    1e16,
    1e21,
    1e-7,
    f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    std::f64::consts::PI,
];

/// Texts `Float.parse` has an edge at: the spellings of the special values,
/// rounding boundaries, overflow and underflow, and malformed numbers.
pub const FLOAT_TEXTS: [&str; 36] = [
    "0",
    "-0",
    "+0",
    "0.0",
    "nan",
    "NaN",
    "-nan",
    "inf",
    "-inf",
    "+inf",
    "infinity",
    "Infinity",
    "1e400",
    "-1e400",
    "1e-400",
    "4.9e-324",
    "2.4703282292062327e-324",
    "2.4703282292062328e-324",
    "2.2250738585072011e-308",
    "1.7976931348623157e308",
    "1.7976931348623158e308",
    "1.7976931348623159e308",
    "9007199254740993",
    "0.1",
    ".5",
    "5.",
    "1_0",
    "0x10",
    " 1",
    "1 ",
    "+1.5",
    "1e",
    "e5",
    "--1",
    "1.5e+3",
    "123456789012345678901234567890",
];

/// Pieces of string literals: ASCII, two-, three- and four-byte characters,
/// a combining mark, a control character, and an escape.
const PIECES: [&str; 16] = [
    "a", "z", "0", " ", ",", "é", "ß", "Ω", "日", "本", "🙂", "\u{301}", "\\n", "\\t", "\\\\",
    "\\\"",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    Int,
    Bool,
    Str,
    /// `Array<Int>`.
    Arr,
    /// `Array<String>`.
    Strs,
    /// `Vector<Int>`.
    Vec,
    /// `StringBuilder`.
    Sb,
    Float,
    /// `Option<Int>`.
    Opt,
    /// `Result<Int, Error>`.
    Res,
    /// The struct `Pt { x: Int, f: Float, s: String }` every program declares.
    Pt,
    /// The enum `Shape` every program declares.
    Shape,
    /// `fn(Int) -> Int`.
    Fun,
}

impl Ty {
    fn name(self) -> &'static str {
        match self {
            Ty::Int => "Int",
            Ty::Bool => "Bool",
            Ty::Str => "String",
            Ty::Arr => "Array<Int>",
            Ty::Strs => "Array<String>",
            Ty::Vec => "Vector<Int>",
            Ty::Sb => "StringBuilder",
            Ty::Float => "Float",
            Ty::Opt => "Option<Int>",
            Ty::Res => "Result<Int, Error>",
            Ty::Pt => "Pt",
            Ty::Shape => "Shape",
            Ty::Fun => "fn(Int) -> Int",
        }
    }

    /// Whether `"{x}"` may interpolate a binding of this type.
    fn printable(self) -> bool {
        matches!(
            self,
            Ty::Int | Ty::Bool | Ty::Str | Ty::Arr | Ty::Strs | Ty::Vec | Ty::Float
        )
    }
}

#[derive(Clone, Debug)]
struct Local {
    name: String,
    ty: Ty,
    mutable: bool,
    /// A `while` counter: read freely, assigned only by its own loop.
    counter: bool,
    /// Visible inside a closure written here. Only immutable bindings of a
    /// value type are — `Int`, `Float`, `String`, `Pt`, a function — which
    /// keeps every closure task-safe and keeps vectors and builders out of
    /// anything that could hold them.
    capturable: bool,
    /// A vector or builder that nothing else holds, so `freeze` or `finish`
    /// may consume it.
    unique: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// `hN(a: Int, b: Int) -> Int`.
    Int,
    /// `rN(n: Int, a: Int) -> Int`, which recurses `n` times.
    Recursive,
    /// `gN(x: Float, y: Float) -> Float`.
    Float,
    /// `eN(a: Int, b: Int) -> Result<Int, Error>`.
    Res,
    /// `oN(a: Int, b: Int) -> Option<Int>`.
    Opt,
}

#[derive(Clone, Debug)]
struct Helper {
    name: String,
    kind: Kind,
}

/// The function a statement is being written into, which decides what `?`
/// may propagate and whether the console may be called.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctx {
    Main,
    Helper(Kind),
}

struct Gen<'a, 'b> {
    u: &'b mut Unstructured<'a>,
    next: usize,
    env: Vec<Local>,
    helpers: Vec<Helper>,
    /// Statements generated so far, which bounds the program's size.
    statements: usize,
    in_loop: bool,
    ctx: Ctx,
    /// Generating for the differential harness: the clock is granted, and no
    /// choice may make the answer depend on which correct evaluator ran it.
    differential: bool,
}

/// The most statements a program is given, across every function.
const MAX_STATEMENTS: usize = 60;
/// How deep an expression nests.
const MAX_DEPTH: usize = 4;

/// What every program declares before its helpers: the struct, the enum and
/// the higher-order helpers the expressions below name.
const PRELUDE: &str = "struct Pt {
  x: Int
  f: Float
  s: String
}

enum Shape {
  Dot
  Line(Int)
  Box(Int, String)
}

fn apply(f: fn(Int) -> Int, x: Int) -> Int {
  f(x)
}

fn twice(f: fn(Int) -> Int, x: Int) -> Int {
  f(f(x))
}

fn adder(n: Int) -> fn(Int) -> Int {
  fn(x) { x + n }
}

fn compose(f: fn(Int) -> Int, g: fn(Int) -> Int) -> fn(Int) -> Int {
  fn(x) { g(f(x)) }
}

";

impl<'a, 'b> Gen<'a, 'b> {
    fn new(u: &'b mut Unstructured<'a>) -> Self {
        Gen {
            u,
            next: 0,
            env: Vec::new(),
            helpers: Vec::new(),
            statements: 0,
            in_loop: false,
            ctx: Ctx::Main,
            differential: false,
        }
    }

    // ---- choices ----------------------------------------------------------

    /// A number in `0..n`, or `0` when the input is exhausted.
    fn pick(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        self.u.int_in_range(0..=n - 1).unwrap_or(0)
    }

    /// True with probability about `num / den`, false once exhausted.
    fn chance(&mut self, num: u8, den: u8) -> bool {
        self.u.ratio(num, den).unwrap_or(false)
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    fn locals(&self, ty: Ty) -> Vec<Local> {
        self.env.iter().filter(|l| l.ty == ty).cloned().collect()
    }

    fn local(&mut self, ty: Ty) -> Option<Local> {
        let found = self.locals(ty);
        if found.is_empty() {
            None
        } else {
            let i = self.pick(found.len());
            Some(found[i].clone())
        }
    }

    fn mutable_local(&mut self, ty: Ty) -> Option<Local> {
        let found: Vec<_> = self
            .locals(ty)
            .into_iter()
            .filter(|l| l.mutable && !l.counter)
            .collect();
        if found.is_empty() {
            None
        } else {
            let i = self.pick(found.len());
            Some(found[i].clone())
        }
    }

    fn bind(&mut self, name: &str, ty: Ty, mutable: bool) {
        self.env.push(Local {
            name: name.to_string(),
            ty,
            mutable,
            counter: false,
            capturable: !mutable && matches!(ty, Ty::Int | Ty::Str | Ty::Float | Ty::Pt | Ty::Fun),
            unique: false,
        });
    }

    fn helper_of(&mut self, kinds: &[Kind]) -> Option<Helper> {
        let found: Vec<_> = self
            .helpers
            .iter()
            .filter(|h| kinds.contains(&h.kind))
            .cloned()
            .collect();
        if found.is_empty() {
            None
        } else {
            let i = self.pick(found.len());
            Some(found[i].clone())
        }
    }

    // ---- program ----------------------------------------------------------

    fn program(&mut self) -> String {
        let mut out = String::from(
            "use console.println\nuse console.eprintln\nuse std.stringbuilder.StringBuilder\n",
        );
        if self.differential {
            out.push_str("use clock\n");
        }
        out.push('\n');
        out.push_str(PRELUDE);
        let helpers = self.pick(6);
        for _ in 0..helpers {
            let kind = match self.pick(8) {
                0 | 1 => Kind::Int,
                2 => Kind::Recursive,
                3 | 4 => Kind::Float,
                5 | 6 => Kind::Res,
                _ => Kind::Opt,
            };
            let text = self.helper(kind);
            out.push_str(&text);
            out.push('\n');
        }
        out.push_str(&self.main());
        out
    }

    /// A helper of `kind`, over its parameters and the helpers before it.
    fn helper(&mut self, kind: Kind) -> String {
        let prefix = match kind {
            Kind::Int => "h",
            Kind::Recursive => "r",
            Kind::Float => "g",
            Kind::Res => "e",
            Kind::Opt => "o",
        };
        let name = self.fresh(prefix);
        self.env.clear();
        self.ctx = Ctx::Helper(kind);
        let (params, returns) = match kind {
            Kind::Int => ("a: Int, b: Int", "Int"),
            Kind::Recursive => ("n: Int, a: Int", "Int"),
            Kind::Float => ("x: Float, y: Float", "Float"),
            Kind::Res => ("a: Int, b: Int", "Result<Int, Error>"),
            Kind::Opt => ("a: Int, b: Int", "Option<Int>"),
        };
        match kind {
            Kind::Recursive => {
                self.bind("n", Ty::Int, false);
                self.bind("a", Ty::Int, false);
            }
            Kind::Float => {
                self.bind("x", Ty::Float, false);
                self.bind("y", Ty::Float, false);
            }
            _ => {
                self.bind("a", Ty::Int, false);
                self.bind("b", Ty::Int, false);
            }
        }
        let mut text = format!("fn {name}({params}) -> {returns} {{\n  var acc = 0\n");
        self.bind("acc", Ty::Int, true);
        match kind {
            Kind::Recursive => text.push_str("  if n <= 0 {\n    return a\n  }\n"),
            Kind::Float => {
                text.push_str("  var t = x\n");
                self.bind("t", Ty::Float, true);
            }
            Kind::Res => {
                let c = self.bool(1);
                let _ = writeln!(
                    text,
                    "  if {c} {{\n    return Err(Error(\"{name} {{a}} {{b}}\"))\n  }}"
                );
            }
            Kind::Opt => {
                let c = self.bool(1);
                let _ = writeln!(text, "  if {c} {{\n    return None\n  }}");
            }
            Kind::Int => {}
        }
        let body = self.block(1, 4);
        text.push_str(&body);
        match kind {
            Kind::Recursive => {
                // Visible to its own body only here, and only with a count
                // that falls by one, so the depth is whatever `n` the caller
                // gave it.
                let next = self.int(1);
                let tail = if self.chance(1, 2) {
                    format!("{name}(n - 1, {next})")
                } else {
                    let here = self.int(2);
                    format!("({here}).bitXor({name}(n - 1, {next}))")
                };
                let _ = writeln!(text, "  acc.bitXor({tail})\n}}");
            }
            Kind::Int => {
                let tail = self.int(0);
                let _ = writeln!(text, "  acc.bitXor({tail})\n}}");
            }
            Kind::Float => {
                let op = ["+", "-", "*", "/"][self.pick(4)];
                let tail = self.float(1);
                let _ = writeln!(text, "  (t {op} {tail}) + (acc).toFloat()\n}}");
            }
            Kind::Res => {
                let tail = self.int(1);
                let _ = writeln!(text, "  Ok(acc.bitXor({tail}))\n}}");
            }
            Kind::Opt => {
                let tail = self.int(1);
                let _ = writeln!(text, "  Some(acc.bitXor({tail}))\n}}");
            }
        }
        self.helpers.push(Helper { name, kind });
        text
    }

    fn main(&mut self) -> String {
        self.env.clear();
        self.ctx = Ctx::Main;
        let mut text = String::from(
            "/// Generated by cove-fuzz.\nexport fn main() -> Result<Unit, Error> {\n  var acc = 0\n",
        );
        self.bind("acc", Ty::Int, true);
        let body = self.block(1, 24);
        text.push_str(&body);
        // What is left unfinished is finished here, so a builder's last
        // bytes meet `finish` whatever happened to them.
        let builders: Vec<_> = self
            .locals(Ty::Sb)
            .into_iter()
            .filter(|l| l.unique)
            .collect();
        for b in builders {
            let s = self.fresh("s");
            let _ = writeln!(text, "  let {s}: String = {}.finish()", b.name);
            self.bind(&s, Ty::Str, false);
        }
        let shown: Vec<_> = self.env.iter().filter_map(shown).collect();
        let _ = writeln!(text, "  println(\"{}\")?\n  Ok(())\n}}", shown.join(" "));
        text
    }

    /// Up to `most` statements at `indent`, in a scope of their own.
    fn block(&mut self, indent: usize, most: usize) -> String {
        let scope = self.env.len();
        let count = self.pick(most + 1);
        let mut text = String::new();
        for _ in 0..count {
            if self.statements >= MAX_STATEMENTS {
                break;
            }
            self.statements += 1;
            let line = self.statement(indent);
            text.push_str(&line);
        }
        // Builders declared in this scope are finished in it, so none is
        // consumed on one path and live on another.
        let builders: Vec<_> = self.env[scope..]
            .iter()
            .filter(|l| l.ty == Ty::Sb && l.unique)
            .cloned()
            .collect();
        let pad = "  ".repeat(indent);
        for b in builders {
            let _ = writeln!(
                text,
                "{pad}acc = acc.bitXor({}.finish().byteLength())",
                b.name
            );
        }
        self.env.truncate(scope);
        text
    }

    fn statement(&mut self, indent: usize) -> String {
        let pad = "  ".repeat(indent);
        let nested = indent < 4;
        let main = self.ctx == Ctx::Main;
        match self.pick(24) {
            0..=4 => self.declaration(&pad),
            5 => {
                let e = self.int(0);
                format!("{pad}acc = acc.bitXor({e})\n")
            }
            6 => self.assignment(&pad),
            7 | 8 => self.vector_statement(&pad, indent == 1),
            9 | 10 => self.builder_statement(&pad),
            11 if nested => {
                let c = self.bool(0);
                let then = self.block(indent + 1, 3);
                let otherwise = self.block(indent + 1, 2);
                format!("{pad}if {c} {{\n{then}{pad}}} else {{\n{otherwise}{pad}}}\n")
            }
            12 if nested => self.while_loop(indent),
            13 if nested => self.for_loop(indent),
            14 if self.in_loop => {
                let c = self.bool(1);
                format!("{pad}if {c} {{\n{pad}  break\n{pad}}}\n")
            }
            15 if main => {
                let s = self.string(1);
                let which = ["println", "eprintln"][self.pick(2)];
                format!("{pad}{which}({s})?\n")
            }
            16 => self.try_statement(&pad),
            17 => self.pt_statement(&pad),
            18 if nested => self.churn(indent),
            19 if main && self.differential => self.clock_statement(&pad),
            20 => {
                let f = self.float(0);
                match self.mutable_local(Ty::Float) {
                    Some(t) => {
                        let op = ["+=", "-=", "*=", "/="][self.pick(4)];
                        format!("{pad}{} {op} {f}\n", t.name)
                    }
                    None => format!("{pad}acc = acc.bitXor(({f}).toInt().unwrapOr(0))\n"),
                }
            }
            _ => {
                let e = self.int(0);
                format!("{pad}acc = acc.bitXor({e})\n")
            }
        }
    }

    fn declaration(&mut self, pad: &str) -> String {
        let ty = match self.pick(17) {
            0..=2 => Ty::Int,
            3 => Ty::Bool,
            4 | 5 => Ty::Str,
            6 => Ty::Arr,
            7 => Ty::Strs,
            8 => Ty::Vec,
            9 => Ty::Sb,
            10 | 11 => Ty::Float,
            12 => Ty::Opt,
            13 => Ty::Res,
            14 => Ty::Pt,
            15 => Ty::Shape,
            _ => Ty::Fun,
        };
        let (prefix, mutable) = match ty {
            Ty::Vec => ("v", true),
            Ty::Sb => ("sb", true),
            Ty::Fun | Ty::Shape | Ty::Opt | Ty::Res => ("x", false),
            _ => ("x", self.chance(1, 3)),
        };
        // The vector and builder constructors here are fresh, which is
        // what lets `freeze` and `finish` consume what they bind.
        let (value, unique) = match ty {
            Ty::Vec => self.fresh_vector(1),
            Ty::Sb => (
                format!("StringBuilder.withCapacity({})", self.capacity()),
                true,
            ),
            // An annotated binding is the one place a bare `None`, `Ok` or
            // `Err` has a type to take.
            Ty::Opt if self.chance(1, 4) => ("None".to_string(), false),
            Ty::Opt if self.chance(1, 4) => (format!("Some({})", self.int(1)), false),
            Ty::Res if self.chance(1, 4) => (format!("Ok({})", self.int(1)), false),
            Ty::Res if self.chance(1, 4) => (format!("Err(Error({}))", self.string(2)), false),
            _ => (self.expr(ty, 0), false),
        };
        let name = self.fresh(prefix);
        let keyword = if mutable { "var" } else { "let" };
        let text = format!("{pad}{keyword} {name}: {} = {value}\n", ty.name());
        self.bind(&name, ty, mutable);
        if let Some(local) = self.env.last_mut() {
            local.unique = unique;
        }
        text
    }

    fn assignment(&mut self, pad: &str) -> String {
        let ty = [Ty::Int, Ty::Str, Ty::Bool, Ty::Arr, Ty::Float, Ty::Pt][self.pick(6)];
        let Some(target) = self.mutable_local(ty) else {
            let e = self.int(0);
            return format!("{pad}acc = acc.bitXor({e})\n");
        };
        let value = self.expr(ty, 0);
        if ty == Ty::Int && self.chance(1, 3) {
            let op = ["+=", "-=", "*="][self.pick(3)];
            return format!("{pad}{} {op} {value}\n", target.name);
        }
        format!("{pad}{} = {value}\n", target.name)
    }

    /// `let x: Int = <a Result or an Option>?`, where the function this is in
    /// answers one: what `?` propagates is that function's answer, so a `?`
    /// in `main` can end the run with `Err`.
    fn try_statement(&mut self, pad: &str) -> String {
        let operand = match self.ctx {
            Ctx::Main | Ctx::Helper(Kind::Res) => self.res(1),
            Ctx::Helper(Kind::Opt) => self.opt(1),
            _ => {
                let e = self.int(0);
                return format!("{pad}acc = acc.bitXor({e})\n");
            }
        };
        let x = self.fresh("x");
        let text = format!("{pad}let {x}: Int = {operand}?\n");
        self.bind(&x, Ty::Int, false);
        text
    }

    /// A struct copied and then mutated through the copy, or a field written:
    /// a `Pt` is a value, so the original must not see the write.
    fn pt_statement(&mut self, pad: &str) -> String {
        let Some(p) = self.local(Ty::Pt) else {
            return self.declaration(pad);
        };
        if p.mutable && !p.counter && self.chance(1, 2) {
            let (field, value) = match self.pick(3) {
                0 => ("x", self.int(1)),
                1 => ("f", self.float(1)),
                _ => ("s", self.string(1)),
            };
            return format!("{pad}{}.{field} = {value}\n", p.name);
        }
        let copy = self.fresh("x");
        let e = self.int(1);
        let text = format!(
            "{pad}var {copy}: Pt = {}\n{pad}{copy}.x = {e}\n{pad}acc = acc.bitXor({copy}.x).bitXor({}.x)\n",
            p.name, p.name
        );
        self.bind(&copy, Ty::Pt, true);
        text
    }

    /// A loop that builds strings into a vector and drops the oldest, so that
    /// a small heap collects while the vector, the strings and every local
    /// around them are live.
    fn churn(&mut self, indent: usize) -> String {
        let pad = "  ".repeat(indent);
        let v = self.fresh("c");
        let e = self.fresh("e");
        let bound = if self.chance(1, 4) {
            format!("({}).bitAnd(255)", self.int(2))
        } else {
            (8 * self.pick(17)).to_string()
        };
        let keep = 1 + self.pick(8);
        let s = self.string_atom();
        format!(
            "{pad}var {v}: Vector<String> = Vector.of()\n\
             {pad}for {e} in 0..<{bound} {{\n\
             {pad}  {v}.push(\" \".join([\"{{{e}}}\", {s}]))\n\
             {pad}  if {v}.length() > {keep} {{\n\
             {pad}    acc = acc.bitXor({v}.remove(0).unwrapOr(\"\").byteLength())\n\
             {pad}  }}\n\
             {pad}}}\n\
             {pad}acc = acc.bitXor({v}.length())\n"
        )
    }

    /// The virtual clock: a sleep the program's own arithmetic sizes, which
    /// may be negative (an `Err`, propagated) or overflow (a trap), and a
    /// reading of the time.
    fn clock_statement(&mut self, pad: &str) -> String {
        if self.chance(1, 2) {
            let n = if self.chance(1, 4) {
                self.int(2)
            } else {
                self.pick(20).to_string()
            };
            let unit = ["nanos", "micros", "millis", "seconds"][self.pick(4)];
            format!("{pad}clock.sleep(Duration.{unit}({n}))?\n")
        } else {
            format!("{pad}acc = acc.bitXor(clock.now().nanos())\n")
        }
    }

    /// A statement on a vector. `top` is the top level of a function body,
    /// the one place a `freeze` cannot be on only one of two paths.
    fn vector_statement(&mut self, pad: &str, top: bool) -> String {
        let Some(v) = self.mutable_local(Ty::Vec) else {
            return self.declaration(pad);
        };
        let name = v.name.clone();
        match self.pick(8) {
            0..=2 => {
                let e = self.int(1);
                format!("{pad}{name}.push({e})\n")
            }
            3 => format!("{pad}acc = acc.bitXor({name}.pop().unwrapOr(0))\n"),
            4 => {
                let i = self.index(&name, Ty::Vec);
                let e = self.int(2);
                format!("{pad}acc = acc.bitXor({name}.set({i}, {e}).unwrapOr(0))\n")
            }
            5 => {
                let i = self.index(&name, Ty::Vec);
                format!("{pad}acc = acc.bitXor({name}.remove({i}).unwrapOr(0))\n")
            }
            6 => {
                // An alias: every growth through it is visible through `name`.
                let alias = self.fresh("v");
                let e = self.int(2);
                let text =
                    format!("{pad}var {alias}: Vector<Int> = {name}\n{pad}{alias}.push({e})\n");
                if let Some(local) = self.env.iter_mut().find(|l| l.name == name) {
                    local.unique = false;
                }
                self.bind(&alias, Ty::Vec, true);
                text
            }
            _ if v.unique && top => {
                let a = self.fresh("x");
                self.env.retain(|l| l.name != name);
                let text = format!("{pad}let {a}: Array<Int> = {name}.freeze()\n");
                self.bind(&a, Ty::Arr, false);
                text
            }
            _ => {
                let a = self.fresh("x");
                let text = format!("{pad}let {a}: Array<Int> = {name}.toArray()\n");
                self.bind(&a, Ty::Arr, false);
                text
            }
        }
    }

    fn builder_statement(&mut self, pad: &str) -> String {
        let builders: Vec<_> = self
            .locals(Ty::Sb)
            .into_iter()
            .filter(|l| l.unique)
            .collect();
        if builders.is_empty() {
            return self.declaration(pad);
        }
        let name = builders[self.pick(builders.len())].name.clone();
        match self.pick(8) {
            0 | 1 => {
                let s = self.string(1);
                format!("{pad}{name}.append({s})\n")
            }
            2..=4 => {
                let s = self.string_atom();
                let (from, to) = self.byte_range(&s);
                format!("{pad}{name}.appendSlice({s}, {from}, {to})\n")
            }
            5 | 6 => {
                let b = self.byte();
                format!("{pad}{name}.appendByte({b})\n")
            }
            _ => {
                format!("{pad}acc = acc.bitXor({name}.length())\n")
            }
        }
    }

    fn while_loop(&mut self, indent: usize) -> String {
        let pad = "  ".repeat(indent);
        let i = self.fresh("i");
        let bound = self.loop_bound();
        let was = self.in_loop;
        self.in_loop = true;
        self.env.push(Local {
            name: i.clone(),
            ty: Ty::Int,
            mutable: true,
            counter: true,
            capturable: false,
            unique: false,
        });
        let body = self.block(indent + 1, 4);
        self.env.pop();
        self.in_loop = was;
        format!("{pad}var {i} = 0\n{pad}while {i} < {bound} {{\n{body}{pad}  {i} += 1\n{pad}}}\n")
    }

    fn for_loop(&mut self, indent: usize) -> String {
        let pad = "  ".repeat(indent);
        let x = self.fresh("e");
        let over = match self.pick(3) {
            0 => format!("0..<{}", self.loop_bound()),
            1 => self.array(1),
            _ => self.array_atom(),
        };
        let was = self.in_loop;
        self.in_loop = true;
        self.bind(&x, Ty::Int, false);
        let body = self.block(indent + 1, 4);
        self.env.pop();
        self.in_loop = was;
        format!("{pad}for {x} in {over} {{\n{body}{pad}}}\n")
    }

    // ---- boundary values -------------------------------------------------

    fn edge(&mut self) -> String {
        match self.pick(4) {
            0 => {
                let v: i64 = self.u.arbitrary().unwrap_or(0);
                literal(v)
            }
            _ => literal(EDGES[self.pick(EDGES.len())]),
        }
    }

    fn float_edge(&mut self) -> String {
        match self.pick(5) {
            0 => {
                let bits: u64 = self.u.arbitrary().unwrap_or(0);
                float_literal(f64::from_bits(bits))
            }
            _ => float_literal(FLOAT_EDGES[self.pick(FLOAT_EDGES.len())]),
        }
    }

    /// A small count most of the time, an edge some of the time.
    fn loop_bound(&mut self) -> String {
        if self.chance(1, 8) {
            let e = self.int(2);
            format!("({e}).bitAnd(63)")
        } else {
            self.pick(9).to_string()
        }
    }

    fn capacity(&mut self) -> String {
        match self.pick(6) {
            0 => "0".to_string(),
            1 => "-1".to_string(),
            2 => self.edge(),
            _ => self.pick(65).to_string(),
        }
    }

    fn byte(&mut self) -> String {
        match self.pick(8) {
            0 => self.edge(),
            1 => ["256", "-1", "128", "191", "192", "237", "244", "245"][self.pick(8)].to_string(),
            _ => self.pick(256).to_string(),
        }
    }

    /// An index into `name`, of type `ty`: often in range, often one either
    /// side of the end, sometimes anything.
    fn index(&mut self, name: &str, ty: Ty) -> String {
        let length = match ty {
            Ty::Str => "byteLength",
            _ => "length",
        };
        match self.pick(6) {
            0 => format!("{name}.{length}()"),
            1 => format!("({name}.{length}() - 1)"),
            2 => format!("({name}.{length}() + 1)"),
            3 => self.edge(),
            _ => self.pick(4).to_string(),
        }
    }

    /// A byte range over `s`: whole, a prefix, backwards, past the end, or
    /// anything.
    fn byte_range(&mut self, s: &str) -> (String, String) {
        match self.pick(6) {
            0 | 1 => ("0".to_string(), format!("{s}.byteLength()")),
            2 => (format!("{s}.byteLength()"), "0".to_string()),
            3 => ("1".to_string(), format!("({s}.byteLength() + 1)")),
            4 => (self.int(2), self.int(2)),
            _ => {
                let from = self.pick(4).to_string();
                (from, self.pick(8).to_string())
            }
        }
    }

    /// A digit count for `Float.format`: in `0..=17` almost always.
    fn digits(&mut self) -> String {
        match self.pick(20) {
            0 => ["-1", "18", "100"][self.pick(3)].to_string(),
            _ => self.pick(18).to_string(),
        }
    }

    fn fresh_vector(&mut self, depth: usize) -> (String, bool) {
        match self.pick(4) {
            0 => ("Vector.of()".to_string(), true),
            1 => {
                let n = if self.chance(1, 6) {
                    self.edge()
                } else {
                    self.pick(10).to_string()
                };
                let e = self.int(2);
                (format!("Vector.filled({n}, {e})"), true)
            }
            2 => {
                let items = self.ints(3);
                (format!("Vector.of({items})"), true)
            }
            _ => {
                let a = self.array(depth.max(1));
                (format!("({a}).toVector()"), true)
            }
        }
    }

    // ---- expressions -----------------------------------------------------

    fn expr(&mut self, ty: Ty, depth: usize) -> String {
        match ty {
            Ty::Int => self.int(depth),
            Ty::Bool => self.bool(depth),
            Ty::Str => self.string(depth),
            Ty::Arr => self.array(depth),
            Ty::Strs => self.strings(depth),
            Ty::Vec => self.fresh_vector(depth).0,
            Ty::Sb => unreachable!("a builder is only ever declared"),
            Ty::Float => self.float(depth),
            Ty::Opt => self.opt(depth),
            Ty::Res => self.res(depth),
            Ty::Pt => self.pt(depth),
            Ty::Shape => self.shape(depth),
            Ty::Fun => self.fun(depth),
        }
    }

    fn ints(&mut self, depth: usize) -> String {
        let n = self.pick(5);
        (0..n)
            .map(|_| self.int(depth + 1))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn int(&mut self, depth: usize) -> String {
        if depth >= MAX_DEPTH {
            return self.int_leaf();
        }
        let d = depth + 1;
        match self.pick(36) {
            0..=4 => self.int_leaf(),
            5 => {
                let op = ["+", "-", "*", "/", "%"][self.pick(5)];
                let a = self.int(d);
                let b = self.int(d);
                format!("({a} {op} {b})")
            }
            6 => {
                let a = self.int(d);
                format!("(-({a}))")
            }
            7 => {
                let m = ["bitAnd", "bitOr", "bitXor", "floorDiv", "floorMod"][self.pick(5)];
                let a = self.int(d);
                let b = self.int(d);
                format!("({a}).{m}({b})")
            }
            8 => {
                let m = ["shiftLeft", "shiftRight", "shiftRightLogical"][self.pick(3)];
                let a = self.int(d);
                let count = if self.chance(1, 4) {
                    self.int(d)
                } else {
                    format!("({}).bitAnd(63)", self.int(d))
                };
                format!("({a}).{m}({count})")
            }
            9 => {
                let a = self.int(d);
                format!("({a}).bitNot()")
            }
            10 => {
                let s = self.string(d);
                let m = ["length", "byteLength"][self.pick(2)];
                format!("({s}).{m}()")
            }
            11 => {
                let a = self.array(d);
                let i = self.int(d);
                let or = self.int_leaf();
                match self.pick(3) {
                    0 => format!("({a}).length()"),
                    1 => format!("({a}).get({i}).unwrapOr({or})"),
                    _ => format!("({a}).indexOf({i}).unwrapOr({or})"),
                }
            }
            12 => match self.local(Ty::Vec) {
                Some(v) => {
                    let i = self.index(&v.name, Ty::Vec);
                    let or = self.int_leaf();
                    match self.pick(3) {
                        0 => format!("{}.length()", v.name),
                        _ => format!("{}.get({i}).unwrapOr({or})", v.name),
                    }
                }
                None => self.int_leaf(),
            },
            13 => {
                let s = self.string_atom();
                let i = self.index(&s, Ty::Str);
                format!("{s}.codePointAtByte({i}).unwrapOr(-1)")
            }
            14 => {
                let s = self.string(d);
                let t = self.string(d);
                format!("({s}).indexOf({t}).unwrapOr(-1)")
            }
            15 => {
                let s = self.string(d);
                let or = self.int_leaf();
                if self.chance(1, 2) {
                    format!("Int.parse({s}).unwrapOr({or})")
                } else {
                    let radix = self.radix();
                    format!("Int.parseRadix({s}, {radix}).unwrapOr({or})")
                }
            }
            16 => {
                let a = self.array(d);
                let init = self.int(d);
                let (p, x) = (self.fresh("p"), self.fresh("q"));
                let body = self.closure_int(&[&p, &x], d);
                format!("({a}).fold({init}, fn({p}, {x}) {{ {body} }})")
            }
            17 => self.call(d),
            18 => {
                let c = self.bool(d);
                let a = self.int(d);
                let b = self.int(d);
                format!("(if {c} {{ {a} }} else {{ {b} }})")
            }
            19 => {
                let a = self.array(d);
                let i = self.int(d);
                let x = self.fresh("m");
                self.bind(&x, Ty::Int, false);
                let some = self.int(d);
                self.env.pop();
                let none = self.int(d);
                format!(
                    "(match ({a}).get({i}) {{\n    Some({x}) => {some}\n    None => {none}\n  }})"
                )
            }
            20 => match self.local(Ty::Sb).filter(|l| l.unique) {
                Some(b) => format!("{}.length()", b.name),
                None => self.int_leaf(),
            },
            21 => {
                let s = self.strings(d);
                format!("({s}).length()")
            }
            22 | 23 => {
                // A `Float` back to an `Int`: `toInt` refuses NaN, the
                // infinities and everything outside `Int`'s range.
                let f = self.float(d);
                let or = self.int_leaf();
                if self.chance(1, 3) {
                    format!("({f}).round().toInt().unwrapOr({or})")
                } else {
                    format!("({f}).toInt().unwrapOr({or})")
                }
            }
            24 => {
                let r = self.res(d);
                let or = self.int_leaf();
                format!("({r}).unwrapOr({or})")
            }
            25 => {
                let r = self.res(d);
                let v = self.fresh("m");
                self.bind(&v, Ty::Int, false);
                let ok = self.int(d);
                self.env.pop();
                let err = if self.chance(1, 2) {
                    "e.message.byteLength()".to_string()
                } else {
                    self.int(d)
                };
                format!("(match {r} {{\n    Ok({v}) => {ok}\n    Err(e) => {err}\n  }})")
            }
            26 => {
                let o = self.opt(d);
                let or = self.int_leaf();
                format!("({o}).unwrapOr({or})")
            }
            27 => {
                let o = self.opt(d);
                let v = self.fresh("m");
                self.bind(&v, Ty::Int, false);
                let some = self.int(d);
                self.env.pop();
                let none = self.int(d);
                format!("(match {o} {{\n    Some({v}) => {some}\n    None => {none}\n  }})")
            }
            28 => {
                let p = self.pt(d);
                format!("({p}).x")
            }
            29 => self.shape_match(d),
            30 | 31 => {
                let f = self.fun(d);
                let x = self.int(d);
                match self.pick(3) {
                    0 => format!("twice({f}, {x})"),
                    1 => match self.local(Ty::Fun) {
                        Some(l) => format!("{}({x})", l.name),
                        None => format!("apply({f}, {x})"),
                    },
                    _ => format!("apply({f}, {x})"),
                }
            }
            32 => {
                // Past the end some of the time; mostly a byte that is there,
                // so that one `byteAt` does not end most programs.
                let s = self.string_atom();
                if self.chance(1, 3) {
                    let i = self.index(&s, Ty::Str);
                    format!("{s}.byteAt({i})")
                } else {
                    let i = self.int(d);
                    format!(
                        "(if {s}.isEmpty() {{ -1 }} else {{ {s}.byteAt(({i}).floorMod({s}.byteLength())) }})"
                    )
                }
            }
            _ => self.int_leaf(),
        }
    }

    fn int_leaf(&mut self) -> String {
        match self.pick(3) {
            0 => match self.local(Ty::Int) {
                Some(l) => l.name,
                None => self.edge(),
            },
            _ => self.edge(),
        }
    }

    fn radix(&mut self) -> String {
        match self.pick(12) {
            0 => self.edge(),
            1 => ["0", "1", "37", "-1"][self.pick(4)].to_string(),
            _ => ["2", "8", "10", "16", "36"][self.pick(5)].to_string(),
        }
    }

    /// A call to an `Int` helper defined above, or a leaf when there is none.
    fn call(&mut self, d: usize) -> String {
        let Some(helper) = self.helper_of(&[Kind::Int, Kind::Recursive]) else {
            return self.int_leaf();
        };
        let second = self.int(d);
        let first = if helper.kind == Kind::Recursive && !self.chance(1, 8) {
            // A depth the program computes, kept below the call-depth limit
            // most of the time and not always.
            format!("({}).bitAnd(63)", self.int(d))
        } else {
            self.int(d)
        };
        format!("{}({first}, {second})", helper.name)
    }

    /// The body of a closure over `params`, which sees only them, the
    /// capturable bindings around it, and the helpers.
    fn closure_int(&mut self, params: &[&str], d: usize) -> String {
        let saved = std::mem::take(&mut self.env);
        self.env = saved.iter().filter(|l| l.capturable).cloned().collect();
        for p in params {
            self.bind(p, Ty::Int, false);
        }
        let body = self.int(d);
        self.env = saved;
        body
    }

    fn closure_bool(&mut self, params: &[&str], d: usize) -> String {
        let saved = std::mem::take(&mut self.env);
        self.env = saved.iter().filter(|l| l.capturable).cloned().collect();
        for p in params {
            self.bind(p, Ty::Int, false);
        }
        let body = self.bool(d);
        self.env = saved;
        body
    }

    /// The body of `fold(0.0, fn(p, q) { .. })`: `p` a `Float`, `q` an `Int`.
    fn closure_float(&mut self, p: &str, q: &str, d: usize) -> String {
        let saved = std::mem::take(&mut self.env);
        self.env = saved.iter().filter(|l| l.capturable).cloned().collect();
        self.bind(p, Ty::Float, false);
        self.bind(q, Ty::Int, false);
        let body = self.float(d);
        self.env = saved;
        format!("{p} * 0.5 + ({q}).toFloat() + {body}")
    }

    fn bool(&mut self, depth: usize) -> String {
        if depth >= MAX_DEPTH {
            return self.bool_leaf();
        }
        let d = depth + 1;
        match self.pick(18) {
            0 | 1 => self.bool_leaf(),
            2..=4 => {
                let op = ["<", "<=", ">", ">=", "==", "!="][self.pick(6)];
                let a = self.int(d);
                let b = self.int(d);
                format!("({a} {op} {b})")
            }
            5 => {
                let op = ["==", "!=", "<", "<=", ">", ">="][self.pick(6)];
                let a = self.string(d);
                let b = self.string(d);
                format!("({a} {op} {b})")
            }
            6 => {
                let a = self.bool(d);
                format!("(!{a})")
            }
            7 => {
                let op = ["&&", "||"][self.pick(2)];
                let a = self.bool(d);
                let b = self.bool(d);
                format!("({a} {op} {b})")
            }
            8 => {
                let m = ["contains", "startsWith", "endsWith"][self.pick(3)];
                let a = self.string(d);
                let b = self.string(d);
                format!("({a}).{m}({b})")
            }
            9 => {
                let a = self.array(d);
                let i = self.int(d);
                if self.chance(1, 2) {
                    format!("({a}).contains({i})")
                } else {
                    format!("({a}).isEmpty()")
                }
            }
            10 => {
                let s = self.string(d);
                format!("({s}).isEmpty()")
            }
            11..=13 => {
                // Every comparison against a NaN is false, `!=` included in
                // its own way, and `-0.0 == 0.0`.
                let op = ["<", "<=", ">", ">=", "==", "!="][self.pick(6)];
                let a = self.float(d);
                let b = self.float(d);
                format!("({a} {op} {b})")
            }
            14 => {
                if self.chance(1, 2) {
                    let r = self.res(d);
                    let m = ["isOk", "isError"][self.pick(2)];
                    format!("({r}).{m}()")
                } else {
                    let o = self.opt(d);
                    let m = ["isSome", "isNone"][self.pick(2)];
                    format!("({o}).{m}()")
                }
            }
            15 => {
                let op = ["==", "!="][self.pick(2)];
                if self.chance(1, 2) {
                    let a = self.pt(d);
                    let b = self.pt(d);
                    format!("({a} {op} {b})")
                } else {
                    let a = self.shape(d);
                    let b = self.shape(d);
                    format!("({a} {op} {b})")
                }
            }
            _ => match self.local(Ty::Vec) {
                Some(v) => {
                    let i = self.int(d);
                    format!("{}.contains({i})", v.name)
                }
                None => self.bool_leaf(),
            },
        }
    }

    fn bool_leaf(&mut self) -> String {
        match self.local(Ty::Bool) {
            Some(l) if self.chance(1, 2) => l.name,
            _ => ["true", "false"][self.pick(2)].to_string(),
        }
    }

    fn string_literal(&mut self) -> String {
        let n = self.pick(7);
        let text: String = (0..n).map(|_| PIECES[self.pick(PIECES.len())]).collect();
        format!("\"{text}\"")
    }

    /// A string literal or a string local: something `codePointAtByte` and
    /// `appendSlice` can be asked about twice without evaluating it twice.
    fn string_atom(&mut self) -> String {
        match self.local(Ty::Str) {
            Some(l) if self.chance(2, 3) => l.name,
            _ => self.string_literal(),
        }
    }

    fn string(&mut self, depth: usize) -> String {
        if depth >= MAX_DEPTH {
            return self.string_atom();
        }
        let d = depth + 1;
        match self.pick(22) {
            0..=3 => self.string_atom(),
            4 => {
                let s = self.string(d);
                let a = self.int(d);
                let b = self.int(d);
                format!("({s}).slice({a}, {b})")
            }
            5 | 6 => {
                let s = self.string_atom();
                let (from, to) = self.byte_range(&s);
                let or = self.string_literal();
                format!("{s}.sliceBytes({from}, {to}).unwrapOr({or})")
            }
            7 => {
                // Interpolation over locals only: a literal inside `{}`
                // would need its own quoting rules.
                let n = 1 + self.pick(3);
                let mut text = String::new();
                for _ in 0..n {
                    let shown: Vec<_> = self
                        .env
                        .iter()
                        .filter(|l| l.ty.printable())
                        .cloned()
                        .collect();
                    if shown.is_empty() || self.chance(1, 3) {
                        text.push_str(PIECES[self.pick(PIECES.len())]);
                    } else {
                        let l = &shown[self.pick(shown.len())];
                        let _ = write!(text, "{{{}}}", l.name);
                    }
                }
                format!("\"{text}\"")
            }
            8 => {
                let s = self.string(d);
                let m = ["toUpper", "toLower", "trim"][self.pick(3)];
                format!("({s}).{m}()")
            }
            9 => {
                let s = self.string(d);
                let a = self.string_atom();
                let b = self.string_atom();
                format!("({s}).replace({a}, {b})")
            }
            10 => {
                let i = match self.pick(4) {
                    0 => [
                        "55296", "57343", "1114111", "1114112", "-1", "0", "127", "128",
                    ][self.pick(8)]
                    .to_string(),
                    _ => self.int(d),
                };
                let or = self.string_literal();
                format!("String.fromCodePoint({i}).unwrapOr({or})")
            }
            11 => {
                let bytes = self.byte_array(d);
                if self.chance(1, 2) {
                    let or = self.string_literal();
                    format!("String.fromUtf8({bytes}).unwrapOr({or})")
                } else {
                    format!("String.fromUtf8Lossy({bytes})")
                }
            }
            12 => {
                let i = self.int(d);
                let r = self.radix();
                format!("({i}).toStringRadix({r})")
            }
            13 => {
                let i = self.int(d);
                let m = ["toHex", "toDecimal"][self.pick(2)];
                let w = match self.pick(16) {
                    0 => self.edge(),
                    1 => "-1".to_string(),
                    _ => self.pick(24).to_string(),
                };
                format!("({i}).{m}({w})")
            }
            14 => {
                let sep = self.string_atom();
                let parts = self.strings(d);
                format!("{sep}.join({parts})")
            }
            15 => {
                let parts = self.strings(d);
                let i = self.int(d);
                let or = self.string_literal();
                format!("({parts}).get({i}).unwrapOr({or})")
            }
            16 | 17 => {
                let f = self.float(d);
                let digits = self.digits();
                format!("({f}).format({digits})")
            }
            18 => {
                let p = self.pt(d);
                format!("({p}).s")
            }
            19 => {
                let r = self.res(d);
                let v = self.fresh("m");
                format!(
                    "(match {r} {{\n    Ok({v}) => \"ok {{{v}}}\"\n    Err(e) => e.message\n  }})"
                )
            }
            _ => self.string_atom(),
        }
    }

    /// An `Array<Int>` that is usually bytes, for `fromUtf8`.
    fn byte_array(&mut self, d: usize) -> String {
        match self.pick(3) {
            0 => {
                let s = self.string(d);
                format!("({s}).toUtf8()")
            }
            1 => {
                let n = 1 + self.pick(6);
                let items: Vec<_> = (0..n).map(|_| self.byte()).collect();
                format!("[{}]", items.join(", "))
            }
            _ => self.array(d),
        }
    }

    fn strings(&mut self, depth: usize) -> String {
        let d = depth + 1;
        match self.pick(4) {
            0 => {
                let s = self.string(d.max(MAX_DEPTH - 1));
                format!("({s}).chars()")
            }
            1 => {
                let s = self.string(d.max(MAX_DEPTH - 1));
                let sep = self.string_atom();
                format!("({s}).split({sep})")
            }
            2 => {
                let n = 1 + self.pick(3);
                let items: Vec<_> = (0..n).map(|_| self.string_atom()).collect();
                format!("[{}]", items.join(", "))
            }
            _ => match self.local(Ty::Strs) {
                Some(l) => l.name,
                None => format!("[{}]", self.string_atom()),
            },
        }
    }

    fn array_atom(&mut self) -> String {
        match self.local(Ty::Arr) {
            Some(l) if self.chance(2, 3) => l.name,
            _ => {
                let n = 1 + self.pick(4);
                let items: Vec<_> = (0..n).map(|_| self.int_leaf()).collect();
                format!("[{}]", items.join(", "))
            }
        }
    }

    fn array(&mut self, depth: usize) -> String {
        if depth >= MAX_DEPTH {
            return self.array_atom();
        }
        let d = depth + 1;
        match self.pick(13) {
            0..=2 => self.array_atom(),
            3 => {
                let items = self.ints(d);
                if items.is_empty() {
                    "[0].slice(0, 0)".to_string()
                } else {
                    format!("[{items}]")
                }
            }
            4 => {
                let a = self.array(d);
                let from = self.int(d);
                let to = self.int(d);
                format!("({a}).slice({from}, {to})")
            }
            5 => {
                let s = self.string(d);
                format!("({s}).toUtf8()")
            }
            6 => {
                let a = self.array(d);
                let x = self.fresh("q");
                let body = self.closure_int(&[&x], d);
                format!("({a}).map(fn({x}) {{ {body} }})")
            }
            7 => {
                let a = self.array(d);
                let x = self.fresh("q");
                let body = self.closure_bool(&[&x], d);
                format!("({a}).filter(fn({x}) {{ {body} }})")
            }
            8 => {
                let a = self.array(d);
                let (p, q) = (self.fresh("q"), self.fresh("q"));
                // A comparison that may contradict itself is allowed outside
                // the differential harness: the run gets some permutation and
                // does not stop, and which one is not the language's to say.
                let body = if self.differential {
                    [format!("{p} < {q}"), format!("{p} > {q}")][self.pick(2)].clone()
                } else if self.chance(3, 4) {
                    format!("{p} < {q}")
                } else {
                    self.closure_bool(&[&p, &q], d)
                };
                format!("({a}).sorted(by: fn({p}, {q}) {{ {body} }})")
            }
            9 => match self.local(Ty::Vec) {
                Some(v) => format!("{}.toArray()", v.name),
                None => self.array_atom(),
            },
            10 => {
                // An empty `Vector.of()` is typed by its binding's annotation,
                // and here there is none.
                let (v, _) = self.fresh_vector(d);
                let v = if v == "Vector.of()" {
                    "Vector.of(0)".to_string()
                } else {
                    v
                };
                format!("({v}).toArray()")
            }
            11 => {
                // A function value handed to `map` whole, not a lambda
                // written at the call.
                let a = self.array(d);
                let f = self.fun(d);
                format!("({a}).map({f})")
            }
            _ => self.array_atom(),
        }
    }

    // ---- floats ------------------------------------------------------------

    fn float_leaf(&mut self) -> String {
        match self.pick(3) {
            0 => match self.local(Ty::Float) {
                Some(l) => l.name,
                None => self.float_edge(),
            },
            _ => self.float_edge(),
        }
    }

    fn float(&mut self, depth: usize) -> String {
        if depth >= MAX_DEPTH {
            return self.float_leaf();
        }
        let d = depth + 1;
        match self.pick(22) {
            0..=4 => self.float_leaf(),
            5 | 6 => {
                // `%` is rarer than the rest: the native tier does not lower
                // it, so a helper that has one runs on the encoded VM whole.
                let op = ["+", "-", "*", "/", "+", "-", "*", "/", "%"][self.pick(9)];
                let a = self.float(d);
                let b = self.float(d);
                format!("({a} {op} {b})")
            }
            7 => {
                let a = self.float(d);
                format!("(-({a}))")
            }
            8 => {
                let m = ["abs", "sqrt", "round"][self.pick(3)];
                let a = self.float(d);
                format!("({a}).{m}()")
            }
            9 => {
                let m = ["min", "max"][self.pick(2)];
                let a = self.float(d);
                let b = self.float(d);
                format!("({a}).{m}({b})")
            }
            10 => {
                let i = self.int(d);
                format!("({i}).toFloat()")
            }
            11 => {
                let text = FLOAT_TEXTS[self.pick(FLOAT_TEXTS.len())];
                let or = self.float_leaf();
                format!("Float.parse(\"{text}\").unwrapOr({or})")
            }
            12 => {
                // A value's own text, read back: `format(17)` is exact enough
                // to round-trip every finite value, and an arbitrary string
                // is mostly refused.
                let or = self.float_leaf();
                if self.chance(2, 3) {
                    let f = self.float(d);
                    let digits = self.digits();
                    format!("Float.parse(({f}).format({digits})).unwrapOr({or})")
                } else {
                    let s = self.string(d);
                    format!("Float.parse({s}).unwrapOr({or})")
                }
            }
            13 => {
                let m = ["exp", "ln", "log10", "sin", "cos"][self.pick(5)];
                let a = self.float(d);
                format!("({a}).{m}()")
            }
            14 => {
                let a = self.float(d);
                let b = self.float(d);
                format!("({a}).pow({b})")
            }
            15 => {
                let c = self.bool(d);
                let a = self.float(d);
                let b = self.float(d);
                format!("(if {c} {{ {a} }} else {{ {b} }})")
            }
            16 | 17 => match self.helper_of(&[Kind::Float]) {
                Some(h) => {
                    let a = self.float(d);
                    let b = self.float(d);
                    format!("{}({a}, {b})", h.name)
                }
                None => self.float_leaf(),
            },
            18 => {
                let p = self.pt(d);
                format!("({p}).f")
            }
            19 => {
                let a = self.array(d);
                let (p, q) = (self.fresh("p"), self.fresh("q"));
                let body = self.closure_float(&p, &q, d);
                format!("({a}).fold(0.0, fn({p}, {q}) {{ {body} }})")
            }
            _ => self.float_leaf(),
        }
    }

    // ---- options, results, structs, enums, functions -------------------------

    fn opt(&mut self, depth: usize) -> String {
        let d = depth + 1;
        if depth >= MAX_DEPTH {
            return match self.local(Ty::Opt) {
                Some(l) => l.name,
                None => format!("Some({})", self.int_leaf()),
            };
        }
        match self.pick(8) {
            0 => {
                let a = self.array(d);
                let i = self.int(d);
                format!("({a}).get({i})")
            }
            1 => {
                let a = self.array(d);
                let i = self.int(d);
                format!("({a}).indexOf({i})")
            }
            2 => {
                let s = self.string(d);
                let t = self.string(d);
                format!("({s}).indexOf({t})")
            }
            3 => {
                let s = self.string_atom();
                let i = self.index(&s, Ty::Str);
                format!("{s}.codePointAtByte({i})")
            }
            4 | 5 => match self.helper_of(&[Kind::Opt]) {
                Some(h) => {
                    let a = self.int(d);
                    let b = self.int(d);
                    format!("{}({a}, {b})", h.name)
                }
                None => format!("Some({})", self.int(d)),
            },
            6 => match self.local(Ty::Opt) {
                Some(l) => l.name,
                None => format!("Some({})", self.int(d)),
            },
            _ => format!("Some({})", self.int(d)),
        }
    }

    fn res(&mut self, depth: usize) -> String {
        let d = depth + 1;
        if depth >= MAX_DEPTH {
            return match self.local(Ty::Res) {
                Some(l) => l.name,
                None => format!("Int.parse({})", self.string_atom()),
            };
        }
        match self.pick(8) {
            0 => {
                let s = self.string(d);
                format!("Int.parse({s})")
            }
            1 => {
                let s = self.string(d);
                let r = self.radix();
                format!("Int.parseRadix({s}, {r})")
            }
            2 => {
                let f = self.float(d);
                format!("({f}).toInt()")
            }
            3 | 4 => match self.helper_of(&[Kind::Res]) {
                Some(h) => {
                    let a = self.int(d);
                    let b = self.int(d);
                    format!("{}({a}, {b})", h.name)
                }
                None => {
                    let f = self.float(d);
                    format!("({f}).toInt()")
                }
            },
            5 => match self.local(Ty::Res) {
                Some(l) => l.name,
                None => format!("Int.parse({})", self.string_atom()),
            },
            _ => {
                let r = self.res(d);
                format!("({r}).mapError(fn(e) {{ Error(\"wrapped {{e.message}}\") }})")
            }
        }
    }

    fn pt(&mut self, depth: usize) -> String {
        if let Some(l) = self.local(Ty::Pt) {
            if depth >= MAX_DEPTH || self.chance(1, 2) {
                return l.name;
            }
        }
        let d = (depth + 1).max(MAX_DEPTH - 1);
        let x = self.int(d);
        let f = self.float(d);
        let s = self.string(d);
        format!("Pt(x: {x}, f: {f}, s: {s})")
    }

    fn shape(&mut self, depth: usize) -> String {
        if let Some(l) = self.local(Ty::Shape) {
            if depth >= MAX_DEPTH || self.chance(1, 2) {
                return l.name;
            }
        }
        let d = (depth + 1).max(MAX_DEPTH - 1);
        match self.pick(3) {
            0 => "Shape.Dot".to_string(),
            1 => format!("Shape.Line({})", self.int(d)),
            _ => {
                let n = self.int(d);
                let s = self.string(d);
                format!("Shape.Box({n}, {s})")
            }
        }
    }

    fn shape_match(&mut self, d: usize) -> String {
        let s = self.shape(d);
        let dot = self.int(d);
        let k = self.fresh("m");
        self.bind(&k, Ty::Int, false);
        let line = self.int(d);
        let t = self.fresh("m");
        self.bind(&t, Ty::Str, false);
        let boxed = self.int(d);
        self.env.pop();
        self.env.pop();
        format!(
            "(match {s} {{\n    Shape.Dot => {dot}\n    Shape.Line({k}) => {line}\n    Shape.Box({k}, {t}) => {boxed}\n  }})"
        )
    }

    fn fun(&mut self, depth: usize) -> String {
        let d = depth + 1;
        if depth >= MAX_DEPTH {
            return match self.local(Ty::Fun) {
                Some(l) => l.name,
                None => format!("adder({})", self.int_leaf()),
            };
        }
        match self.pick(6) {
            0 | 1 => {
                let x = self.fresh("q");
                let body = self.closure_int(&[&x], d);
                format!("fn({x}) {{ {body} }}")
            }
            2 => format!("adder({})", self.int(d)),
            3 => {
                let f = self.fun(d);
                let g = self.fun(d);
                format!("compose({f}, {g})")
            }
            _ => match self.local(Ty::Fun) {
                Some(l) => l.name,
                None => format!("adder({})", self.int(d)),
            },
        }
    }
}

/// How `main`'s last line shows a binding, if it can: a printable one as
/// itself, the others through a field or a call that answers something
/// printable.
fn shown(l: &Local) -> Option<String> {
    let n = &l.name;
    Some(match l.ty {
        Ty::Sb | Ty::Shape => return None,
        Ty::Opt | Ty::Res => format!("{{{n}.unwrapOr(-1)}}"),
        Ty::Pt => format!("{{{n}.x}} {{{n}.f}} {{{n}.s}}"),
        Ty::Fun => format!("{{{n}(1)}}"),
        _ => format!("{{{n}}}"),
    })
}

/// `v` as Cove source. `i64::MIN` has no literal — its magnitude is one past
/// the largest — so it is written as the subtraction that makes it.
fn literal(v: i64) -> String {
    if v == i64::MIN {
        "(-9223372036854775807 - 1)".to_string()
    } else if v < 0 {
        format!("({v})")
    } else {
        v.to_string()
    }
}

/// `v` as Cove source. A NaN and the infinities have no literal, so they are
/// written as the divisions that make them; a negative value, `-0.0`
/// included, as the negation of its magnitude. Rust's shortest round-trip text
/// is Cove's literal syntax once it has a point.
fn float_literal(v: f64) -> String {
    if v.is_nan() {
        return "(0.0 / 0.0)".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 {
            "(1.0 / 0.0)"
        } else {
            "(-1.0 / 0.0)"
        }
        .to_string();
    }
    let mut text = format!("{:?}", v.abs());
    if !text.contains('.') {
        match text.find('e') {
            Some(at) => text.insert_str(at, ".0"),
            None => text.push_str(".0"),
        }
    }
    if v.is_sign_negative() {
        format!("(-{text})")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::float_literal;

    #[test]
    fn a_float_literal_reads_back_as_the_value() {
        for v in super::FLOAT_EDGES {
            let text = float_literal(v);
            if v.is_finite() {
                let digits = text.trim_start_matches("(-").trim_end_matches(')');
                let back: f64 = digits.parse().expect("Rust reads the literal");
                assert_eq!(back, v.abs(), "{text}");
                assert!(digits.contains('.'), "{text} has a point");
            }
        }
        assert_eq!(float_literal(1e16), "1.0e16");
        assert_eq!(float_literal(-0.0), "(-0.0)");
        assert_eq!(float_literal(5e-324), "5.0e-324");
    }
}
