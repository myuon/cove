//! What the encoder does to real lowerings, end to end.
//!
//! The cases in `crate::bytecode` are about the format: every opcode, every
//! boundary value, and bytes no encoder produced. This is the other half —
//! programs the compiler actually lowers, encoded, verified, and read back —
//! because a format that round-trips its own samples and not its own compiler
//! is a format that has been tested against itself.
//!
//! Three claims, made about every function of every program below:
//!
//! - **the encoder is total.** ADR 0041's audit covers all forty-nine
//!   variants, so there is no instruction in any of these that it refuses;
//! - **the encoding is 1:1 and lossless.** `decode(encode(inst)) == inst` at
//!   every program counter, so bytecode pc is IR pc and the debugger's mapping
//!   is the identity;
//! - **a lowering the compiler's own verifier accepts, the byte verifier
//!   accepts too.** The two check different things — one a lowering, one bytes
//!   — and they must not disagree about a program that is well formed.

use std::collections::BTreeSet;

use cove_schema::HostSchemas;

use super::{checked, checked_with};
use crate::bytecode::{decode, encode, encode_program, op::Op, verify};
use crate::lower::lower;
use crate::program::Program;
use crate::Pc;

/// A loop, a branch, arithmetic and an immediate.
const CONTROL: &str = "\
fn total(n: Int) -> Int {
  var sum = 0
  var i = 0
  while i < n {
    if i % 2 == 0 {
      sum = sum + i * 3
    } else {
      sum = sum - 1
    }
    i = i + 1
  }
  sum
}";

/// Inline structs, fields of them, and a value wider than one word crossing a
/// call boundary.
const STRUCTS: &str = "\
struct Point { x: Int, y: Int }
struct Line { from: Point, to: Point }

fn length(l: Line) -> Int {
  (l.to.x - l.from.x) + (l.to.y - l.from.y)
}

fn make() -> Int {
  let l = Line(from: Point(x: 1, y: 2), to: Point(x: 4, y: 6))
  length(l)
}";

/// Heap objects: an array, a string, a loop over elements, and the builtins
/// over both.
const HEAP: &str = "\
fn words(s: String) -> Int {
  let xs = [1, 2, 3]
  var total = 0
  for x in xs {
    total = total + x
  }
  total + s.length()
}";

/// An enum and a `match`, which is a switch over a table, plus the `trap` the
/// lowering leaves for a value no arm covers.
const ENUMS: &str = "\
enum Shape { Dot, Line(Int), Box(Int, Int) }

fn area(s: Shape) -> Int {
  match s {
    Shape.Dot => 0,
    Shape.Line(a) => a,
    Shape.Box(a, b) => a * b,
  }
}";

/// A closure, its captures, and a call through one.
const CLOSURES: &str = "\
fn apply(g: fn(Int) -> Int, n: Int) -> Int { g(n) }

fn twice(n: Int) -> Int {
  let scale = n
  apply(fn(x: Int) { x * scale }, 2)
}";

/// A scope, a spawn, an await and a cancel — the widest task instructions.
const TASKS: &str = "\
fn go() -> Result<Int, Error> {
  scope tasks {
    let t = tasks.spawn { 1 }
    let u = tasks.spawn { 2 }
    u.cancel()
    Ok(await t)
  }
}";

/// A `Result` and a `?`, which is the shape that produces the enum payload
/// reads and the early return.
const RESULTS: &str = "\
fn half(n: Int) -> Result<Int, Error> {
  if n % 2 == 0 {
    Ok(n / 2)
  } else {
    Err(Error(\"odd\"))
  }
}

fn quarter(n: Int) -> Result<Int, Error> {
  let once = half(n)?
  half(once)
}";

/// An assertion, which is lowered rather than performed and is the only place
/// `assert.failed` comes from.
const ASSERTIONS: &str = "\
fn check(n: Int) -> Result<Unit, Error> {
  assertEqual(n, 3)
}";

/// A host call across the boundary, against the modules the toolchain ships.
const HOSTS: &str = "\
use console
use files

