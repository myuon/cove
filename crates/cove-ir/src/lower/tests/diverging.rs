//! A form whose operand leaves before the form is reached: issue 698.
//!
//! The checker accepts a `return`, a `break` or a `continue` wherever a
//! value of any type goes, so each of them can be the operand of anything.
//! What follows the operand is unreachable and is not lowered — see
//! `lower::unreached` — and before issue 698 it was: `!return x` emitted a
//! `not` over the `Unit` slot a diverging expression answers, and the
//! verifier refused the program.

use super::listing;

/// The negation the issue found: nothing after the `return` is emitted, and
/// the second `return` is the body's own tail, which is dead too.
#[test]
fn a_negation_of_a_return_emits_no_negation() {
    assert_eq!(
        listing("fn f(x: Int) -> Int {\n  !return x\n}", "f"),
        "\
fn @m.f(Int) -> Int
  frame 3: s0!:int s1:int s2:unit
  local x -> s0:Int [0, 2)
     0  return s0:Int
     1  return s1:Int
"
    );
}

/// An operand before the one that leaves is still evaluated, because it is
/// evaluated first; the operator and the call after it are not.
#[test]
fn an_operand_before_the_one_that_leaves_is_still_evaluated() {
    assert_eq!(
        listing(
            "fn g(x: Int) -> Int { x }\nfn f(x: Int) -> Int {\n  let y = g(1) + g(return x)\n  y\n}",
            "f"
        ),
        "\
fn @m.f(Int) -> Int
  frame 6: s0!:int s1:int s2:int s3:int s4:unit s5:int
  local x -> s0:Int [0, 4)
  local y -> s3:Int [3, 3)
     0  int s2:int 1
     1  copy s3:Int s2:Int
     2  return s0:Int
     3  return s1:Int
"
    );
}

/// An `if` fuses a comparison into its branch and so lowers the
/// comparison's operands itself; a leaving operand two levels down still
/// stops it before the branch.
#[test]
fn a_condition_whose_comparison_leaves_emits_no_branch() {
    assert_eq!(
        listing(
            "fn f(x: Int) -> Int {\n  if 1 < (return x) { 1 } else { 2 }\n}",
            "f"
        ),
        "\
fn @m.f(Int) -> Int
  frame 5: s0!:int s1:int s2:int s3:unit s4:bool
  local x -> s0:Int [0, 3)
     0  int s2:int 1
     1  return s0:Int
     2  return s1:Int
"
    );
}

/// Every operand position the issue's family reaches, with each of the
/// three ways to leave, lowers to a program the verifier accepts. Before
/// the fix the operators and the `if` conditions here panicked in the
/// verifier, and the call arguments, the array elements, the field and
/// method receivers and the assignments were refused as gaps.
#[test]
fn every_operand_position_may_leave() {
    let forms = [
        "!R",
        "-R",
        "!!R",
        "-(-R)",
        "(R) + 1",
        "1 + (R)",
        "1.5 < (R)",
        "\"a\" == (R)",
        "1 != (R)",
        "(R) && true",
        "true && (R)",
        "\"{-R}\"",
        "\"{g(R)}\"",
        "g(R)",
        "h(g(1), R)",
        "[1, R]",
        "P(a: g(1), b: R).a",
        "(R).length()",
        "\"abc\".slice(0, R)",
        "1..(R)",
        "if R { 1 } else { 2 }",
        "if 1 < (R) { 1 } else { 2 }",
        "if !(1 < (R)) { 1 } else { 2 }",
        "while 1 < (R) { }",
        "match -R { _ => 1 }",
        "m = R",
        "m += R",
    ];
    let header = "struct P {\n  a: Int\n  b: Int\n}\nfn g(x: Int) -> Int { x + 1 }\nfn h(x: Int, y: Int) -> Int { x + y }\n";
    for leave in ["(return 7)", "(break)", "(continue)"] {
        // As a statement, and as an initialiser followed by a use of what it
        // bound, which is dead code a binding of `Never` reaches.
        for (shape, body) in [
            ("statement", "{}\n    n += 1"),
            ("binding", "let y = {}\n    n += g(0)\n    let z = y"),
        ] {
            let mut source = header.to_string();
            for (at, form) in forms.iter().enumerate() {
                let body = body.replace("{}", &form.replace('R', leave));
                source.push_str(&format!(
                    "fn f{at}(x: Int) -> Int {{\n  var m = 0\n  var n = 0\n  while n < 3 {{\n    {body}\n  }}\n  n\n}}\n"
                ));
            }
            let (sources, checked) = super::checked(&source);
            if let Err(items) = crate::lower(&checked, &sources, &cove_schema::HostSchemas::new()) {
                panic!(
                    "`{leave}` as a {shape} is refused:\n{}",
                    super::rendered(&sources, &items)
                );
            }
        }
    }
}
