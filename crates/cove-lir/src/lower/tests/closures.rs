//! Function values: the environment, the captures in it, and the call.

use super::listing;

/// A lambda is a `Function` numbered after every declaration, and the value
/// the enclosing body holds is one word naming an environment.
///
/// The environment's payload word 0 is the callee's id — `1`, which is
/// `fn1` — and the call reads it out of the object rather than out of the
/// instruction.
#[test]
fn a_lambda_is_a_function_of_its_own_and_an_environment_naming_it() {
    let source = "fn f() -> Int {\n  let g = fn(x: Int) { x + 1 }\n  g(1)\n}";
    assert_eq!(
        listing(source, "f"),
        "\
fn0 m.f() -> Int
  frame 4: s0:int s1:ref s2:int s3:int
     0  alloc s1:ref closure m.f#0<closure>
     1  int s2:int 1
     2  store-field s1:ref +0 s2:int Int
     3  int s2:int 1
     4  call-closure s3:int s1:ref (s2:Int)
     5  copy s0:int s3:int Int
     6  clear s1:ref fn
     7  return s0:int
"
    );
    // The body is an ordinary function whose parameters occupy the frame from
    // slot 0. Nothing about it says it was written as a value.
    assert_eq!(
        listing(source, "f#0"),
        "\
fn1 m.f#0(Int) -> Int
  frame 4: s0!:int s1:int s2:int s3:int
     0  int s2:int 1
     1  add.int s3:int s0:int s2:int
     2  copy s1:int s3:int Int
     3  return s1:int
"
    );
}

/// A capture is stored **inline in the environment, at its own layout's
/// width**, and read back into a run of the callee's frame that follows the
/// parameters.
///
/// `store-field +1 s0 m.Point` writes two words at payload word 1, and the
/// body's `capture p -> s0:m.Point` says where the machine copies them back
/// — so `p.x` and `p.y` are slots 0 and 1 and reaching them costs nothing,
/// exactly as they would in the body that made the closure.
#[test]
fn a_capture_is_inline_in_the_environment_at_its_own_width() {
    let source = "struct Point { x: Int, y: Int }\n\
                  fn f(p: Point) -> Int {\n  let g = fn() { p.x + p.y }\n  g()\n}";
    assert_eq!(
        listing(source, "f"),
        "\
fn0 m.f(m.Point) -> Int
  frame 5: s0!:int s1!:int s2:int s3:ref s4:int
     0  alloc s3:ref closure m.f#0<closure>
     1  int s4:int 1
     2  store-field s3:ref +0 s4:int Int
     3  store-field s3:ref +1 s0:int m.Point
     4  call-closure s4:int s3:ref ()
     5  copy s2:int s4:int Int
     6  clear s3:ref fn
     7  return s2:int
"
    );
    assert_eq!(
        listing(source, "f#0"),
        "\
fn1 m.f#0() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  capture p -> s0:m.Point
     0  add.int s3:int s0:int s1:int
     1  copy s2:int s3:int Int
     2  return s2:int
"
    );
}

/// Captures are by value at creation time, which the oracle pins: the words
/// are copied into the environment where the closure is built, and nothing
/// writes back through one.
///
/// A closure that captures nothing is that with the list empty — one payload
/// word, the callee — and it is not a different shape. There is no second
/// representation for a function value that happens to be closed.
#[test]
fn a_closure_that_captures_nothing_is_the_same_object_with_an_empty_list() {
    let source = "fn f() -> Int {\n  let g = fn() { 1 }\n  g()\n}";
    assert_eq!(
        listing(source, "f"),
        "\
fn0 m.f() -> Int
  frame 3: s0:int s1:ref s2:int
     0  alloc s1:ref closure m.f#0<closure>
     1  int s2:int 1
     2  store-field s1:ref +0 s2:int Int
     3  call-closure s2:int s1:ref ()
     4  copy s0:int s2:int Int
     5  clear s1:ref fn
     6  return s0:int
"
    );
    assert_eq!(
        listing(source, "f#0"),
        "\
fn1 m.f#0() -> Int
  frame 2: s0:int s1:int
     0  int s1:int 1
     1  copy s0:int s1:int Int
     2  return s0:int
"
    );
}

