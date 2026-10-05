//! `cove-edge-compare`: stage 1 of [`examples/edge/compare`](../../../compare/README.md)
//! — the time one call of a tenant's `handle` takes, in-process, with no
//! HTTP and no process start in it.
//!
//! ```text
//! cargo run --release -p cove-edge --bin cove-edge-compare -- [--batches 7] [--min-ms 300]
//! ```
//!
//! Each case is a request the load tests make — `crunch` at `n` = 2,000,
//! 20,000 and 150,000, and `hello?name=Cove` — and each is timed four ways:
//!
//! - `edge`: what the server pays per request, less the socket: build the
//!   `edge.Request` value, a fresh isolate (`Deployed::isolate`), its run
//!   under the tenant's budget (`invoke_within_parkable`), and the body read
//!   back out of the `edge.Response`.
//! - `vm`: one `Vm` over the same lowered program, reused, the argument
//!   built once and cloned: the encoded VM's time for the call alone.
//! - `native`: the same, on [`cove_runtime::compile_native`]'s machine code
//!   (`Vm::with_native`), where this host and build have the tier.
//!
//! A batch is enough calls to last `--min-ms`; the time per call is the
//! batch's time over its calls, and what is printed is the median of
//! `--batches` batches and their spread. Every mode's answer is compared
//! with the first one's before anything is timed, and printed, so that the
//! Go baseline (`compare/go`, `edge-go bench`) can be held to the same bytes.
//!
//! `--breakdown` times the parts of the `edge` path one at a time, each in
//! a loop of its own, for every case that is not `crunch`: building the
//! `edge.Request` value, a fresh isolate built and dropped, the budget, a
//! fresh isolate and its run, the run on one isolate reused, and reading the
//! `edge.Response` back the way the server's `response_of` does.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_edge::deploy::{self, request_value, DeployOptions};
use cove_edge::hosts::SCHEMAS;
use cove_edge::{Latency, State};
use cove_runtime::{Budget, HostRegistry, Limits, Runtime, Step, Value, Vm};
use cove_sema::HostSchemas;

const USAGE: &str =
    "usage: cove-edge-compare [--batches 7] [--min-ms 300] [--tenants DIR] [--breakdown]";

/// One case: a tenant, and the request it is asked.
struct Case {
    label: &'static str,
    tenant: &'static str,
    path: &'static str,
    query: &'static [(&'static str, &'static str)],
}

const CASES: [Case; 4] = [
    Case {
        label: "crunch n=2000",
        tenant: "crunch",
        path: "/",
        query: &[("n", "2000")],
    },
    Case {
        label: "crunch n=20000",
        tenant: "crunch",
        path: "/",
        query: &[("n", "20000")],
    },
    Case {
        label: "crunch n=150000",
        tenant: "crunch",
        path: "/",
        query: &[("n", "150000")],
    },
    Case {
        label: "hello name=Cove",
        tenant: "hello",
        path: "/",
        query: &[("name", "Cove")],
    },
];

