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
//! - `String.slice`, `sliceBytes`, `byteLength`, `codePointAtByte`, `chars`,
//!   `split`, `toUtf8`, `fromUtf8`, `fromUtf8Lossy`, `fromCodePoint`, over
//!   literals that mix one- to four-byte UTF-8 and a combining mark;
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
//! Most of those can stop the run — an overflow, a division by zero, a range
//! `appendSlice` refuses, a heap that cannot hold `Vector.filled(2^40, 0)`.
//! That is the point and is not a finding; it is why every hazard is drawn
//! only *some* of the time, so that a program also gets far enough to run the
//! rest of what it says.
//!
//! # Honesty
//!
//! [`crate::structured`] panics if a generated program does not check. That
//! keeps this module from drifting into generating programs the front end
//! rejects — which would still "pass", while testing nothing past the parser.

use std::fmt::Write as _;

use arbitrary::Unstructured;

/// The program `data` generates.
pub fn from_bytes(data: &[u8]) -> String {
    generate(&mut Unstructured::new(data))
}

/// A whole program — helpers, then `export fn main() -> Result<Unit, Error>` —
/// generated from the choices `u` supplies. An exhausted `u` answers its
/// smallest choice every time, so every input ends in a finite program.
pub fn generate(u: &mut Unstructured<'_>) -> String {
    let mut g = Gen {
        u,
        next: 0,
        env: Vec::new(),
        helpers: Vec::new(),
        statements: 0,
        in_loop: false,
        in_main: false,
    };
    g.program()
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
        }
    }
}

#[derive(Clone, Debug)]
struct Local {
    name: String,
    ty: Ty,
    mutable: bool,
    /// A `while` counter: read freely, assigned only by its own loop.
    counter: bool,
    /// Visible inside a closure written here. Only immutable `Int` and
    /// `String` bindings are, which keeps every closure task-safe and keeps
    /// vectors and builders out of anything that could hold them.
    capturable: bool,
    /// A vector or builder that nothing else holds, so `freeze` or `finish`
    /// may consume it.
    unique: bool,
}

#[derive(Clone, Debug)]
struct Helper {
    name: String,
    /// `rN(n, a)`, which recurses `n` times.
    recursive: bool,
}

struct Gen<'a, 'b> {
    u: &'b mut Unstructured<'a>,
    next: usize,
    env: Vec<Local>,
    helpers: Vec<Helper>,
    /// Statements generated so far, which bounds the program's size.
    statements: usize,
    in_loop: bool,
    in_main: bool,
}

/// The most statements a program is given, across every function.
const MAX_STATEMENTS: usize = 60;
/// How deep an expression nests.
const MAX_DEPTH: usize = 4;

