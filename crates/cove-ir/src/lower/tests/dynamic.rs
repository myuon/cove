use super::*;

// ---------------------------------------------------------------- dyn

/// A parameter written `dyn Trait` is converted in the callee's
/// prologue, which is where `bind_params` converts one, and a call on it
/// dispatches from the value rather than from the type.
#[test]
fn a_dyn_parameter_is_converted_where_it_is_bound_and_dispatches_from_the_value() {
    assert_eq!(
        listing(
            "trait Show {\n  fn show(self) -> String\n}\n\n\
             struct A {\n  n: Int\n}\n\n\
             impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
             fn f(v: dyn Show) -> String {\n  v.show()\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/1 params=[value] -> String\n\
         \x20  0  load 0\n\
         \x20  1  make-dyn m.Show\n\
         \x20  2  store 0\n\
         \x20  3  load 0\n\
         \x20  4  call-dyn m.Show.show argc=1 [m.A]\n\
         \x20  5  return\n"
    );
}

/// A field written `dyn Trait` is converted where the struct is built,
/// which is where `init_struct` converts one, and a dispatch through the
/// field reaches every type the package conforms to the trait — here two
/// of them, which is what makes the choice a run-time one.
#[test]
fn a_dyn_field_is_converted_where_the_struct_is_built() {
    assert_eq!(
        listing(
            "trait Show {\n  fn show(self) -> String\n}\n\n\
             struct A {\n  n: Int\n}\n\n\
             struct B {\n  n: Int\n}\n\n\
             impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
             impl Show for B {\n  fn show(self) -> String {\n    \"b\"\n  }\n}\n\n\
             struct Box {\n  item: dyn Show\n}\n\n\
             fn f() -> String {\n  let held = Box(item: A(n: 1))\n  held.item.show()\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/1 -> String\n\
         \x20  0  const Int(1)\n\
         \x20  1  make-struct m.A fields=n\n\
         \x20  2  make-dyn m.Show\n\
         \x20  3  make-struct m.Box fields=item\n\
         \x20  4  store 0\n\
         \x20  5  load 0\n\
         \x20  6  get-field-at 0\n\
         \x20  7  call-dyn m.Show.show argc=1 [m.A, m.B]\n\
         \x20  8  return\n"
    );
}

/// An `Array<dyn Trait>` converts each element, and the depth in the
/// instruction is the walk `Interpreter::coerce` makes into the array.
#[test]
fn an_array_of_dyn_converts_each_element() {
    assert_eq!(
        listing(
            "trait Show {\n  fn show(self) -> String\n}\n\n\
             struct A {\n  n: Int\n}\n\n\
             impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
             fn f(v: Array<dyn Show>) -> Int {\n  v.length()\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/1 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  make-dyn m.Show inside 1\n\
         \x20  2  store 0\n\
         \x20  3  load 0\n\
         \x20  4  call-builtin length argc=0\n\
         \x20  5  value-to-scalar\n\
         \x20  6  return-scalar\n"
    );
}

/// A declared `dyn Trait` return type converts the answer before it
/// leaves, which is where `Interpreter::call_target` converts one — so the
/// conversion belongs to the callee and every `return` of it reaches
/// one.
#[test]
fn a_dyn_return_type_converts_every_return() {
    assert_eq!(
        listing(
            "trait Show {\n  fn show(self) -> String\n}\n\n\
             struct A {\n  n: Int\n}\n\n\
             impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
             fn f(v: A, c: Bool) -> dyn Show {\n  if c {\n    return v\n  }\n  v\n}\n",
            "f"
        ),
        "fn m.f arity=2 frame=1/1 params=[value, Bool] -> value\n\
         \x20  0  load-scalar 1\n\
         \x20  1  jump-if-false-scalar 5\n\
         \x20  2  load 0\n\
         \x20  3  make-dyn m.Show\n\
         \x20  4  return\n\
         \x20  5  load 0\n\
         \x20  6  make-dyn m.Show\n\
         \x20  7  return\n"
    );
}

/// A call on a value whose type is a bounded type parameter is the same
/// dispatch a trait object gets, and reaches the same candidates: the
/// checker resolved the *signature* through the bound, and the run
/// resolves the *implementation* through the value.
#[test]
fn a_call_through_a_trait_bound_dispatches_from_the_value() {
    assert_eq!(
        listing(
            "trait Show {\n  fn show(self) -> String\n}\n\n\
             struct A {\n  n: Int\n}\n\n\
             struct B {\n  n: Int\n}\n\n\
             impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
             impl Show for B {\n  fn show(self) -> String {\n    \"b\"\n  }\n}\n\n\
             fn f<T: Show>(v: T) -> String {\n  v.show()\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/1 params=[value] -> String\n\
         \x20  0  load 0\n\
         \x20  1  call-dyn m.Show.show argc=1 [m.A, m.B]\n\
         \x20  2  return\n"
    );
}

/// A trait's default body is checked once with `self` typed as the rigid
/// `Self` bounded by that trait, so a call it makes on `self` is a call
/// through a bound — and the body is lowered once per conformance that
/// did not override it, under the name of the type it was materialised
/// for.
#[test]
fn a_trait_default_body_dispatches_on_self() {
    assert_eq!(
        listing(
            "trait Show {\n  fn show(self) -> String\n\n\
             \x20 fn loud(self) -> String {\n    \"!{self.show()}!\"\n  }\n}\n\n\
             struct A {\n  n: Int\n}\n\n\
             impl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\n\
             fn f(v: dyn Show) -> String {\n  v.loud()\n}\n",
            "A.loud"
        ),
        // The return word is `String` rather than `value` because the
        // checker now records a signature for a trait's default body. It
        // always knew the type; nothing was recording it, so every reader
        // had to fall back to the general representation.
        "fn m.A.loud arity=1 frame=0/1 params=[value] receiver -> String\n\
         \x20  0  const Str(\"!\")\n\
         \x20  1  load 0\n\
         \x20  2  call-dyn m.Show.show argc=1 [m.A]\n\
         \x20  3  const Str(\"!\")\n\
         \x20  4  concat 3\n\
         \x20  5  return\n"
    );
}