fn main() {
    let mut batches = 7usize;
    let mut min_ms = 300u64;
    let mut tenants = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tenants");
    let mut breakdown = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| fail("a flag takes a value"));
        match arg.as_str() {
            "--batches" => batches = value().parse().unwrap_or_else(|_| fail("--batches N")),
            "--min-ms" => min_ms = value().parse().unwrap_or_else(|_| fail("--min-ms N")),
            "--tenants" => tenants = value().into(),
            "--breakdown" => breakdown = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown argument `{other}`")),
        }
    }
    let options = DeployOptions {
        tenants: tenants.clone(),
        latency: Latency {
            min: Duration::ZERO,
            max: Duration::ZERO,
        },
        quiet: true,
        blocking_upstream: false,
    };
    let all = deploy::deploy_all(&options).unwrap_or_else(|e| fail(&e));
    let min = Duration::from_millis(min_ms);

    println!("mode    case              median ns/call   min..max ns/call   calls/batch");
    for case in &CASES {
        let tenant = all
            .iter()
            .find(|t| t.name == case.tenant)
            .unwrap_or_else(|| fail(&format!("no tenant `{}`", case.tenant)));
        let State::Deployed(deployed) = &tenant.state else {
            fail(&format!("`{}` was not deployed", case.tenant))
        };
        let query: Vec<(String, String)> = case
            .query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        // The lowered program again, for a `Vm` of our own: the deploy's is
        // inside its `PreparedProgram`.
        let compiled = deploy::compile(&tenants, &deployed.module).unwrap_or_else(|e| fail(&e));
        let lowered = cove_ir::lower_entry(
            &compiled.program,
            &compiled.sources,
            &HostSchemas::only(SCHEMAS),
            &deployed.module,
            &deployed.function,
        )
        .unwrap_or_else(|_| fail("does not lower"));
        let hosts = Arc::new(deploy::registry(tenant, &options));
        let runtime = Runtime::new(
            Arc::new(compiled.program),
            Arc::new(compiled.sources),
            Arc::clone(&hosts),
        );
        let native = cove_runtime::compile_native(&lowered).ok();
        let (module, function) = (deployed.module.as_str(), deployed.function.as_str());

        // edge: the server's per-request path, less the socket.
        let limits = tenant.limits.clone();
        let mut edge = || -> String {
            let argument = request_value("GET", case.path, &query, "");
            let vm = deployed.isolate();
            match vm.invoke_within_parkable(
                Budget::new(limits.clone()),
                module,
                function,
                vec![argument],
            ) {
                Step::Answered(_, Ok(value)) => body(&value),
                Step::Answered(_, Err(e)) => fail(&format!("{}: {:?}", case.label, e)),
                _ => fail("a pure tenant parked or yielded"),
            }
        };
        let expected = edge();

        let argument = request_value("GET", case.path, &query, "");
        let mut vm = Vm::new(&runtime, &hosts as &HostRegistry, &lowered);
        let mut reused = || -> String {
            body(
                &vm.invoke(module, function, vec![argument.clone()])
                    .unwrap_or_else(|e| fail(&format!("{e:?}"))),
            )
        };
        check(case, "vm", &expected, &reused());

        print_row("edge", case, measure(&mut edge, min, batches));
        print_row("vm", case, measure(&mut reused, min, batches));
        match &native {
            Some(native) => {
                let mut vm = Vm::with_native(&runtime, &hosts as &HostRegistry, &lowered, native);
                let mut compiled = || -> String {
                    body(
                        &vm.invoke(module, function, vec![argument.clone()])
                            .unwrap_or_else(|e| fail(&format!("{e:?}"))),
                    )
                };
                check(case, "native", &expected, &compiled());
                print_row("native", case, measure(&mut compiled, min, batches));
            }
            None => println!(
                "native  {:<17} (no native tier on this host or build)",
                case.label
            ),
        }
        if let Some(native) = &native {
            let refused: Vec<&str> = native.refusals().iter().map(|r| r.name.as_str()).collect();
            println!(
                "        {:<17} native: {} of {} reachable fn compiled; refused: {}",
                case.label,
                native.compiled(),
                native.reachable(),
                if refused.is_empty() {
                    "-".to_string()
                } else {
                    refused.join(", ")
                }
            );
        }
        println!("        {:<17} answer: {:?}", case.label, expected);
        if breakdown && case.tenant != "crunch" {
            print_breakdown(case, deployed, &tenant.limits, &query, min, batches);
        }
    }
}

