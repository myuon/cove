//! Host calls, resource handles, and the types a host module declares.
//!
//! The boundary is the subject: which capability a call needs, which
//! instruction routes a call by the handle it stands on rather than by a
//! name, and what a stale handle answers.

use super::*;

/// The same calls, in the same order, with the same values.
#[test]
fn host_calls_reach_the_host_in_the_same_order() {
    let outcome = agree_main(
        "Result<Unit, Error>",
        "  println(\"one\")?\n  for i in 0..<3 {\n    println(\"tick {i}\")?\n  }\n  println(\"done\")?\n  Ok(())",
    );
    assert_eq!(outcome.output, "one\ntick 0\ntick 1\ntick 2\ndone\n");
}

/// A capability the run was not granted is refused at the boundary both
/// backends call through.
#[test]
fn an_ungranted_capability_is_refused_at_the_boundary() {
    let (sources, checked) = checked_module(
        "use console.println\n\nexport fn main() -> Result<Unit, Error> {\n  println(\"hello\")?\n  Ok(())\n}\n",
    );
    let (interpreted, lowered) = crate::on_cove_stack(|| {
        let ungranted = || {
            let mut hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
            hosts.register(Box::new(Console::new(Buffer::default(), Buffer::default())));
            Arc::new(hosts)
        };
        let interpreted = {
            let runtime = Runtime::new(checked.clone(), sources.clone(), ungranted());
            described(Interpreter::new(&runtime).run_entry("m", "main", Vec::new()))
        };
        let lowered = {
            let program = cove_ir::lower::lower(&checked).expect("it lowers");
            let entry = program.function_named("m", "main").expect("`main` lowered");
            let hosts = ungranted();
            let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
            described(Vm::new(&runtime, &hosts, &Arc::new(program)).run(entry, Vec::new()))
        };
        (interpreted, lowered)
    })
    .expect("a thread to run Cove on");
    assert_eq!(format!("{interpreted:?}"), format!("{lowered:?}"));
    assert_eq!(
        lowered.expect_err("the capability was not granted").message,
        "`console.println` requires the `console` capability, which this run was not granted"
    );
}

/// An operation on a resource handle is dispatched by the handle, not by
/// a module name the instruction carries.
///
/// The `Writer` and the `Reader` are two handles of two kinds issued by
/// one host, and the run writes through one and reads back through the
/// other, so an answer that came from anywhere but the handle each call
/// stood on would be a different file or no file at all.
#[test]
fn a_resource_operation_is_dispatched_by_the_handle_it_stands_on() {
    let outcome = agree(
        "use console.println\nuse files\n\nexport fn main() -> Result<Unit, Error> {\n  let writer = files.create(\"notes.txt\")?\n  writer.writeLine(\"first\")?\n  writer.write(\"second\")?\n  writer.close()?\n  let reader = files.open(\"notes.txt\")?\n  println(\"one {reader.readLine()?.unwrapOr(\"\")}\")?\n  println(\"two {reader.readLine()?.unwrapOr(\"\")}\")?\n  reader.close()?\n  Ok(())\n}\n",
    );
    assert_eq!(outcome.output, "one first\ntwo second\n");
}

/// The instruction that dispatched them, so a call that stopped being a
/// resource call would fail here rather than go on passing.
#[test]
fn a_resource_call_is_lowered_to_the_instruction_that_routes_by_a_handle() {
    assert_eq!(
        main_of(
            "use files\n\nexport fn main() -> Result<Unit, Error> {\n  let writer = files.create(\"notes.txt\")?\n  writer.writeLine(\"first\")?\n  writer.close()\n}\n"
        ),
        "fn m.main arity=0 frame=0/1 -> value\n\
         \x20  0  const Str(\"notes.txt\")\n\
         \x20  1  call-host files.create argc=1\n\
         \x20  2  try\n\
         \x20  3  store 0\n\
         \x20  4  load 0\n\
         \x20  5  const Str(\"first\")\n\
         \x20  6  call-resource writeLine argc=1\n\
         \x20  7  try\n\
         \x20  8  pop\n\
         \x20  9  load 0\n\
         \x20 10  call-resource close argc=0\n\
         \x20 11  return\n"
    );
}

/// A handle that outlived what it named fails inside the host, where the
/// only record of what is still open lives — and fails the same way on
/// both backends, which is what `tests/e2e:fail_http_stale_handle` is in
/// the corpus for.
#[test]
fn a_stale_handle_fails_the_same_way_on_both_backends() {
    let outcome = agree(
        "use files\n\nexport fn main() -> Result<Unit, Error> {\n  let writer = files.create(\"notes.txt\")?\n  writer.close()?\n  writer.close()?\n  Ok(())\n}\n",
    );
    assert_eq!(
        outcome.error().message,
        "`files.Writer#1` is closed, so `close` has nothing to act on"
    );
}

/// A value of a type a host module declares is an ordinary struct on
/// both backends.
///
/// `Interpreter::init_host_type` builds a `Value::Struct` named
/// `{module}.{Name}` with `opaque` false and nothing else, so the VM's
/// `make-struct` is the whole of it: the shape table reads the qualified
/// name off the instruction, and `is_opaque` answers false because no
/// module of this package declares `Response`.
#[test]
fn a_type_a_host_declares_is_an_ordinary_struct_on_both_backends() {
    let source = "use http\n\nexport fn main() -> String {\n  let r = http.Response(status: 200, body: \"ok\")\n  \"{r} {r.status}\"\n}\n";
    assert_eq!(
        agree(source).value(),
        "Str(\"Response(status: 200, body: ok) 200\")"
    );
    assert!(
        main_of(source)
            .lines()
            .any(|line| line.contains("make-struct http.Response fields=status,body")),
        "the host type is built rather than asked for:\n{}",
        main_of(source)
    );
}
