//! Fuel and cancellation, and whose budget bounds what.
//!
//! A budget is not a backend's, so every case here runs on both. The numbers
//! differ — ADR 0024 makes a fuel limit non-portable between the two — but
//! that a limit is reached, and which invocation it bounds, does not.

use super::*;
use crate::value::Repr;

/// A program whose one function costs fuel to run: a loop is charged at
/// its back edge on the tree walk and by the block on the VM, so both
/// backends spend something measurable on it.
const COUNTS: &str = "\
/// Adds every number below `n`.
export fn work(n: Int) -> Int {
  var total = 0
  for i in 0..n {
    total = total + i
  }
  total
}
";

/// Invokes `m.work` `times` times on one backend and answers how each
/// invocation came out, together with what the registry's budget holds
/// afterwards.
///
/// `session` is a budget the registry is arranged with before anything
/// runs, which is what a `cove run` gets; `per_call` is one every
/// invocation is handed for itself. The point of the test below is that
/// those two are different things, so this can produce either.
fn invocations(
    checked: &Arc<Checked>,
    sources: &Arc<SourceMap>,
    lowered: &Arc<cove_ir::Program>,
    on_vm: bool,
    session: Option<Limits>,
    per_call: Option<Limits>,
    times: usize,
) -> (Vec<Option<String>>, u64) {
    let buffer = Buffer::default();
    let hosts = hosts(&buffer, session.map(Budget::new));
    let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
    let argument = || vec![Value(Repr::Int(200))];
    let described = |outcome: Result<Value, RuntimeError>| outcome.err().map(|e| e.message);
    let outcomes = if on_vm {
        let mut vm = Vm::new(&runtime, &hosts, lowered);
        (0..times)
            .map(|_| {
                described(match &per_call {
                    Some(limits) => {
                        vm.invoke_within(Budget::new(limits.clone()), "m", "work", argument())
                    }
                    None => vm.invoke("m", "work", argument()),
                })
            })
            .collect()
    } else {
        let mut interpreter = Interpreter::new(&runtime);
        (0..times)
            .map(|_| {
                described(match &per_call {
                    Some(limits) => interpreter.invoke_within(
                        Budget::new(limits.clone()),
                        "m",
                        "work",
                        argument(),
                    ),
                    None => interpreter.invoke("m", "work", argument()),
                })
            })
            .collect()
    };
    let spent = hosts.with_budget(|budget| budget.fuel_spent()).unwrap_or(0);
    (outcomes, spent)
}

/// A budget bounds one invocation, and the registry's own bounds every
/// invocation. Both backends, because a limit is not a backend's.
///
/// The limit here is measured rather than written down: one invocation is
/// run first to find what it costs, and the bound is that and a half. ADR
/// 0024 makes a fuel limit non-portable between backends — the two charge
/// different amounts for the same work and neither owes the other a number
/// — so a constant would have been a constant for one of them.
#[test]
fn a_budget_handed_to_an_invocation_bounds_that_invocation_alone() {
    let (sources, checked) = checked_module(COUNTS);
    let lowered = Arc::new(cove_ir::lower::lower(&checked).expect("the program lowers"));

    for on_vm in [false, true] {
        let backend = if on_vm { "the VM" } else { "the interpreter" };
        let (first, once) = crate::on_cove_stack(|| {
            invocations(
                &checked,
                &sources,
                &lowered,
                on_vm,
                Some(Limits::default()),
                None,
                1,
            )
        })
        .expect("a thread to run Cove on");
        assert_eq!(first, vec![None], "on {backend}");
        assert!(once > 0, "an invocation costs fuel on {backend}");

        let bound = Limits {
            fuel: Some(once + once / 2),
            ..Limits::default()
        };

        // What the registry was arranged with is spent over the whole
        // life of the backend, so a second invocation of the same work
        // has half a run's worth of fuel left and stops.
        let (session, _) = crate::on_cove_stack(|| {
            invocations(
                &checked,
                &sources,
                &lowered,
                on_vm,
                Some(bound.clone()),
                None,
                3,
            )
        })
        .expect("a thread to run Cove on");
        assert_eq!(session[0], None, "on {backend}");
        assert!(
            session[1]
                .as_deref()
                .is_some_and(|why| why.contains("fuel")),
            "on {backend}: {session:?}"
        );

        // The same limit handed to each invocation bounds each of them,
        // on one backend that was built once. What the registry holds
        // afterwards is the last invocation's spend and not the three
        // added up, which is what makes reading it per request possible.
        let (each, spent) = crate::on_cove_stack(|| {
            invocations(&checked, &sources, &lowered, on_vm, None, Some(bound), 3)
        })
        .expect("a thread to run Cove on");
        assert!(each.iter().all(Option::is_none), "on {backend}: {each:?}");
        assert_eq!(spent, once, "on {backend}");
    }
}

// ------------------------------------------- where a run is stopped

/// Fuel exhaustion stops the VM.
///
/// The two backends do not stop at the same point and are not asked to:
/// ADR 0019 makes `fuel_spent` backend-specific, because an instruction
/// is not an AST node. What both must do is stop, with the message the
/// shared budget writes.
#[test]
fn fuel_exhaustion_stops_both_backends() {
    let (sources, checked) = checked_module(
        "export fn main() -> Int {\n  var total = 0\n  var i = 0\n  while i < 1000000 {\n    total += i\n    i += 1\n  }\n  total\n}\n",
    );
    let limits = Limits {
        fuel: Some(1_000),
        ..Limits::default()
    };
    let (interpreted, lowered) = on_both(&checked, &sources, "m", Some(limits));
    assert_eq!(
        interpreted.error().message,
        "execution stopped: fuel budget of 1000 exhausted"
    );
    assert_eq!(
        lowered.error().message,
        "execution stopped: fuel budget of 1000 exhausted"
    );
}

/// Cancellation stops the VM at its next safepoint.
#[test]
fn cancellation_stops_both_backends() {
    let (sources, checked) = checked_module(
        "export fn main() -> Int {\n  var total = 0\n  var i = 0\n  while i < 1000000 {\n    total += i\n    i += 1\n  }\n  total\n}\n",
    );
    // Cancelled before the first instruction, so both backends stop at
    // the first safepoint they reach rather than at a moment a test would
    // have to race them for.
    let cancelled = || {
        let cancellation = Cancellation::new();
        let budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
        cancellation.cancel();
        budget
    };
    let stopped = crate::on_cove_stack(|| {
        (
            interpreted(&checked, &sources, "m", Some(cancelled())),
            lowered(&checked, &sources, "m", Some(cancelled())),
        )
    })
    .expect("a thread to run Cove on");
    assert_eq!(
        stopped.0.error().message,
        "execution stopped: the run was cancelled"
    );
    assert_eq!(
        stopped.1.error().message,
        "execution stopped: the run was cancelled"
    );
}