/// A declared function written where a value goes is the same object with no
/// captures, naming the declaration.
///
/// The alternative would be a second representation for a function value that
/// is known statically, and then every place that holds one would have to know
/// which of the two it had. One shape costs one allocation where the name is
/// read, and it is what makes `xs.map(double)` and `xs.map(fn(x) { ... })` the
/// same lowering.
#[test]
fn a_declared_function_used_as_a_value_is_an_environment_naming_it() {
    assert_eq!(
        listing(
            "fn double(n: Int) -> Int { n * 2 }\nfn f() -> Int {\n  let g = double\n  g(3)\n}",
            "f"
        ),
        "\
fn1 m.f() -> Int
  frame 4: s0:int s1:ref s2:int s3:int
     0  alloc s1:ref closure m.double<closure>
     1  int s2:int 0
     2  store-field s1:ref +0 s2:int Int
     3  int s2:int 3
     4  call-closure s3:int s1:ref (s2:Int)
     5  copy s0:int s3:int Int
     6  clear s1:ref fn
     7  return s0:int
"
    );
}

/// A parameter of function type is one `Repr::Ref` word, and a call through
/// it names the slot holding it.
///
/// Which body it reaches is not a static fact and no instruction claims it
/// is: the callee comes out of the object at the call.
#[test]
fn a_call_through_a_function_value_names_the_slot_holding_it() {
    let source = "fn apply(g: fn(Int) -> Int, n: Int) -> Int { g(n) }\n\
                  fn f() -> Int { apply(fn(x) { x + 1 }, 2) }";
    assert_eq!(
        listing(source, "apply"),
        "\
fn0 m.apply(fn Int) -> Int
  frame 4: s0!:ref s1!:int s2:int s3:int
     0  call-closure s3:int s0:ref (s1:Int)
     1  copy s2:int s3:int Int
     2  return s2:int
"
    );
    // The lambda is built at the call site and passed as an ordinary
    // argument: `s1:fn` is one word, whatever the signature.
    assert_eq!(
        listing(source, "f"),
        "\
fn1 m.f() -> Int
  frame 4: s0:int s1:ref s2:int s3:int
     0  alloc s1:ref closure m.f#0<closure>
     1  int s2:int 2
     2  store-field s1:ref +0 s2:int Int
     3  int s2:int 2
     4  call s3:int m.apply (s1:fn s2:Int)
     5  clear s1:ref fn
     6  copy s0:int s3:int Int
     7  return s0:int
"
    );
}

/// A lambda inside a lambda is numbered after the one that made it, and the
/// name says which body wrote it.
///
/// The inner one captures `n` because the outer one captured it first: the
/// free names of a nested lambda are free in the enclosing one too, which is
/// what makes the value reachable at all.
#[test]
fn a_lambda_inside_a_lambda_is_numbered_after_the_one_that_made_it() {
    let source = "fn f(n: Int) -> Int {\n  \
                  let outer = fn() {\n    let inner = fn() { n + 1 }\n    inner()\n  }\n  \
                  outer()\n}";
    assert_eq!(
        listing(source, "f#0"),
        "\
fn1 m.f#0() -> Int
  frame 4: s0:int s1:int s2:ref s3:int
  capture n -> s0:Int
     0  alloc s2:ref closure m.f#0#0<closure>
     1  int s3:int 2
     2  store-field s2:ref +0 s3:int Int
     3  store-field s2:ref +1 s0:int Int
     4  call-closure s3:int s2:ref ()
     5  copy s1:int s3:int Int
     6  clear s2:ref fn
     7  return s1:int
"
    );
    assert_eq!(
        listing(source, "f#0#0"),
        "\
fn2 m.f#0#0() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  capture n -> s0:Int
     0  int s2:int 1
     1  add.int s3:int s0:int s2:int
     2  copy s1:int s3:int Int
     3  return s1:int
"
    );
}