/// `--breakdown`: each part of the `edge` path, timed alone.
fn print_breakdown(
    case: &Case,
    deployed: &deploy::Deployed,
    limits: &Limits,
    query: &[(String, String)],
    min: Duration,
    batches: usize,
) {
    let (module, function) = (deployed.module.as_str(), deployed.function.as_str());
    let answered = |step: Step| match step {
        Step::Answered(vm, Ok(value)) => (vm, value),
        Step::Answered(_, Err(e)) => fail(&format!("{}: {:?}", case.label, e)),
        _ => fail("a pure tenant parked or yielded"),
    };
    let part = |name: &str, call: &mut dyn FnMut() -> String| {
        print_row(&format!("  {name}"), case, measure(call, min, batches));
    };
    part("request_value", &mut || {
        std::hint::black_box(request_value("GET", case.path, query, ""));
        String::new()
    });
    part("isolate+drop", &mut || {
        std::hint::black_box(deployed.isolate());
        String::new()
    });
    part("budget", &mut || {
        std::hint::black_box(Budget::new(limits.clone()));
        String::new()
    });
    // The isolate is built and dropped inside this loop too, so the run
    // alone is this row less `isolate+drop`'s.
    let argument = request_value("GET", case.path, query, "");
    part("isolate+invoke", &mut || {
        let vm = deployed.isolate();
        let step = vm.invoke_within_parkable(
            Budget::new(limits.clone()),
            module,
            function,
            vec![argument.clone()],
        );
        std::hint::black_box(answered(step).1);
        String::new()
    });
    let mut reused = Some(deployed.isolate());
    part("invoke, isolate reused", &mut || {
        let vm = reused.take().expect("handed back by the last run");
        let step = vm.invoke_within_parkable(
            Budget::new(limits.clone()),
            module,
            function,
            vec![argument.clone()],
        );
        let (vm, value) = answered(step);
        reused = Some(vm);
        std::hint::black_box(value);
        String::new()
    });
    let (_, value) = answered(deployed.isolate().invoke_within_parkable(
        Budget::new(limits.clone()),
        module,
        function,
        vec![argument.clone()],
    ));
    part("response_of", &mut || {
        let status = value.field("status").and_then(Value::as_int).unwrap_or(500);
        let content_type = value
            .field("contentType")
            .and_then(Value::as_str)
            .unwrap_or("text/plain")
            .to_string();
        let body = value
            .field("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        std::hint::black_box((status, content_type, body));
        String::new()
    });
}

/// The `body` field of an `edge.Response`.
fn body(value: &Value) -> String {
    value
        .field("body")
        .and_then(Value::as_str)
        .unwrap_or_else(|| fail(&format!("not an edge.Response: {value}")))
        .to_string()
}

fn check(case: &Case, mode: &str, expected: &str, got: &str) {
    if expected != got {
        fail(&format!(
            "{}: {mode} answered {got:?}, edge answered {expected:?}",
            case.label
        ));
    }
}

/// Median, min and max nanoseconds per call over `batches` batches, and the
/// calls in each batch.
fn measure(
    call: &mut dyn FnMut() -> String,
    min: Duration,
    batches: usize,
) -> (f64, f64, f64, u64) {
    // Warm up, and size a batch to last at least `min`.
    let mut calls = 1u64;
    loop {
        let started = Instant::now();
        for _ in 0..calls {
            std::hint::black_box(call());
        }
        if started.elapsed() >= min / 4 {
            break;
        }
        calls *= 2;
    }
    calls *= 4;
    let mut per: Vec<f64> = (0..batches.max(1))
        .map(|_| {
            let started = Instant::now();
            for _ in 0..calls {
                std::hint::black_box(call());
            }
            started.elapsed().as_nanos() as f64 / calls as f64
        })
        .collect();
    per.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (per[per.len() / 2], per[0], per[per.len() - 1], calls)
}

fn print_row(mode: &str, case: &Case, (median, min, max, calls): (f64, f64, f64, u64)) {
    println!(
        "{mode:<7} {:<17} {median:>14.0}   {min:>8.0}..{max:<8.0}   {calls}",
        case.label
    );
}

fn fail(message: &str) -> ! {
    eprintln!("cove-edge-compare: {message}\n{USAGE}");
    std::process::exit(2)
}
