//! `Shared(value)` and `lock`.
//!
//! `cove_ir::lower::cells` is the prose; these are the listings.

use super::listing;

/// A cell is an allocation and a store, and there is no instruction for it.
///
/// The allocation zeroes the payload, which is what says "no task holds this
/// cell", and the value goes into payload word 1 at its own width. An
/// instruction meaning *those two* would be a third spelling of something the
/// IR can already express twice.
#[test]
fn a_cell_is_an_allocation_and_a_store() {
    assert_eq!(
        listing("fn make() -> Shared<Int> { Shared(1) }", "make"),
        "\
fn0 m.make() -> Shared
  frame 3: s0:ref s1:int s2:ref
     0  int s1:int 1
     1  alloc s2:ref Shared<shared>
     2  store-field s2:ref +1 s1:int Int
     3  copy s0:ref s2:ref Shared
     4  clear s2:ref Shared
     5  return s0:ref Shared
"
    );
}

/// `lock` is acquire, an ordinary `call-closure`, and release.
///
/// The shape `docs/LINEAR_VM.md` asks for, and the reason is the one it gives
/// for `map`: **a builtin never calls back into Cove**, so the call is a frame
/// like any other and the two instructions around it are what hold the cell
/// for the length of it.
///
/// Three things about the order are the point. The closure is built *before*
/// the cell is taken, which is where `Interpreter::call_shared_method`
/// evaluates it and what leaves the held region with nothing in it that can
/// jump. The address is cleared before the release, because an address into an
/// object is live for exactly as long as the lock that made it safe to hold.
/// And the release is on the path that finished — the other path is a runtime
/// error, which is not a jump this crate can emit and is the machine's to
/// answer for every cell the task was holding.
#[test]
fn a_lock_is_acquire_call_release() {
    assert_eq!(
        listing(
            "fn bump(cell: Shared<Int>) -> Int {\n  cell.lock(fn(var value) {\n    value = value + 1\n    value\n  })\n}",
            "bump"
        ),
        "\
fn0 m.bump(Shared) -> Int
  frame 5: s0!:ref s1:int s2:ref s3:int s4:addr
  local cell -> s0:Shared [0, 11)
     0  alloc s2:ref closure m.bump#0<closure>
     1  int s3:int 1
     2  store-field s2:ref +0 s3:int Int
     3  shared.lock s0:ref
     4  addr-of-field s4:addr s0:ref +1
     5  call-closure s3:int s2:ref (s4:<addr>)
     6  clear s4:addr <addr>
     7  shared.unlock s0:ref
     8  clear s2:ref fn
     9  copy s1:int s3:int Int
    10  return s1:int Int
"
    );
}

/// The closure's `var` parameter is an `addr` slot, so the body writes where
/// the value lies.
///
/// This is the one lambda in the language whose first parameter may be written
/// `var`, and the whole of what that costs is this frame: slot 0 is an address
/// rather than an `Int`, so a read is a `load` through it and a write is a
/// `store` through it — the same two instructions a declared `var` parameter's
/// body already uses. Nothing is copied in, and nothing is copied back out.
#[test]
fn the_lock_closures_var_parameter_is_an_address() {
    assert_eq!(
        listing(
            "fn bump(cell: Shared<Int>) -> Int {\n  cell.lock(fn(var value) {\n    value = value + 1\n    value\n  })\n}",
            "bump#0"
        ),
        "\
fn1 m.bump#0(<addr>) -> Int
  frame 5: s0!:addr s1:int s2:int s3:int s4:int
  local value -> s0:<addr> [0, 7)
     0  load s2:int s0:addr Int
     1  int s3:int 1
     2  add.int s4:int s2:int s3:int
     3  store s0:addr s4:int Int
     4  load s4:int s0:addr Int
     5  copy s1:int s4:int Int
     6  return s1:int Int
"
    );
}

/// A closure that did not write `var` is handed a copy, and nothing is stored
/// back.
///
/// The `load` between the address and the call is the whole difference, and it
/// is the oracle's: `Interpreter::call_shared_method` reads
/// `params.first().is_var` off the written lambda and passes an
/// `ArgSlot::Alias` or an `ArgSlot::Value` accordingly — and on the second
/// path the value it stores back into the cell afterwards is the place it
/// made, which the closure never touched.
#[test]
fn a_closure_without_var_is_handed_a_copy() {
    assert_eq!(
        listing(
            "fn read(cell: Shared<Int>) -> Int {\n  cell.lock(fn(value) { value })\n}",
            "read"
        ),
        "\
fn0 m.read(Shared) -> Int
  frame 6: s0!:ref s1:int s2:ref s3:int s4:addr s5:int
  local cell -> s0:Shared [0, 12)
     0  alloc s2:ref closure m.read#0<closure>
     1  int s3:int 1
     2  store-field s2:ref +0 s3:int Int
     3  shared.lock s0:ref
     4  addr-of-field s4:addr s0:ref +1
     5  load s3:int s4:addr Int
     6  call-closure s5:int s2:ref (s3:Int)
     7  clear s4:addr <addr>
     8  shared.unlock s0:ref
     9  clear s2:ref fn
    10  copy s1:int s5:int Int
    11  return s1:int Int
"
    );
}

/// A cell wrapping a struct is the lock word and then the struct's fields,
/// inline.
///
/// One layout per wrapped-value layout, and the address the closure is handed
/// is the address of the *first* word of that value — so a field of it is an
/// `addr-of-part`, which is the same arithmetic a field of an inline struct is
/// done to an address instead of to a slot number. Nothing loads the whole
/// `Metrics` to write one of its words.
#[test]
fn a_cell_wrapping_a_struct_holds_its_fields_inline() {
    assert_eq!(
        listing(
            "struct Metrics {\n  requests: Int\n  failures: Int\n}\n\nfn count(cell: Shared<Metrics>) -> Int {\n  cell.lock(fn(var value) {\n    value.failures = value.failures + 1\n    value.failures\n  })\n}",
            "count#0"
        ),
        "\
fn1 m.count#0(<addr>) -> Int
  frame 6: s0!:addr s1:int s2:addr s3:int s4:int s5:int
  local value -> s0:<addr> [0, 13)
     0  addr-of-part s2:addr s0:addr +1
     1  load s3:int s2:addr Int
     2  clear s2:addr <addr>
     3  int s4:int 1
     4  add.int s5:int s3:int s4:int
     5  addr-of-part s2:addr s0:addr +1
     6  store s2:addr s5:int Int
     7  clear s2:addr <addr>
     8  addr-of-part s2:addr s0:addr +1
     9  load s5:int s2:addr Int
    10  clear s2:addr <addr>
    11  copy s1:int s5:int Int
    12  return s1:int Int
"
    );
}