/// A capture of a `var` parameter is the value behind the address, taken at
/// creation time.
///
/// The oracle pins it: `Env::captures` reads every binding it captures
/// through `Place::read`, and reading an alias place is reading the storage
/// it names. So the environment holds a copy like any other capture, and the
/// one instruction the difference costs is the load — after which the body
/// reads an ordinary `Int` capture and knows nothing about the alias.
#[test]
fn a_capture_of_a_var_parameter_is_the_value_behind_the_address() {
    assert_eq!(
        listing(
            "fn f(var n: Int) -> Int {\n  let g = fn() { n + 1 }\n  g()\n}",
            "f"
        ),
        "\
fn0 m.f(<addr>) -> Int
  frame 5: s0!:addr s1:int s2:int s3:ref s4:int
     0  load s2:int s0:addr Int
     1  alloc s3:ref closure m.f#0<closure>
     2  int s4:int 1
     3  store-field s3:ref +0 s4:int Int
     4  store-field s3:ref +1 s2:int Int
     5  call-closure s2:int s3:ref ()
     6  copy s1:int s2:int Int
     7  clear s3:ref fn
     8  return s1:int
"
    );
    assert_eq!(
        listing(
            "fn f(var n: Int) -> Int {\n  let g = fn() { n + 1 }\n  g()\n}",
            "f#0"
        ),
        "\
fn1 m.f#0() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  capture n -> s0:Int
     0  int s2:int 1
     1  add.int s3:int s0:int s2:int
     2  copy s1:int s3:int Int
     3  return s1:int
"
    );
}

// ---- a `fn` declared inside a body -------------------------------------

/// A local `fn` is the closure the enclosing body writes, and the name is a
/// binding of the enclosing scope.
///
/// Nothing about the environment or the call says which of the two spellings
/// made it: `fn double(n: Int) -> Int { n * 2 }` and
/// `let double = fn(n: Int) { n * 2 }` are one lowering, which is what the
/// checker, the resolver and the oracle all already say a local `fn` is.
/// Binding the name is also what makes `double(21)` a call: the frame
/// answers before any of the arms that resolve a declaration do.
#[test]
fn a_local_fn_is_the_closure_the_body_wrote_and_a_binding_of_its_scope() {
    let source = "fn f() -> Int {\n  fn double(n: Int) -> Int { n * 2 }\n  double(21)\n}";
    assert_eq!(
        listing(source, "f"),
        "\
fn0 m.f() -> Int
  frame 4: s0:int s1:ref s2:int s3:int
     0  alloc s1:ref closure m.f#0<closure>
     1  int s2:int 1
     2  store-field s1:ref +0 s2:int Int
     3  int s2:int 21
     4  call-closure s3:int s1:ref (s2:Int)
     5  copy s0:int s3:int Int
     6  clear s1:ref fn
     7  return s0:int
"
    );
    assert_eq!(
        listing(source, "f#0"),
        "\
fn1 m.f#0(Int) -> Int
  frame 4: s0!:int s1:int s2:int s3:int
     0  int s2:int 2
     1  mul.int s3:int s0:int s2:int
     2  copy s1:int s3:int Int
     3  return s1:int
"
    );
}

/// It captures what the body around it binds, by value and at creation time,
/// exactly as a lambda written in its place would.
#[test]
fn a_local_fn_captures_the_bindings_around_it() {
    let source =
        "fn f(base: Int) -> Int {\n  fn shifted(n: Int) -> Int { n + base }\n  shifted(1)\n}";
    assert_eq!(
        listing(source, "f#0"),
        "\
fn1 m.f#0(Int) -> Int
  frame 4: s0!:int s1:int s2:int s3:int
  capture base -> s1:Int
     0  add.int s3:int s0:int s1:int
     1  copy s2:int s3:int Int
     2  return s2:int
"
    );
}
