//! Host calls: the one place in ordinary execution where a `Value` exists.

use super::{checked, listing};
use crate::lower::lower;
use crate::repr::Repr;

#[test]
fn a_host_call_names_the_module_and_the_operation() {
    // The boundary looks the pair up in the registry exactly as the
    // interpreter does, so what the lowering records is the module and the
    // operation as the source writes them. The argument is an ordinary slot;
    // materialising it into a `Value` is the boundary's work, and the
    // string it built is cleared the moment the call has read it.
    assert_eq!(
        listing(
            "use console.println\n\
             fn say() -> Result<Unit, Error> {\n  println(\"hi\")?\n  Ok(())\n}",
            "say"
        ),
        "\
fn0 m.say(0) -> ref
  frame 8: s0:ref s1:ref s2:ref s3:int s4:int s5:bool s6:unit s7:ref
     0  str s1:ref \"hi\"
     1  call-host s2:ref console.println (s1:ref)
     2  clear s1:ref
     3  get-word s3:int s2:ref +0
     4  int s4:int 0
     5  eq.int s5:bool s3:int s4:int
     6  branch-false s5:bool 9
     7  get-word s6:unit s2:ref +1
     8  jump 15
     9  get-word s1:ref s2:ref +1
    10  alloc s7:ref Result<enum>
    11  int s3:int 1
    12  set-word s7:ref +0 s3:int
    13  set-word s7:ref +1 s1:ref
    14  return s7:ref
    15  clear s2:ref
    16  unit s6:unit
    17  alloc s2:ref Result<enum>
    18  int s3:int 0
    19  set-word s2:ref +0 s3:int
    20  set-word s2:ref +1 s6:unit
    21  move s0:ref s2:ref
    22  clear s2:ref
    23  return s0:ref
"
    );
}

#[test]
fn the_result_word_is_what_the_schema_declared() {
    // `clock.now()` answers a `Duration`, so it answers a word rather than
    // an object: the boundary writes the host's answer back into a slot the
    // frame calls a duration, and the arithmetic after it is the ordinary
    // integer arithmetic nanoseconds add by.
    assert_eq!(
        listing(
            "use clock\nfn since(start: Duration) -> Duration { clock.now() - start }",
            "since"
        ),
        "\
fn0 m.since(1) -> duration
  frame 4: s0!:duration s1:duration s2:duration s3:duration
     0  call-host s2:duration clock.now ()
     1  sub.int s3:duration s2:duration s0:duration
     2  move s1:duration s3:duration
     3  return s1:duration
"
    );
}

#[test]
fn an_operation_named_two_ways_is_one_entry() {
    // `use console.println` and `use console` reach the same operation, and
    // a name is not what the boundary looks the operation up by. Two call
    // sites of one operation are therefore one [`crate::HostOp`], whichever
    // way each of them was written.
    let program = lower(&checked(
        "use console\n\
         use files\n\
         fn twice(p: String) -> Result<String, Error> {\n\
           console.println(\"a\")?\n\
           console.println(\"b\")?\n\
           files.read(p)\n\
         }",
    ))
    .expect("the program lowers");
    let named: Vec<(String, String, Repr)> = program
        .host_ops
        .iter()
        .map(|op| (op.module.to_string(), op.operation.to_string(), op.result))
        .collect();
    assert_eq!(
        named,
        vec![
            ("console".to_string(), "println".to_string(), Repr::Ref),
            ("files".to_string(), "read".to_string(), Repr::Ref),
        ]
    );
}

#[test]
fn an_unqualified_import_reaches_the_same_operation() {
    let program = lower(&checked(
        "use console.println\nfn say() -> Result<Unit, Error> { println(\"hi\") }",
    ))
    .expect("the program lowers");
    assert_eq!(program.host_ops.len(), 1);
    assert_eq!(&*program.host_ops[0].module, "console");
    assert_eq!(&*program.host_ops[0].operation, "println");
}
