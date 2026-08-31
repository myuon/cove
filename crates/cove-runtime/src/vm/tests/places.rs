//! `var` places: what a place names, how far it reaches, and what is left
//! standing when a path through a call is abandoned.
//!
//! Assigning to a `let` binding, and to a field of one, used to be two
//! tests here: the interpreter refused the write when it happened and the
//! lowering refused the program before the VM was handed anything, and
//! what the two said was the point. ADR 0021 made both a check-time
//! error, so neither program reaches a backend at all and there is
//! nothing here for the two to agree about. `cove-sema`'s
//! `rejects_an_assignment_to_a_let_binding` and its neighbours are where
//! the rule is pinned now, and the two helpers that drove one backend at a
//! time — `not_lowered` and `only_interpreted` — went with them, because
//! everything left in this file runs both.

use super::*;

/// A `var` binding is still written, on both backends.
///
/// The refusal above is about a read-only place and not about assignment,
/// and this is what says so.
#[test]
fn writing_a_var_binding_is_performed_by_both_backends() {
    assert_eq!(
        agree("export fn main() -> Int {\n  var x = 1\n  x = 2\n  x\n}\n").value(),
        "Int(2)"
    );
}

/// A method name a builtin type and a declared type both answer to is
/// resolved by the receiver's type, which is the only thing that decides
/// it.
///
/// The interpreter tries a declared method of the receiver's *runtime*
/// type first and falls back to the builtin table, so which applies is a
/// fact about the receiver — and this used to be refused, because the
/// lowering had no type table and `[1, 2, 3].length()` answering the
/// builtin's `3` and a `Call` to the declared `Box.length` are two
/// different programs. The checker settles which, so this asserts what
/// the refusal used to protect: the array reaches the builtin with a
/// `Box.length` declared in the same program.
///
/// `a_declared_method_a_builtin_also_names_lowers_and_agrees` is the
/// same fact read from the other side, with both calls written together.
/// **A `var` parameter is an alias, not a copy that is written back.**
///
/// `two(var x, var x)` is the case that separates the two, and the oracle
/// answers 11: both parameters name the same cell, so the second
/// parameter's `+= 10` is applied to what the first one's `+= 1` left.
/// Copy-in/copy-out would answer 10, because the second write-back would
/// overwrite the first with a value read before it happened. There is no
/// design here that answers 10.
#[test]
fn two_var_parameters_naming_one_binding_are_one_binding() {
    assert_eq!(
        agree(
            "fn two(var a: Int, var b: Int) {\n  a += 1\n  b += 10\n}\n\nexport fn main() -> Int {\n  var x = 0\n  two(var x, var x)\n  x\n}\n"
        )
        .value(),
        "Int(11)"
    );
}

/// A place can carry a field path, so `var c.hits` names one field of the
/// caller's struct and leaves the rest of it alone.
#[test]
fn a_var_argument_can_name_a_field_of_the_callers_struct() {
    assert_eq!(
        agree(
            "struct C {\n  hits: Int\n  other: Int\n}\n\nfn bump(var n: Int) {\n  n += 1\n}\n\nexport fn main() -> String {\n  var c = C(hits: 0, other: 0)\n  bump(var c.hits)\n  \"{c}\"\n}\n"
        )
        .value(),
        "Str(\"C(hits: 1, other: 0)\")"
    );
}

/// A place is forwarded through a call: a `var` parameter passed on as a
/// `var` argument aliases the original binding rather than the
/// parameter's own slot, which is what `load-place` alone does.
#[test]
fn a_var_parameter_passed_on_still_names_the_original_binding() {
    assert_eq!(
        agree(
            "fn bump(var n: Int) {\n  n += 1\n}\n\nfn forward(var n: Int) {\n  bump(var n)\n}\n\nexport fn main() -> Int {\n  var x = 5\n  forward(var x)\n  x\n}\n"
        )
        .value(),
        "Int(6)"
    );
}

/// And it forwards with a step added on the way: the incoming place
/// already has a path, and a field is appended to it at run time.
#[test]
fn a_place_forwarded_with_a_field_appended_reaches_two_deep() {
    assert_eq!(
        agree(
            "struct Inner {\n  hits: Int\n}\n\nstruct Outer {\n  inner: Inner\n}\n\nfn bump(var n: Int) {\n  n += 1\n}\n\nfn deep(var o: Outer) {\n  bump(var o.inner.hits)\n}\n\nexport fn main() -> String {\n  var o = Outer(inner: Inner(hits: 1))\n  deep(var o)\n  \"{o}\"\n}\n"
        )
        .value(),
        "Str(\"Outer(inner: Inner(hits: 2))\")"
    );
}

