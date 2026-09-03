//! `value.snapshot()`, the one method of the builtin `Snapshot` trait.
//!
//! `crates/cove-runtime/src/builtins.rs` is the specification and it has two
//! answers for a value no declared conformance speaks for: an immutable value
//! answers itself, and a `Vector` answers a new vector. Neither is a builtin
//! here — `cove_runtime::vm::builtins` has no `snapshot` arm — because the
//! first is a copy the instruction set already makes and the second is a walk
//! that may end in a call, which `docs/LINEAR_VM.md` puts in the lowering
//! rather than in a builtin.

use super::{listing, refused};

/// An immutable value answers itself, and "itself" is a copy into a location
/// of this expression's own rather than the receiver's location handed back.
/// A borrowed location would be an alias of the binding, and
/// `f(a.snapshot(), g())` would then hand the call whatever `g` left in `a`.
#[test]
fn an_immutable_value_answers_a_copy_of_its_own_words() {
    assert_eq!(
        listing("fn n(x: Int) -> Int { x.snapshot() }", "n"),
        "\
fn0 m.n(Int) -> Int
  frame 3: s0!:int s1:int s2:int
  local x -> s0:Int [0, 3)
     0  copy s2:int s0:int Int
     1  copy s1:int s2:int Int
     2  return s1:int
"
    );
}

/// An `Array` is in that first answer whatever it holds, which is the
/// oracle's own reading and not a shortcut: each of `Array`, `Map` and `Set`
/// is immutable, so an element that shares storage with something else went
/// on sharing it before the call and there is nothing for a copy to separate.
#[test]
fn an_array_answers_itself_rather_than_being_walked() {
    assert_eq!(
        listing("fn a(xs: Array<Int>) -> Array<Int> { xs.snapshot() }", "a"),
        "\
fn0 m.a(Array) -> Array
  frame 3: s0!:ref s1:ref s2:ref
  local xs -> s0:Array [0, 4)
     0  copy s2:ref s0:ref Array
     1  copy s1:ref s2:ref Array
     2  clear s2:ref Array
     3  return s1:ref
"
    );
}

/// A `Vector` is the one storage a copy is observable of, so it answers a new
/// vector. Where every element answers itself, the new storage is the old
/// words in a store of their own: `Vector.toArray` clones them out and
/// `Array.toVector` allocates the vector around them, which is the oracle's
/// `allocate_vector(snapshotted)` for the case where snapshotting an element
/// is the identity.
#[test]
fn a_vector_of_immutable_elements_is_copied_out_and_back() {
    assert_eq!(
        listing(
            "fn v(xs: Vector<String>) -> Vector<String> { xs.snapshot() }",
            "v"
        ),
        "\
fn0 m.v(Vector) -> Vector
  frame 4: s0!:ref s1:ref s2:ref s3:ref
  local xs -> s0:Vector [0, 6)
     0  call-builtin s2:ref Vector.toArray (s0:Vector)
     1  call-builtin s3:ref Array.toVector (s2:Array)
     2  clear s2:ref Array
     3  copy s1:ref s3:ref Vector
     4  clear s3:ref Vector
     5  return s1:ref
"
    );
}

/// An element with a graph of its own has to be snapshotted one at a time,
/// and one of the ways it answers is a call to the conformance its type
/// declared. That is a walk this lowering has not been taught, so it is named
/// rather than guessed at — and naming the element type is what says which
/// walk is missing.
#[test]
fn a_vector_whose_elements_need_snapshots_of_their_own_is_named() {
    assert_eq!(
        refused("struct P { n: Int }\nfn v(xs: Vector<P>) -> Vector<P> { xs.snapshot() }"),
        [
            "not yet lowered: `Vector.snapshot` of a `P`, whose elements each need a snapshot of \
             their own"
        ]
    );
}