impl Gen<'_, '_> {
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
            capturable: !mutable && matches!(ty, Ty::Int | Ty::Str),
            unique: false,
        });
    }

    // ---- program ----------------------------------------------------------

    fn program(&mut self) -> String {
        let mut out = String::from("use console.println\nuse std.stringbuilder.StringBuilder\n\n");
        let helpers = self.pick(4);
        for _ in 0..helpers {
            let recursive = self.chance(1, 3);
            let text = self.helper(recursive);
            out.push_str(&text);
            out.push('\n');
        }
        out.push_str(&self.main());
        out
    }

    /// `fn hN(a: Int, b: Int) -> Int` or `fn rN(n: Int, a: Int) -> Int`,
    /// over its parameters and the helpers before it.
    fn helper(&mut self, recursive: bool) -> String {
        let name = self.fresh(if recursive { "r" } else { "h" });
        let (p, q) = if recursive { ("n", "a") } else { ("a", "b") };
        self.env.clear();
        for param in [p, q] {
            self.bind(param, Ty::Int, false);
        }
        self.in_main = false;
        let mut text = format!("fn {name}({p}: Int, {q}: Int) -> Int {{\n  var acc = 0\n");
        self.bind("acc", Ty::Int, true);
        if recursive {
            text.push_str("  if n <= 0 {\n    return a\n  }\n");
        }
        let body = self.block(1, 4);
        text.push_str(&body);
        let tail = if recursive {
            // Visible to its own body only here, and only with a count that
            // falls by one, so the depth is whatever `n` the caller gave it.
            let next = self.int(1);
            if self.chance(1, 2) {
                format!("{name}(n - 1, {next})")
            } else {
                let here = self.int(2);
                format!("({here}).bitXor({name}(n - 1, {next}))")
            }
        } else {
            self.int(0)
        };
        let _ = writeln!(text, "  acc.bitXor({tail})\n}}");
        self.helpers.push(Helper { name, recursive });
        text
    }

    fn main(&mut self) -> String {
        self.env.clear();
        self.in_main = true;
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
        let shown: Vec<_> = self
            .env
            .iter()
            .filter(|l| l.ty != Ty::Sb)
            .map(|l| format!("{{{}}}", l.name))
            .collect();
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
        match self.pick(16) {
            0..=3 => self.declaration(&pad),
            4 => {
                let e = self.int(0);
                format!("{pad}acc = acc.bitXor({e})\n")
            }
            5 => self.assignment(&pad),
            6 | 7 => self.vector_statement(&pad, indent == 1),
            8 | 9 => self.builder_statement(&pad),
            10 if nested => {
                let c = self.bool(0);
                let then = self.block(indent + 1, 3);
                let otherwise = self.block(indent + 1, 2);
                format!("{pad}if {c} {{\n{then}{pad}}} else {{\n{otherwise}{pad}}}\n")
            }
            11 if nested => self.while_loop(indent),
            12 if nested => self.for_loop(indent),
            13 if self.in_loop => {
                let c = self.bool(1);
                format!("{pad}if {c} {{\n{pad}  break\n{pad}}}\n")
            }
            14 if self.in_main => {
                let s = self.string(1);
                format!("{pad}println({s})?\n")
            }
            _ => {
                let e = self.int(0);
                format!("{pad}acc = acc.bitXor({e})\n")
            }
        }
    }

    fn declaration(&mut self, pad: &str) -> String {
        let ty = match self.pick(10) {
            0..=2 => Ty::Int,
            3 => Ty::Bool,
            4 | 5 => Ty::Str,
            6 => Ty::Arr,
            7 => Ty::Strs,
            8 => Ty::Vec,
            _ => Ty::Sb,
        };
        let (prefix, mutable) = match ty {
            Ty::Vec => ("v", true),
            Ty::Sb => ("sb", true),
            _ => ("x", self.chance(1, 3)),
        };
        // The vector and builder constructors here are fresh, which is
        // what lets `freeze` and `finish` consume what they bind.
        let (value, unique) = match ty {
            Ty::Vec => self.fresh_vector(),
            Ty::Sb => (
                format!("StringBuilder.withCapacity({})", self.capacity()),
                true,
            ),
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
        let ty = [Ty::Int, Ty::Str, Ty::Bool, Ty::Arr][self.pick(4)];
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

    fn fresh_vector(&mut self) -> (String, bool) {
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
                let a = self.array(1);
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
            Ty::Vec => self.fresh_vector().0,
            Ty::Sb => unreachable!("a builder is only ever declared"),
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
        match self.pick(24) {
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
        match self.pick(6) {
            0 => self.edge(),
            1 => ["0", "1", "37", "-1"][self.pick(4)].to_string(),
            _ => ["2", "8", "10", "16", "36"][self.pick(5)].to_string(),
        }
    }

    /// A call to a helper defined above, or a leaf when there is none.
    fn call(&mut self, d: usize) -> String {
        if self.helpers.is_empty() {
            return self.int_leaf();
        }
        let which = self.pick(self.helpers.len());
        let helper = self.helpers[which].clone();
        let second = self.int(d);
        let first = if helper.recursive && !self.chance(1, 8) {
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

    fn bool(&mut self, depth: usize) -> String {
        if depth >= MAX_DEPTH {
            return self.bool_leaf();
        }
        let d = depth + 1;
        match self.pick(12) {
            0 | 1 => self.bool_leaf(),
            2..=4 => {
                let op = ["<", "<=", ">", ">=", "==", "!="][self.pick(6)];
                let a = self.int(d);
                let b = self.int(d);
                format!("({a} {op} {b})")
            }
            5 => {
                let op = ["==", "!="][self.pick(2)];
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
        match self.pick(18) {
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
                        .filter(|l| l.ty != Ty::Sb)
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
                let w = match self.pick(8) {
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
        match self.pick(12) {
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
                // A comparison that may contradict itself is allowed: the
                // run gets some permutation and does not stop.
                let body = if self.chance(3, 4) {
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
                let (v, _) = self.fresh_vector();
                let v = if v == "Vector.of()" {
                    "Vector.of(0)".to_string()
                } else {
                    v
                };
                format!("({v}).toArray()")
            }
            _ => self.array_atom(),
        }
    }
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