/// A `var self` receiver is a place like any other, and a method that
/// takes one writes into the caller's binding.
#[test]
fn a_var_self_method_writes_into_the_callers_binding() {
    assert_eq!(
        agree(
            "struct Counter {\n  hits: Int\n}\n\nimpl Counter {\n  fn hit(var self) {\n    self.hits += 1\n  }\n\n  fn hitMany(var self, n: Int) -> Int {\n    for _step in 0..<n {\n      self.hits += 1\n    }\n    self.hits\n  }\n}\n\nexport fn main() -> String {\n  var counter = Counter(hits: 0)\n  counter.hit()\n  let total = counter.hitMany(3)\n  \"{counter} {total}\"\n}\n"
        )
        .value(),
        "Str(\"Counter(hits: 4) 4\")"
    );
}

/// Writing a field through a place makes the struct private again at the
/// step it is written, which is what keeps a copy of a struct a copy.
/// The same program without that call answers with both documents
/// renamed.
#[test]
fn writing_through_a_place_leaves_a_copy_of_the_struct_alone() {
    assert_eq!(
        agree(
            "struct Meta {\n  version: Int\n}\n\nstruct Document {\n  title: String\n  meta: Meta\n}\n\nimpl Document {\n  fn rename(var self, title: String) {\n    self.title = title\n  }\n\n  fn bumpVersion(var self) {\n    self.meta.version += 1\n  }\n}\n\nexport fn main() -> String {\n  var original = Document(title: \"first\", meta: Meta(version: 1))\n  var copy = original\n  copy.rename(\"second\")\n  copy.bumpVersion()\n  \"{original} {copy}\"\n}\n"
        )
        .value(),
        "Str(\"Document(title: first, meta: Meta(version: 1)) Document(title: second, meta: Meta(version: 2))\")"
    );
}

/// `freeze` consumes uniquely owned storage, so it has to see the
/// caller's own handle exactly once — which is what taking the place
/// rather than a read of it arranges. Both backends answer the array,
/// and both refuse when a second alias exists.
#[test]
fn freeze_through_a_place_consumes_the_storage_it_names() {
    assert_eq!(
        agree(
            "export fn main() -> String {\n  var v = Vector.of(\"a\", \"b\")\n  let frozen = v.freeze()\n  \"{frozen}\"\n}\n"
        )
        .value(),
        "Str(\"[a, b]\")"
    );
    let aliased = agree(
        "export fn main() -> String {\n  var v = Vector.of(\"a\")\n  let other = v\n  let frozen = v.freeze()\n  \"{frozen}\"\n}\n",
    );
    assert!(
        aliased.error().message.contains("uniquely owned"),
        "{}",
        aliased.error().message
    );
}

/// A `break` written inside a call that has already pushed a place has
/// to take the place with it, or the loop's exit is reached at a depth
/// nothing else reaches it at. That is what `place-pop` is for, and
/// `validate` is what would have caught its absence.
#[test]
fn a_break_inside_a_half_built_call_leaves_no_place_standing() {
    assert_eq!(
        agree(
            "fn add(var n: Int, by: Int) {\n  n += by\n}\n\nexport fn main() -> Int {\n  var total = 0\n  var i = 0\n  while i < 10 {\n    add(var total, by: if i == 3 {\n      break\n    } else {\n      i\n    })\n    i += 1\n  }\n  total\n}\n"
        )
        .value(),
        "Int(3)"
    );
}

/// A `var` parameter whose type the checker settled as `Int` names the
/// scalar slot the binding already lived in: nothing moves to be named.
///
/// This test recorded the opposite until issue #162. A place could address
/// only the value stack, so `total` — and, because the pre-pass collected
/// *names*, every binding of that name in the body — was kept there for the
/// whole body, and its every read and write crossed. The listing then read
/// `const Int(1)` / `store 0` / `place 0` and `load 0` / `value-to-scalar`
/// where it now reads the scalar forms, and `counted` beside it was the
/// control that said only the rooted name had moved. Now neither has.
#[test]
fn a_binding_a_place_is_rooted_at_keeps_its_scalar_slot() {
    let listed = main_of(
        "fn bump(var n: Int) {\n  n += 1\n}\n\nexport fn main() -> Int {\n  var total = 1\n  var counted = 2\n  bump(var total)\n  total + counted\n}\n",
    );
    // Both are scalar slots, and `place-scalar` names the first of them
    // where it stands.
    assert_eq!(
        listed,
        "fn m.main arity=0 frame=2/0 -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  scalar-const 2\n\
         \x20  3  store-scalar 1\n\
         \x20  4  place-scalar 0 Int\n\
         \x20  5  call m.bump argc=0/0/1\n\
         \x20  6  pop\n\
         \x20  7  load-scalar 0\n\
         \x20  8  load-scalar 1\n\
         \x20  9  int Add\n\
         \x20 10  return-scalar\n"
    );
}

