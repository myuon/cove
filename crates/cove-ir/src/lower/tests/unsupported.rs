use super::*;

// -------------------------------------------------------- unsupported

#[test]
fn every_unsupported_construct_is_named() {
    let cases: Vec<(&str, &str)> = vec![
        (
            // Every lambda but one: the closure a `lock` is given may
            // write its first parameter `var`, because `Inst::Lock` hands
            // it a place rather than a value. Nothing else can, because
            // every argument of an `Inst::CallValue` travels on the value
            // stack. An `async` lambda is no different, and an `async fn`
            // and a call to one both lower.
            "a closure's `var` parameter `n`",
            "fn f() -> Int {\n  let g = fn(var n: Int) {\n    n = 2\n  }\n  1\n}\n",
        ),
        (
            // A scope lowers; what does not is a scope in a function
            // whose answer travels on the scalar stack. A child that
            // settled `Err` returns that failure from the enclosing
            // call, and the oracle does exactly that whatever the
            // declared return type is — so a function that answers an
            // `Int` has no stack for the failure to come back on.
            "a task scope in a function that answers an `Int` or a `Bool`",
            "fn f() -> Int {\n  scope tasks {\n    1\n  }\n}\n",
        ),
        (
            "a `var` variadic parameter",
            "fn g(var x: Int...) -> Int {\n  1\n}\n",
        ),
        (
            // A closure is a function type, and a function type in Cove
            // names a fixed list of parameters. `cove check` still lets
            // `...` through here and types the parameter as its element
            // type, while `bind_params` wraps it in an `Array` — so this
            // is the one variadic shape left where the two backends would
            // answer differently, and the VM refuses rather than answer.
            "a closure's variadic parameter `items`",
            "fn f() -> Int {\n  let g = fn(items: Int...) {\n    items\n  }\n  g(1)\n  1\n}\n",
        ),
        (
            // A `dyn` the conversion does not reach. A `Map`'s value
            // type is written inside a head with two arguments, which
            // is where `Interpreter::coerce` stops walking, so nothing
            // converts what stands there and this is refused rather
            // than converted anyway.
            "a `dyn` parameter",
            "trait Show {\n  fn show(self) -> String\n}\n\nstruct A {\n  n: Int\n}\n\nimpl Show for A {\n  fn show(self) -> String {\n    \"a\"\n  }\n}\n\nfn f(v: Map<String, dyn Show>) -> Int {\n  v.length()\n}\n",
        ),
        (
            // `Shared(value)` and `lock` both lower; what does not is a
            // `lock` whose closure is a value rather than written at the
            // call. A `var` parameter names the cell's contents, which
            // only `Inst::Lock` can hand over, so the closure has to be
            // the one this instruction is lowering.
            "a `lock` whose closure is not written at the call",
            "fn g(var n: Int) {\n  n = 2\n}\n\nfn f() -> Int {\n  let s = Shared(1)\n  let h = fn(v: Int) {\n    v\n  }\n  s.lock(h)\n}\n",
        ),
        (
            "`snapshot` on a `Vector<B>`, which a conformance answers",
            "struct B {\n  n: Int\n}\n\nimpl Snapshot for B {\n  fn snapshot(self) -> B {\n    B(n: self.n)\n  }\n}\n\nfn f(v: Vector<B>) -> Vector<B> {\n  v.snapshot()\n}\n",
        ),
        (
            "a call to `g`, whose parameter `n` is declared `var` and whose argument is not written `var`",
            "fn g(var n: Int) {\n  n = 1\n}\n\nfn f() -> Int {\n  var x = 1\n  g(x)\n  x\n}\n",
        ),
        (
            "a call to `g`, whose parameter `n` is not declared `var` and whose argument is written `var`",
            "fn g(n: Int) -> Int {\n  n\n}\n\nfn f() -> Int {\n  var x = 1\n  g(var x)\n}\n",
        ),
        (
            "a function declared inside a function body",
            "fn f() -> Int {\n  fn g() -> Int {\n    1\n  }\n  g()\n}\n",
        ),
        (
            "`g` used as a value, whose parameter `n` has a default",
            "fn g(n: Int = 1) -> Int {\n  n\n}\n\nfn f() -> Int {\n  let h = g\n  1\n}\n",
        ),
    ];
    for (what, source) in cases {
        assert_eq!(refused(source), what, "for:\n{source}");
    }
}

/// The one refusal in this pass no checked program can reach.
///
/// [`arguments_in_order`] is the invariant the calling convention rests
/// on: `Body::call_declared` pushes the arguments in the order the
/// parameters are declared, and that is the order they were *written* in
/// only because the labels stand in declaration order. `cove-sema`
/// refuses a call whose labels do not (ADR 0021), so this is reached by
/// driving the function rather than a program — which is what an
/// invariant is worth stating for, and is why it stays.
#[test]
fn arguments_that_do_not_stand_in_declaration_order_are_refused() {
    let span = Span::new(cove_diag::FileId(0), 0, 0);
    let arg = |label: &str| Arg {
        label: Some(cove_diag::Spanned {
            node: label.to_string(),
            span,
        }),
        is_var: false,
        spread: false,
        value: Expr {
            id: cove_syntax::ast::ExprId(0),
            kind: ExprKind::Int(1),
            span,
        },
        span,
    };
    let written = vec![arg("b"), arg("a")];
    let why = match arguments_in_order(&["a", "b"], Args::new(&written, None), "g", false, span) {
        Ok(_) => panic!("labels out of declaration order are refused"),
        Err(why) => why,
    };
    assert_eq!(
        why.what,
        "a call to `g` whose arguments do not stand in declaration order"
    );

    let in_order = vec![arg("a"), arg("b")];
    let assigned =
        match arguments_in_order(&["a", "b"], Args::new(&in_order, None), "g", false, span) {
            Ok(assigned) => assigned,
            Err(why) => panic!("labels in declaration order are accepted, but {why}"),
        };
    assert_eq!(assigned.slots, vec![Some(0), Some(1)]);
}

/// The `Display` a diagnostic shows says which backend refused, so a
/// person reading it knows the program is fine and the VM is not ready.
#[test]
fn an_unsupported_construct_reads_as_a_sentence() {
    let why = lower(&checked(
        "fn f() -> Int {\n  fn g() -> Int {\n    1\n  }\n  g()\n}\n",
    ))
    .expect_err("a function declared inside a function body is refused");
    assert_eq!(
        why.to_string(),
        "the VM cannot yet run a function declared inside a function body"
    );
}

/// "yet" stands before the construct, so a construct named by a phrase
/// that ends in a clause still reads as a sentence. It used to be
/// appended, and several of the names below end in one.
#[test]
fn a_construct_named_by_a_clause_still_reads_as_a_sentence() {
    let why = lower(&checked(
        "fn g(n: Int = 1) -> Int {\n  n\n}\n\nfn f() -> Int {\n  let h = g\n  1\n}\n",
    ))
    .expect_err("a declaration with a default used as a value is refused");
    assert_eq!(
        why.to_string(),
        "the VM cannot yet run `g` used as a value, whose parameter `n` has a default"
    );
}
