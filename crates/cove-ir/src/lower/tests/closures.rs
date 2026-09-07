//! Function values: the environment, the captures in it, and the call.

use super::{listing, listing_in};

/// A lambda is a `Function` numbered after every declaration, and the value
/// the enclosing body holds is one word naming an environment.
///
/// The environment's payload word 0 is the callee's [`crate::FunctionId`],
/// written by [`crate::Inst::FuncRef`] and read by the call out of the
/// object rather than out of the instruction. The listing names it
/// symbolically — `@m.f#0`, never a bare number — which is the fact the
/// test below this one pins: the id underneath does renumber when the
/// package changes, and used to make it into this text.
#[test]
fn a_lambda_is_a_function_of_its_own_and_an_environment_naming_it() {
    let source = "fn f() -> Int {\n  let g = fn(x: Int) { x + 1 }\n  g(1)\n}";
    assert_eq!(
        listing(source, "f"),
        "\
fn @m.f() -> Int
  frame 4: s0:int s1:ref s2:int s3:int
  local g -> s1:fn [3, 6)
     0  alloc s1:ref closure m.f#0<closure>
     1  func-ref s2:int @m.f#0
     2  store-field s1:ref +0 s2:int Int
     3  int s2:int 1
     4  call-closure s3:int s1:ref (s2:Int)
     5  copy s0:int s3:int Int
     6  return s0:int Int
"
    );
    // The body is an ordinary function whose parameters occupy the frame from
    // slot 0. Nothing about it says it was written as a value.
    assert_eq!(
        listing(source, "f#0"),
        "\
fn @m.f#0(Int) -> Int
  frame 3: s0!:int s1:int s2:int
  local x -> s0:Int [0, 3)
     0  add.int.imm s2:int s0:int 1
     1  copy s1:int s2:int Int
     2  return s1:int Int
"
    );
}

/// [Issue #262](https://github.com/myuon/cove/issues/262)'s acceptance
/// criterion, and [#275](https://github.com/myuon/cove/issues/275)'s: a
/// listing with a closure in it is byte-for-byte unchanged when an unrelated
/// declaration is added ahead of it, though the addition renumbers the
/// callee's `FunctionId` underneath — and, since #275, renumbers the
/// closure's own `FunctionId` too.
///
/// `Plan::index` numbers a module's declarations in name order — `by_name`
/// is a `BTreeMap` — so `aardvark` sorts ahead of `f` and `id` and is what
/// actually renumbers them here; a name that sorted after would prove
/// nothing.
///
/// Before [`crate::Inst::FuncRef`] the callee's renumbering reached this
/// text: the shifted id passed through an [`crate::Inst::Int`] and the
/// `int sN:int <id>` line changed with it. Before #275 the closure's *own*
/// renumbering reached it too, through the header's `fn20`; that test could
/// only assert the two listings differed **solely** in that one line, because
/// the header carried a position by design. Now nothing does, so the three
/// cases below assert the listings are identical, full stop — a declared
/// function, a nested closure, and a generic instantiation, each read out of
/// a package with an unrelated earlier declaration inserted ahead of it.
/// Twelve standard-library functions moving earlier is exactly this, at the
/// scale of a real PR: #259 changed 27 golden lowerings for it.
#[test]
fn a_declared_functions_header_is_unmoved_by_an_unrelated_declaration_ahead_of_it() {
    let source = "fn f() -> Int {\n  let g = fn(x: Int) { x + 1 }\n  g(1)\n}";
    let before = listing_in(&[("m", source)], "m", "f");

    let unrelated_ahead = format!("fn aardvark() -> Int {{ 42 }}\n{source}");
    let after = listing_in(&[("m", &unrelated_ahead)], "m", "f");

    assert_eq!(before, after);
    assert!(before.starts_with("fn @m.f() -> Int\n"));
}

#[test]
fn a_nested_closures_header_is_unmoved_by_an_unrelated_declaration_ahead_of_it() {
    let source = "fn f() -> Int {\n  \
                    let g = fn(x: Int) {\n    \
                      let h = fn(y: Int) { x + y }\n    \
                      h(1)\n  \
                    }\n  \
                    g(2)\n\
                  }";
    let before = listing_in(&[("m", source)], "m", "f#0#0");

    let unrelated_ahead = format!("fn aardvark() -> Int {{ 42 }}\n{source}");
    let after = listing_in(&[("m", &unrelated_ahead)], "m", "f#0#0");

    assert_eq!(before, after);
    assert!(before.starts_with("fn @m.f#0#0(Int) -> Int\n"));
}

