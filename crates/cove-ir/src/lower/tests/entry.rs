use super::*;

// ---------------------------------------------------- from an entry

/// The names of the functions a lowered program holds, in the order it
/// numbered them.
fn lowered_names(program: &Program) -> Vec<String> {
    program
        .functions
        .iter()
        .map(|function| format!("{}.{}", function.module, function.name))
        .collect()
}

/// Issue #115 itself: `hello` is three lines and `callbacks/` holds an
/// `async` closure, and the two share a package and nothing else.
#[test]
fn an_entry_lowers_only_what_it_reaches_of_the_examples() {
    let checked = examples();
    // The whole package lowers now. It did not when this test was
    // written — it stopped at an `async` closure, and the point of the
    // test was that `hello` lowered anyway — so what is left is the half
    // that was always the point: an entry is what it reaches, whatever
    // else the package holds.
    lower(&checked).expect("`examples/` lowers whole");

    let lowered = lower_entry(&checked, "hello", "main").expect("`hello.main` lowers");
    validate(&lowered.program).expect("it holds the VM's invariants");
    assert_eq!(
        lowered_names(&lowered.program),
        ["hello.main", "hello.greeting"]
    );
    assert_eq!(lowered.entry, FunctionId(0));
    assert!(
        lowered
            .program
            .function_named("callbacks", "main")
            .is_none(),
        "nothing `hello` cannot reach comes with it"
    );
}

/// A program holds what its entry reaches and nothing beside it, so the
/// count is the measurement and the absence is the point.
#[test]
fn a_lowered_entry_holds_only_what_it_reaches() {
    let source = "fn used() -> Int {\n  1\n}\n\nfn between() -> Int {\n  used()\n}\n\n\
                  fn unreached() -> Int {\n  used()\n}\n\nfn main() -> Int {\n  between()\n}\n";
    let checked = checked(source);

    let lowered = lower_entry(&checked, "m", "main").expect("`m.main` lowers");
    validate(&lowered.program).expect("it holds the VM's invariants");
    // Numbered on discovery: the entry, then what its body called, then
    // what that body called.
    assert_eq!(
        lowered_names(&lowered.program),
        ["m.main", "m.between", "m.used"]
    );
    assert!(lowered.program.function_named("m", "unreached").is_none());

    // The same package, lowered whole, holds the one the entry cannot
    // reach as well.
    let whole = lower(&checked).expect("the package lowers");
    assert_eq!(
        lowered_names(&whole),
        ["m.between", "m.main", "m.unreached", "m.used"]
    );
}

/// A declaration is numbered once, so a call back to something already
/// numbered adds nothing to walk and the worklist empties.
#[test]
fn recursion_and_mutual_recursion_terminate() {
    let checked = checked(
        "fn down(n: Int) -> Int {\n  if n == 0 {\n    0\n  } else {\n    up(n - 1)\n  }\n}\n\n\
         fn up(n: Int) -> Int {\n  down(n - 1)\n}\n\n\
         fn main() -> Int {\n  main2(3)\n}\n\n\
         fn main2(n: Int) -> Int {\n  down(n) + main2(0)\n}\n",
    );
    let lowered = lower_entry(&checked, "m", "main").expect("`m.main` lowers");
    validate(&lowered.program).expect("it holds the VM's invariants");
    assert_eq!(
        lowered_names(&lowered.program),
        ["m.main", "m.main2", "m.down", "m.up"]
    );
}

/// A method is reached through a call like anything else, so a method
/// only an unreached function calls is not part of the program.
#[test]
fn a_method_is_lowered_where_the_entry_reaches_it() {
    let checked = checked(
        "struct P {\n  x: Int\n}\n\n\
         impl P {\n  fn reached(self) -> Int {\n    self.x\n  }\n\n  \
         fn unreached(self) -> Int {\n    self.x + 1\n  }\n}\n\n\
         fn aside() -> Int {\n  P(x: 2).unreached()\n}\n\n\
         fn main() -> Int {\n  P(x: 1).reached()\n}\n",
    );
    let lowered = lower_entry(&checked, "m", "main").expect("`m.main` lowers");
    validate(&lowered.program).expect("it holds the VM's invariants");
    assert_eq!(lowered_names(&lowered.program), ["m.main", "m.P.reached"]);
    assert!(lowered.program.function_named("m", "P.unreached").is_none());
    assert!(lowered.program.function_named("m", "aside").is_none());
}

/// Narrowing what is lowered narrows nothing about what is refused: a
/// construct the entry reaches is reported in the words it always was.
#[test]
fn an_unsupported_construct_on_the_path_is_still_refused() {
    let source = "fn helper() -> Int {\n  fn g() -> Int {\n    1\n  }\n  g()\n}\n\n\
                  fn main() -> Int {\n  helper()\n}\n";
    let checked = checked(source);
    let whole = lower(&checked).expect_err("the package does not lower");
    let entry = lower_entry(&checked, "m", "main").expect_err("nor does the entry");
    assert_eq!(entry.what, "a function declared inside a function body");
    assert_eq!(entry.what, whole.what);
    assert_eq!(entry.span, whole.span);
}

/// A `[run.<name>]` table is a file a person edits, so a name it gets
/// wrong is reported rather than crashed on.
#[test]
fn a_missing_entry_is_reported() {
    let checked = checked("fn main() -> Int {\n  1\n}\n");
    let missing = lower_entry(&checked, "m", "notMain").expect_err("there is no `m.notMain`");
    assert_eq!(
        missing.what,
        "`m.notMain`, which this package does not declare"
    );
    let missing = lower_entry(&checked, "elsewhere", "main").expect_err("there is no module");
    assert_eq!(
        missing.what,
        "`elsewhere.main`, which this package does not declare"
    );
}
