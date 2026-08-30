use super::*;

// ------------------------------------------------------------- places

/// `two(var x, var x)` is what a place has to be an alias for: both
/// arguments are the same `place-scalar 0 Int`, so the callee's two
/// parameters name one slot of the caller's frame and the second write sees
/// the first.
///
/// `x` stays a scalar slot, which is the whole of issue #162's first cliff.
/// This listing used to read `const Int(0)` / `store 0` / `place 0` and end
/// in `load 0` / `value-to-scalar`, because a place could only address the
/// value stack and so the binding a place was rooted at had to be kept
/// there. Nothing moves now: the place names the slot where it is.
#[test]
fn two_var_arguments_naming_one_binding_push_the_same_place_twice() {
    assert_eq!(
        listing(
            "fn two(var a: Int, var b: Int) {\n  a += 1\n  b += 10\n}\n\nfn f() -> Int {\n  var x = 0\n  two(var x, var x)\n  x\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/1 -> Int\n\
         \x20  0  scalar-const 0\n\
         \x20  1  store-scalar 0\n\
         \x20  2  place-scalar 0 Int\n\
         \x20  3  place-scalar 0 Int\n\
         \x20  4  call m.two argc=0/0/2\n\
         \x20  5  pop\n\
         \x20  6  load-scalar 0\n\
         \x20  7  return-scalar\n"
    );
    assert_eq!(
        listing(
            "fn two(var a: Int, var b: Int) {\n  a += 1\n  b += 10\n}\n\nfn f() -> Int {\n  var x = 0\n  two(var x, var x)\n  x\n}\n",
            "two"
        ),
        "fn m.two arity=2 frame=0/0/2 params=[place, place] -> value\n\
         \x20  0  load-place 0\n\
         \x20  1  load-place 0\n\
         \x20  2  place-read\n\
         \x20  3  value-to-scalar\n\
         \x20  4  scalar-const 1\n\
         \x20  5  int Add\n\
         \x20  6  scalar-to-value Int\n\
         \x20  7  place-write\n\
         \x20  8  load-place 1\n\
         \x20  9  load-place 1\n\
         \x20 10  place-read\n\
         \x20 11  value-to-scalar\n\
         \x20 12  scalar-const 10\n\
         \x20 13  int Add\n\
         \x20 14  scalar-to-value Int\n\
         \x20 15  place-write\n\
         \x20 16  const Unit\n\
         \x20 17  return\n"
    );
}

/// A `var self` receiver is a place argument, and the body writes
/// through it: `self.hits += 1` is the place built twice, read through
/// once, and written through once.
#[test]
fn a_var_self_receiver_is_a_place_argument() {
    let source = "struct Counter {\n  hits: Int\n}\n\nimpl Counter {\n  fn hit(var self) {\n    self.hits += 1\n  }\n}\n\nfn f() -> Int {\n  var c = Counter(hits: 0)\n  c.hit()\n  c.hits\n}\n";
    assert_eq!(
        listing(source, "Counter.hit"),
        "fn m.Counter.hit arity=1 frame=0/0/1 params=[place] receiver -> value\n\
         \x20  0  load-place 0\n\
         \x20  1  place-field 0\n\
         \x20  2  load-place 0\n\
         \x20  3  place-field 0\n\
         \x20  4  place-read\n\
         \x20  5  value-to-scalar\n\
         \x20  6  scalar-const 1\n\
         \x20  7  int Add\n\
         \x20  8  scalar-to-value Int\n\
         \x20  9  place-write\n\
         \x20 10  const Unit\n\
         \x20 11  return\n"
    );
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=0 frame=1/0 -> Int\n\
         \x20  0  const Int(0)\n\
         \x20  1  make-struct m.Counter fields=hits\n\
         \x20  2  store 0\n\
         \x20  3  place 0\n\
         \x20  4  call m.Counter.hit argc=0/0/1\n\
         \x20  5  pop\n\
         \x20  6  load 0\n\
         \x20  7  get-field-at-scalar 0\n\
         \x20  8  return-scalar\n"
    );
}

/// A `var` parameter passed on as a `var` argument is one `load-place`
/// and nothing else, which is what makes the callee's callee alias the
/// original binding rather than this frame's slot.
#[test]
fn a_forwarded_var_argument_is_the_place_the_parameter_holds() {
    assert_eq!(
        listing(
            "fn bump(var n: Int) {\n  n += 1\n}\n\nfn forward(var n: Int) {\n  bump(var n)\n}\n",
            "forward"
        ),
        "fn m.forward arity=1 frame=0/0/1 params=[place] -> value\n\
         \x20  0  load-place 0\n\
         \x20  1  call m.bump argc=0/0/1\n\
         \x20  2  return\n"
    );
}

/// A path deeper than one field is written through a place even where
/// the root is an ordinary local: the whole-value update
/// `Body::assign_field` performs replaces a local's struct, and it has
/// nowhere to put the intermediate one back.
#[test]
fn a_field_two_deep_is_written_through_a_place() {
    assert_eq!(
        listing(
            "struct Inner {\n  n: Int\n}\n\nstruct Outer {\n  inner: Inner\n}\n\nfn f() -> Int {\n  var o = Outer(inner: Inner(n: 1))\n  o.inner.n = 2\n  o.inner.n\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=1/0 -> Int\n\
         \x20  0  const Int(1)\n\
         \x20  1  make-struct m.Inner fields=n\n\
         \x20  2  make-struct m.Outer fields=inner\n\
         \x20  3  store 0\n\
         \x20  4  place 0\n\
         \x20  5  place-field 0\n\
         \x20  6  place-field 0\n\
         \x20  7  const Int(2)\n\
         \x20  8  place-write\n\
         \x20  9  load 0\n\
         \x20 10  get-field-at 0\n\
         \x20 11  get-field-at-scalar 0\n\
         \x20 12  return-scalar\n"
    );
}

/// `startup` is not one of the eight, and it lowers too: it is the
/// smallest function the package has, and it is what a frame of nothing
/// looks like.
#[test]
fn the_smallest_entry_is_a_unit_and_a_return() {
    let program = lower(&bench("startup")).expect("`benches/startup` lowers");
    validate(&program).expect("it holds the invariants");
    let id = program
        .function_named("startup", "main")
        .expect("its entry is lowered");
    assert_eq!(
        crate::render(&program, id),
        "fn startup.main arity=0 frame=0/0 -> value\n\
         \x20  0  const Unit\n\
         \x20  1  return\n"
    );
}