fn say() -> Result<Unit, Error> {
  console.println(\"hello\")
}

fn read(path: String) -> Result<Option<String>, Error> {
  let f = files.open(path)?
  f.readLine()
}";

fn shipped() -> HostSchemas {
    let mut schemas = HostSchemas::new();
    for module in cove_schema::hosts::shipped() {
        schemas = schemas.with(*module);
    }
    schemas
}

/// Every program above, lowered.
fn programs() -> Vec<(&'static str, Program)> {
    let mut held = Vec::new();
    for (name, source) in [
        ("control", CONTROL),
        ("structs", STRUCTS),
        ("heap", HEAP),
        ("enums", ENUMS),
        ("closures", CLOSURES),
        ("tasks", TASKS),
        ("results", RESULTS),
        ("assertions", ASSERTIONS),
    ] {
        let (sources, program) = checked(source);
        held.push((
            name,
            lower(&program, &sources, &HostSchemas::new()).expect("the program lowers"),
        ));
    }
    let schemas = shipped();
    let (sources, program) = checked_with(HOSTS, &schemas);
    held.push((
        "hosts",
        lower(&program, &sources, &schemas).expect("the program lowers"),
    ));
    held
}

/// Nothing the compiler lowers is an instruction the encoder cannot write
/// down, and no program's slots are wider than the format's fields.
#[test]
fn every_instruction_the_compiler_lowers_encodes() {
    for (name, program) in programs() {
        let encoded = encode_program(&program)
            .unwrap_or_else(|why| panic!("`{name}` does not encode: {why}"));
        assert_eq!(encoded.functions.len(), program.functions.len());
        for (index, function) in program.functions.iter().enumerate() {
            assert_eq!(
                encoded.functions[index].len(),
                function.code.len(),
                "`{name}`'s {} is a different length encoded",
                function.qualified()
            );
        }
    }
}

/// The encoding is a genuine inverse over everything the compiler produces,
/// not only over the samples the format's own tests build.
#[test]
fn every_encoded_instruction_decodes_back_to_the_one_it_came_from() {
    for (name, program) in programs() {
        let encoded = encode_program(&program).expect("the program encodes");
        for (index, function) in program.functions.iter().enumerate() {
            for (pc, inst) in function.code.iter().enumerate() {
                let bytes = encoded.functions[index][pc];
                assert_eq!(
                    decode(bytes, pc as Pc).as_ref(),
                    Ok(inst),
                    "`{name}` {}+{pc}",
                    function.qualified()
                );
                assert_eq!(
                    encode(inst, pc as Pc),
                    Ok(bytes),
                    "`{name}` {}+{pc}",
                    function.qualified()
                );
            }
        }
    }
}

/// The byte verifier agrees with the lowering's own verifier about every
/// program the compiler produces.
///
/// They check different things — one a lowering, one bytes — and a program
/// `crate::verify` accepts is one this must accept too, or the boundary would
/// refuse programs the compiler is entitled to run. `lower` has already run
/// `crate::verify` over each of these, so reaching here is half the claim and
/// the assertion is the other half.
#[test]
fn a_lowering_the_compiler_accepts_verifies_as_bytes() {
    for (name, program) in programs() {
        let encoded = encode_program(&program).expect("the program encodes");
        assert_eq!(verify(&program, &encoded), Ok(()), "`{name}`");
    }
}

/// These programs between them reach most of the instruction set, which is
/// what makes the three claims above worth making.
///
/// Thirty-eight of the hundred, which is a floor rather than a target: an
/// opcode is
/// reached because a language construct lowers to it, and a lowering that
/// stopped emitting a whole family would fail here.
#[test]
fn the_programs_between_them_reach_most_of_the_instruction_set() {
    let mut reached: BTreeSet<u8> = BTreeSet::new();
    for (_, program) in programs() {
        let encoded = encode_program(&program).expect("the program encodes");
        for code in &encoded.functions {
            reached.extend(code.iter().map(|held| held.opcode()));
        }
    }
    assert!(
        reached.len() >= 38,
        "only {} of the {} opcodes are reached",
        reached.len(),
        Op::all().len()
    );
    // Every byte that turned up names an opcode, which is the encoder's side
    // of what the verifier checks.
    for byte in &reached {
        assert!(Op::from_number(*byte).is_some(), "{byte} names nothing");
    }
}