#[test]
fn a_generic_instantiations_header_is_unmoved_by_an_unrelated_declaration_ahead_of_it() {
    let source = "fn id<T>(x: T) -> T { x }\nfn f() -> Int { id(1) }";
    let before = listing_in(&[("m", source)], "m", "id<Int>");

    let unrelated_ahead = format!("fn aardvark() -> Int {{ 42 }}\n{source}");
    let after = listing_in(&[("m", &unrelated_ahead)], "m", "id<Int>");

    assert_eq!(before, after);
    assert!(before.starts_with("fn @m.id<Int>(Int) -> Int\n"));
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
fn @m.f(m.Point) -> Int
  frame 5: s0!:int s1!:int s2:int s3:ref s4:int
  local p -> s0:m.Point [0, 7)
  local g -> s3:fn [4, 6)
     0  alloc s3:ref closure m.f#0<closure>
     1  func-ref s4:int @m.f#0
     2  store-field s3:ref +0 s4:int Int
     3  store-field s3:ref +1 s0:int m.Point
     4  call-closure s4:int s3:ref ()
     5  copy s2:int s4:int Int
     6  return s2:int Int
"
    );
    assert_eq!(
        listing(source, "f#0"),
        "\
fn @m.f#0() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  capture p -> s0:m.Point
  local p -> s0:m.Point [0, 3)
     0  add.int s3:int s0:int s1:int
     1  copy s2:int s3:int Int
     2  return s2:int Int
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
fn @m.f() -> Int
  frame 3: s0:int s1:ref s2:int
  local g -> s1:fn [3, 5)
     0  alloc s1:ref closure m.f#0<closure>
     1  func-ref s2:int @m.f#0
     2  store-field s1:ref +0 s2:int Int
     3  call-closure s2:int s1:ref ()
     4  copy s0:int s2:int Int
     5  return s0:int Int
"
    );
    assert_eq!(
        listing(source, "f#0"),
        "\
fn @m.f#0() -> Int
  frame 2: s0:int s1:int
     0  int s1:int 1
     1  copy s0:int s1:int Int
     2  return s0:int Int
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
fn @m.f() -> Int
  frame 4: s0:int s1:ref s2:int s3:int
  local g -> s1:fn [3, 6)
     0  alloc s1:ref closure m.double<closure>
     1  func-ref s2:int @m.double
     2  store-field s1:ref +0 s2:int Int
     3  int s2:int 3
     4  call-closure s3:int s1:ref (s2:Int)
     5  copy s0:int s3:int Int
     6  return s0:int Int
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
fn @m.apply(fn Int) -> Int
  frame 4: s0!:ref s1!:int s2:int s3:int
  local g -> s0:fn [0, 3)
  local n -> s1:Int [0, 3)
     0  call-closure s3:int s0:ref (s1:Int)
     1  copy s2:int s3:int Int
     2  return s2:int Int
"
    );
    // The lambda is built at the call site and passed as an ordinary
    // argument: `s1:fn` is one word, whatever the signature.
    assert_eq!(
        listing(source, "f"),
        "\
fn @m.f() -> Int
  frame 4: s0:int s1:ref s2:int s3:int
     0  alloc s1:ref closure m.f#0<closure>
     1  func-ref s2:int @m.f#0
     2  store-field s1:ref +0 s2:int Int
     3  int s2:int 2
     4  call s3:int m.apply (s1:fn s2:Int) Int
     5  clear s1:ref fn
     6  copy s0:int s3:int Int
     7  return s0:int Int
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
fn @m.f#0() -> Int
  frame 4: s0:int s1:int s2:ref s3:int
  capture n -> s0:Int
  local n -> s0:Int [0, 7)
  local inner -> s2:fn [4, 6)
     0  alloc s2:ref closure m.f#0#0<closure>
     1  func-ref s3:int @m.f#0#0
     2  store-field s2:ref +0 s3:int Int
     3  store-field s2:ref +1 s0:int Int
     4  call-closure s3:int s2:ref ()
     5  copy s1:int s3:int Int
     6  return s1:int Int
"
    );
    assert_eq!(
        listing(source, "f#0#0"),
        "\
fn @m.f#0#0() -> Int
  frame 3: s0:int s1:int s2:int
  capture n -> s0:Int
  local n -> s0:Int [0, 3)
     0  add.int.imm s2:int s0:int 1
     1  copy s1:int s2:int Int
     2  return s1:int Int
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
fn @m.f(<addr>) -> Int
  frame 5: s0!:addr s1:int s2:int s3:ref s4:int
  local n -> s0:<addr> [0, 8)
  local g -> s3:fn [5, 7)
     0  load s2:int s0:addr Int
     1  alloc s3:ref closure m.f#0<closure>
     2  func-ref s4:int @m.f#0
     3  store-field s3:ref +0 s4:int Int
     4  store-field s3:ref +1 s2:int Int
     5  call-closure s2:int s3:ref ()
     6  copy s1:int s2:int Int
     7  return s1:int Int
"
    );
    assert_eq!(
        listing(
            "fn f(var n: Int) -> Int {\n  let g = fn() { n + 1 }\n  g()\n}",
            "f#0"
        ),
        "\
fn @m.f#0() -> Int
  frame 3: s0:int s1:int s2:int
  capture n -> s0:Int
  local n -> s0:Int [0, 3)
     0  add.int.imm s2:int s0:int 1
     1  copy s1:int s2:int Int
     2  return s1:int Int
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
fn @m.f() -> Int
  frame 4: s0:int s1:ref s2:int s3:int
  local double -> s1:fn [3, 6)
     0  alloc s1:ref closure m.f#0<closure>
     1  func-ref s2:int @m.f#0
     2  store-field s1:ref +0 s2:int Int
     3  int s2:int 21
     4  call-closure s3:int s1:ref (s2:Int)
     5  copy s0:int s3:int Int
     6  return s0:int Int
"
    );
    assert_eq!(
        listing(source, "f#0"),
        "\
fn @m.f#0(Int) -> Int
  frame 3: s0!:int s1:int s2:int
  local n -> s0:Int [0, 3)
     0  mul.int.imm s2:int s0:int 2
     1  copy s1:int s2:int Int
     2  return s1:int Int
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
fn @m.f#0(Int) -> Int
  frame 4: s0!:int s1:int s2:int s3:int
  capture base -> s1:Int
  local base -> s1:Int [0, 3)
  local n -> s0:Int [0, 3)
     0  add.int s3:int s0:int s1:int
     1  copy s2:int s3:int Int
     2  return s2:int Int
"
    );
}