/// A `Bool` binding is rooted the same way, and the tag comes back on.
///
/// The scalar stack keeps no tag, so a place rooted at one of its slots
/// carries which of the two words it names — `place-scalar 0 Bool` — and a
/// read through it puts `Value::Bool` back rather than `Value::Int`. This is
/// the test that would fail if the tag travelled wrongly, because the two
/// renderings differ.
#[test]
fn a_var_argument_rooted_at_a_bool_reads_and_writes_as_a_bool() {
    assert_eq!(
        agree(
            "fn flip(var b: Bool) -> Bool {\n  let was = b\n  b = !b\n  was\n}\n\nexport fn main() -> String {\n  var on = true\n  let was = flip(var on)\n  \"{was} {on}\"\n}\n"
        )
        .value(),
        "Str(\"true false\")"
    );
}

/// A scalar binding rooted for a `var` argument is still the same binding
/// the loop around it reads and writes.
///
/// This is `benches/convention`'s `conv_var` shape as a correctness test:
/// one `bump(var total)` written after the loop, and a loop body that reads
/// and writes `total` on the scalar stack throughout. It used to be the
/// program that proved the *opposite* — that the binding had moved — and
/// what it proves now is that moving it was never necessary for it to be
/// right.
#[test]
fn a_scalar_local_rooted_after_a_loop_is_the_binding_the_loop_wrote() {
    assert_eq!(
        agree(
            "fn bump(var n: Int) {\n  n += 1\n}\n\nexport fn main() -> Int {\n  var total = 0\n  var i = 0\n  while i < 10 {\n    total += i\n    i += 1\n  }\n  bump(var total)\n  total\n}\n"
        )
        .value(),
        "Int(46)"
    );
}

/// The over-approximation the pre-pass performed is gone with it, and this
/// is the program that used to pay for it.
///
/// `var_argument_roots` collected *names*, so a `bump(var total)` anywhere
/// in a body demoted every binding called `total` the body declared,
/// including the one in a block no place ever names. Both are scalar slots
/// now, and the answer is the same either way — which is why the cost was
/// only ever a cost.
#[test]
fn a_shadowing_binding_of_a_rooted_name_is_not_demoted_with_it() {
    assert_eq!(
        agree(
            "fn bump(var n: Int) {\n  n += 1\n}\n\nexport fn main() -> Int {\n  var total = 0\n  if true {\n    var total = 100\n    total += 1\n  }\n  bump(var total)\n  total\n}\n"
        )
        .value(),
        "Int(1)"
    );
}

/// A closure over a `var` parameter of a settled scalar type still captures
/// the *value* the place names, and the place it reads through is rooted at
/// a scalar slot.
///
/// `Inst::PlaceLocal`'s note is the statement being tested: a closure
/// captures what a place names and never the place, so the answer does not
/// change when the binding is assigned to after the closure is written.
/// Nothing about that depended on which stack the binding lived in, and this
/// is what says so now that it can live in either.
#[test]
fn a_closure_over_a_scalar_var_parameter_captures_the_value_it_named() {
    assert_eq!(
        agree(
            "fn watch(var n: Int) -> Int {\n  let f: fn() -> Int = fn() {\n    n\n  }\n  n += 100\n  f()\n}\n\nexport fn main() -> Int {\n  var x = 7\n  let seen = watch(var x)\n  seen + x\n}\n"
        )
        .value(),
        "Int(114)"
    );
}

/// `snapshot` on a `Vector` allocates storage of its own, and both
/// backends see the copy stop sharing.
///
/// The observable difference between an ordinary copy and a snapshot is
/// exactly this: pushing onto one is seen by the other or it is not.
#[test]
fn snapshot_of_a_vector_allocates_storage_the_original_does_not_share() {
    assert_eq!(
        agree(
            "export fn main() -> String {\n  var original = Vector.of(1)\n  var alias = original\n  var copy = original.snapshot()\n  alias.push(2)\n  copy.push(3)\n  \"{original.length()} {alias.length()} {copy.length()}\"\n}\n"
        )
        .value(),
        "Str(\"2 2 2\")"
    );
}

/// A struct's own conformance is reached through the `Call` the checker
/// recorded, and the builtin half through the instruction.
#[test]
fn snapshot_dispatches_to_a_conformance_and_falls_back_to_the_instruction() {
    let source = "struct B {\n  guests: Vector<String>\n}\n\nimpl Snapshot for B {\n  fn snapshot(self) -> B {\n    B(guests: self.guests.snapshot())\n  }\n}\n\nexport fn main() -> String {\n  var original = B(guests: Vector.of(\"a\"))\n  var copy = original.snapshot()\n  copy.guests.push(\"b\")\n  \"{original.guests.length()} {copy.guests.length()} {5.snapshot()} {[1, 2].snapshot()}\"\n}\n";
    assert_eq!(agree(source).value(), "Str(\"1 2 5 [1, 2]\")");
    let listed = main_of(source);
    assert!(
        listed
            .lines()
            .any(|line| line.contains("call m.B.snapshot")),
        "the struct's conformance is a call:\n{listed}"
    );
    assert!(
        listed
            .lines()
            .any(|line| line.trim().ends_with("snapshot") && !line.contains("call")),
        "the builtin half is the instruction:\n{listed}"
    );
}
