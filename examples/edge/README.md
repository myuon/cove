# edge — a multi-tenant server of parked isolates

A Workers-style HTTP server whose tenants are Cove programs. Each directory
under `tenants/` is one tenant; `tenants/cove.toml` grants each one its
capabilities; a request to `/<tenant>/...` runs that tenant's
`handle(request: edge.Request) -> edge.Response` in an isolate of its own.
Four worker threads serve every tenant, and a tenant that waits on a slow
upstream does not hold one of them while it waits: the run **parks**, and
resumes on whichever worker is free when the answer arrives.

It is a demonstration of the embedding API this repository grew in one week:

- [`PreparedProgram`](../../crates/cove-runtime/src/vm/mod.rs) and
  `OwnedVm::new` (#570): a tenant is parsed, checked, lowered, encoded and
  verified **once**, at startup; an isolate per request costs about 3 µs.
- [`OwnedVm`, `ParkedVm`, `Step`, `HostApi::call_parkable`,
  `HostAnswer::Pending`](../../crates/cove-runtime/src/vm/parked.rs)
  ([ADR 0080](../../docs/adr/0080-a-host-call-may-answer-pending.md)): a
  host call that answers pending hands the run back as a `Send` value, which
  waits in a timer heap rather than on a thread.
- The pacing collector of
  [ADR 0081](../../docs/adr/0081-a-run-collects-when-it-has-allocated-its-allowance.md),
  which is what keeps a resident (pooled) isolate's heap from growing.

std only — no async runtime, no HTTP crate. `host/` is a workspace member, so
`cargo t` runs `host/tests/server.rs`, which starts the server in-process on a
free port and asks it over TCP.

## Running it

```console
$ cargo run --release -p cove-edge -- --port 8787
cove-edge: deploying tenants from …/examples/edge/tenants
  aggregate  requires [upstream]  granted [upstream]  deployed: 221 fn, checked in 16.9 ms, prepared in 1.4 ms, isolate 3 us
  counter    requires [kv, log]  granted [kv, log]  deployed: 218 fn, checked in 16.0 ms, prepared in 1.2 ms, isolate 3 us
  greedy     requires [kv, upstream]  granted [kv]  REFUSED: `greedy.handle` requires `upstream`, which cove.toml does not grant
  hello      requires [-]  granted [-]  deployed: 220 fn, checked in 13.9 ms, prepared in 0.9 ms, isolate 3 us

listening on http://127.0.0.1:8787 — 4 worker thread(s), a fresh isolate per request, upstream latency 20..100 ms (parked), open-file limit 1048576

try:
  curl -s http://127.0.0.1:8787/aggregate/
  curl -s http://127.0.0.1:8787/counter/home
  curl -s http://127.0.0.1:8787/hello/?name=Cove
  curl -s http://127.0.0.1:8787/_stats
```

`requires` is what the checker derived from each entry's call graph
(`FnEntry::required_capabilities`); `granted` is `allow` in `tenants/cove.toml`,
and nothing else. `hello` requires nothing: building an `edge.Response`
initializes a type the `edge` schema declares, which is not a call into the
host (see [what was awkward](#what-was-awkward) item 2). `greedy` asks
for `upstream` without being granted it, so it is refused **at deploy** and the
other three start. Flags: `--workers N`, `--latency MIN..MAX` (ms),
`--pool N` (resident isolates instead of fresh ones), `--blocking-upstream`
(the control: `upstream.get` sleeps on the worker instead of parking),
`--quiet` (no `log.info` lines).

## The tenants

| tenant | grant | what it shows |
| --- | --- | --- |
| [`hello`](tenants/hello/hello.cove) | — | pure; `/hello/spin` loops until the tenant's `fuel = 2000000` stops it |
| [`counter`](tenants/counter/counter.cove) | `kv`, `log` | state that outlives the isolate lives behind a capability, per tenant |
| [`aggregate`](tenants/aggregate/aggregate.cove) | `upstream` | three slow calls in a row; the run parks at each; `max_host_calls = 8` per request, however many are in flight |
| [`greedy`](tenants/greedy/greedy.cove) | `kv` | over-reaches for `upstream` and is not deployed |

The contract is a host module, [`edge`](host/src/hosts.rs), whose schema
declares `Request { method, path, query: Map<String, String>, body }` and
`Response { status, contentType, body }` and no operations. It is handed to
the checker with the other three, so a tenant that misspells a field is
refused at deploy with the checker's own diagnostic.

## A curl walkthrough

Every response below is what the server above answered, copied verbatim.

```console
$ curl -i 'http://localhost:8787/hello/?name=Cove'
HTTP/1.1 200 OK
Content-Type: text/plain
Content-Length: 21
Connection: close

Hello, Cove! (GET /)
```

A tenant's bug is one failed request. The error is the runtime's own
diagnostic, with the tenant's source line:

```console
$ curl -i 'http://localhost:8787/hello/spin'
HTTP/1.1 500 Internal Server Error
Content-Type: text/plain
Content-Length: 365
Connection: close

error[cove::runtime]: execution stopped: fuel budget of 2000000 exhausted
  --> hello/hello.cove:23:3
   |
23 |   while turns >= 0 {
   |   ^^^^^^^^^^^^^^^^^^
  --> hello/hello.cove:10:12
   |
10 |     return spin()
   |            ^^^^^^ called from here
  rule: ADR 0001: CPU, time, concurrency, and host-call limits are runtime controls, not termination proofs.
```

Every request is a fresh isolate, so the count lives in the tenant's `kv`
store; the server prints the `log.info` lines (`[counter] GET /home -> 1`, …):

```console
$ curl -s 'http://localhost:8787/counter/home'
/home has been visited 1 time(s)
$ curl -s 'http://localhost:8787/counter/home'
/home has been visited 2 time(s)
$ curl -s 'http://localhost:8787/counter/about'
/about has been visited 1 time(s)
```

`aggregate` parks three times per request. The bracketed figure is the latency
the simulated upstream chose for that call:

```console
$ curl -s 'http://localhost:8787/aggregate/'
weather: sunny, 21C [77 ms]
stocks: COVE +3.2% [44 ms]
news: parked isolates resume on any thread [43 ms]
$ curl -s 'http://localhost:8787/aggregate/?services=weather,fail-db,news'
weather: sunny, 21C [20 ms]
fail-db: unavailable (`fail-db` is down)
news: parked isolates resume on any thread [45 ms]
```

The refused tenant, and one that does not exist:

```console
$ curl -i 'http://localhost:8787/greedy/'
HTTP/1.1 503 Service Unavailable
Content-Type: text/plain
Content-Length: 102
Connection: close

tenant `greedy` was not deployed: `greedy.handle` requires `upstream`, which cove.toml does not grant
$ curl -i 'http://localhost:8787/nobody/'
HTTP/1.1 404 Not Found
Content-Type: text/plain
Content-Length: 25
Connection: close

no tenant named `nobody`
```

`/_stats` (add `?reset` to forget the peaks and the latency samples):

```console
$ curl -s http://localhost:8787/_stats
{
  "uptime_s": 18.8,
  "workers": 4,
  "isolates": "a fresh isolate per request",
  "served": 10007,
  "errors": 1,
  "in_flight": 0,
  "in_flight_peak": 1000,
  "parked": 0,
  "parked_peak": 1000,
  "parks": 30006,
  "queue_peak": 53,
  "live_isolates": 0,
  "pooled_isolates": 0,
  "isolate_heap_bytes": {"mean": 1647, "max": 1760},
  "latency_ms": {"p50": 181.27, "p99": 270.44, "max": 314.48, "samples": 10000},
  "rss_kib": 65840,
  "peak_rss_kib": 65840,
  "tenants": {
    "aggregate": {"state": "deployed", "served": 10002, "errors": 0},
    …
```

## Load

```console
$ cargo run --release -p cove-edge --bin cove-edge-load -- --path /aggregate/ --concurrency 1000 --requests 10000
10000 requests to http://127.0.0.1:8787/aggregate/, 1000 in flight, 8 client thread(s)
  answered 10000 (10000 with 200), 0 failed to connect or read, in 2.00 s: 4994 req/s
  latency ms: p50 182.0  p90 234.6  p99 270.5  max 307.9
server /_stats after the run:
…
```

The load generator is std too: each client thread owns its share of the
connections and polls them without blocking, so it holds ten thousand
requests in flight on eight threads.

### Numbers

`--release`, macOS x86-64 (16 hardware threads), server with **4 workers**,
`cove-edge-load` on the same machine, one connection per request, one run per
row. `aggregate` makes three `upstream.get` calls per request, each 20–100 ms
(mean 60) unless the row says otherwise, so 180 ms is the floor of its
latency. Peak RSS growth is the server's peak RSS minus its RSS after warm-up,
divided by the peak number of requests in flight.

| workload | isolates | in flight (peak parked) | throughput | p50 / p99 | peak RSS growth per in-flight request |
| --- | --- | ---: | ---: | ---: | ---: |
| `hello`, 10,000 requests | fresh | 32 (0) | 30,455 req/s | 0.8 / 1.5 ms | — |
| `hello`, 10,000 requests | pool of 64 | 32 (0) | 31,631 req/s | 0.7 / 1.5 ms | — |
| `aggregate`, 10,000 requests | fresh | 1,000 (1,000) | 4,994 req/s | 182.0 / 270.5 ms | 38 KB |
| `aggregate`, 10,000 requests | pool of 64 | 1,000 (1,000) | 4,972 req/s | 182.1 / 272.9 ms | 90 KB |
| `aggregate`, 50 ms upstream | fresh | 10,000 offered (6,168) | 13,547 req/s | 232.3 / 356.9 ms | 30 KB |
| `aggregate`, 1 s upstream | fresh | **10,000 (10,000)** | 2,692 req/s | 3,309 / 3,509 ms | 31 KB |
| `aggregate`, 300 requests, **upstream blocks the worker** | fresh | 100 (0) | **7 req/s** | 14,633 / 15,340 ms | — |
| `aggregate`, 300 requests, parked (the same, as control) | fresh | 100 (100) | **218 req/s** | 504 / 507 ms | — |

What the rows say:

- **Ten thousand requests waiting at once, on four threads.** With a 1 s
  upstream every request was parked at the same time (`parked_peak` 10,000),
  and the server's RSS grew by 31 KB per waiting request — a parked run is
  18–22 KB by ADR 0080's and 0081's own measurements, and the rest is the
  socket, the request and the queue entry. With a 50 ms upstream the first
  requests finish before the generator has opened the last connection, so the
  peak is 6,168.
- **Parking is the whole difference.** The last two rows are the same
  workload, the same four workers, and the same timer; the only change is
  `--blocking-upstream`, which answers `upstream.get` by sleeping on the
  worker the way a host that never heard of ADR 0080 would. 7 against 218
  requests per second, and the blocking server's latency is the queue.
- **A fresh isolate per request is as fast as a pool.** `hello` costs the same
  either way (30.5k against 31.6k req/s, inside one run's noise; the TCP
  connection per request is most of it), and `OwnedVm::new` over a
  `PreparedProgram` measures 3 µs at deploy. The pool saves nothing worth its
  hazard, so per-request is the default; a pooled isolate's heap does grow to
  the collector's allowance (`isolate_heap_bytes` mean 61 KB pooled against
  360 B fresh on `hello`), which ADR 0081 is what bounds.

**One caveat about this machine's clock.** Every timed wait in an idle process
here overshoots by 125–150 ms — `std::thread::sleep(20 ms)` takes about
170 ms, measured with a standalone program, and setting the thread's QoS to
user-interactive does not change it. Under load the timer thread is never
idle and wakes on time (the 1,000-in-flight rows sit on the 180 ms floor), but
a single `curl` to `aggregate` takes about 0.45 s instead of 0.18 s, the
1 s-upstream row is 3.3 s rather than 3.0 s, and the two 100-in-flight rows
both pay it — which is why they are compared with each other and with nothing
else.

Between two 10,000-connection runs, wait half a minute: the server closes each
connection, so macOS holds every one of them in `TIME_WAIT`, and a new
connection that lands on one can stall. The generator gives up on a request
after 30 s and counts it, rather than hanging.

## What was awkward

The most useful output of this demo. Ordered by how much each cost.

1. **A per-invocation budget was per *registry*, not per run, and concurrent
   runs share the registry — fixed ([issue
   #577](https://github.com/myuon/cove/issues/577)).**
   `OwnedVm::invoke_within_parkable(budget, …)` used to install `budget` in
   the registry's one budget slot (`HostRegistry::begin_run`), and a host call
   was charged to whatever budget the registry held when it was made.
   Observed: with one registry per tenant, 997 of 10,000 concurrent
   `aggregate` requests — three host calls each — failed with `execution
   stopped: host-call limit of 1000 exceeded`. The budget is the run's now:
   the `Vm` holds it, a spawned task is handed it, and a host call is charged
   to the budget of the run that made it, so `begin_run` is gone and
   `OwnedVm::meter` reads what one run spent. The workaround — a registry and
   a `Runtime` per isolate — is gone with it: each tenant has one registry
   and one `Runtime`, built at deploy and shared by every request in flight
   (`Deployed`, `host/src/deploy.rs`), and an isolate is the `OwnedVm` alone
   (2–3 µs at deploy, against 3 µs with its own registry). `aggregate` now
   carries `max_host_calls = 8`, which a thousand requests parked at once over
   that one registry each have to themselves: 20,000 requests at 1,000 in
   flight all answered 200, and
   `more_runs_wait_on_the_upstream_than_there_are_workers` (twenty parked at
   once, two calls each) would fail on the shared slot.
2. **Building a host-declared struct required the module's capability** —
   fixed since. `edge.Response(status: …)` was read as a call into `edge`
   (`call_capability` in `crates/cove-sema/src/resolve.rs`), and a name the
   schema declares no operation for falls back to the module's capability.
   So a pure tenant required `edge`, although the runtime never crosses the
   boundary to build a struct, and the server granted `edge` to every tenant
   to make up for it. The checker now asks the schema for a declared type
   before it asks for an operation, the same precedence the interpreter
   gives the name: initializing a type a module declares requires nothing,
   just as naming one of its enum cases (`http.Method.Get`) never did. No
   schema syntax was needed — a module with types and no operations already
   says "types only". What remains is item 3: a `cove` command with no
   schema for `edge` still cannot tell `edge.Response(…)` from an operation,
   so `cove outline` in `tenants/` still says `requires edge`.
3. **No `cove` command can be handed the server's schemas** (issue #151, which
   the rules example already names). `cove check` in `tenants/` reports nine
   `cove::type::host_type` warnings — every `edge.Request` and
   `edge.Response` unchecked — and `cove run hello` fails with three
   `cove::lower::unknown_type` errors ("the type of this expression was never
   settled"), which says nothing about the real reason (the entry takes an
   `edge.Request` only the server can supply). The server is the only full
   checker of its tenants.
   One more thing came out of that blindness, and it is fixed on this branch:
   without the schema, `match upstream.get(service) { Ok(answer) => … }`
   bound `answer` as a `Recovery` unknown — "an error was already reported" —
   in a package that reported none, and `crates/cove-sema/tests/settled.rs`'s
   corpus invariant failed on `aggregate.cove`. `variant_pattern`
   (`crates/cove-sema/src/typeck.rs`) now binds what `Ty::abstain` carries,
   the same `DynamicBoundary` a field read on that value already gets.
4. **`[run.<name>]` is the only grant table, so it was borrowed.**
   `tenants/cove.toml` reuses it because `cove_sema::config::parse` is public
   and `RunConfig` already carries `allow`, `fuel`, `deadline` and
   `max_host_calls` — exactly a tenant's grant. But `[run]` means "`cove run`
   can start this", and neither `cove check` nor anything else notices that
   `hello.handle(request: edge.Request)` is not a runnable entry.
5. **Each tenant re-checks the standard library** — a third of it fixed
   since. A ten-line handler reaches 218–221 functions, and deploying it cost
   14–17 ms of checking, all of it the standard library
   `cove_sema::stdlib::install` attaches to every package: deploying `hello`
   100 times in one process, by difference of prefix passes, was parse 5.6 ms,
   resolve 2.6, type-check 5.8, lower 0.8 and prepare under 0.3 — 15.0 ms,
   and the tenant's own share of the front end too small to measure. The
   parse is now done once per process (`stdlib::attach` keeps it), which
   makes the same deploy 9.1 ms: parse 0.3, resolve 2.9, type-check 5.1,
   lower 0.8. A thousand tenants would be about nine seconds rather than
   fifteen. The resolve and the type-check still run per tenant, because
   both run over the whole package at once; sharing them is
   [issue 569](https://github.com/myuon/cove/issues/569)'s separate
   compilation of the standard library.
6. **Composing a one-module package by hand is copied code.**
   `host/src/deploy.rs`'s `load` is the same walk as
   `examples/rules/host/src/lib.rs:913` (`collect`), for the same reason: an
   embedder decides what is in its package, and `cove_sema::package::load`
   loads a whole package root. A "load this directory as module `m`" helper
   would serve both.
7. **A pending answer is built as a `Value` to become a `Transfer`.**
   `upstream.get` answers `Result<String, Error>`; the timer thread builds
   `Value::ok(Value::string(…))` — an `Rc` value on a thread that never runs
   Cove — only to call `Transfer::of` on it (`host/src/server.rs`, in
   `run_timer`), because `Transfer` has no constructors for `Ok`/`Err` and its
   `Enum` case would mean guessing the builtin's type name.
8. **A parked run's deadline does not fire while it is parked.** `Limits`'
   deadline bounds the run "parked time included" (`OwnedVm::invoke_within_parkable`'s
   documentation), but nothing wakes a parked run to stop it: the deadline is
   noticed at the next safepoint after a resume. An upstream that never
   answers holds the run, and its socket, until the embedder gives up on it
   itself; this server's simulated upstream always answers, so it does not
   implement that.
9. **Small things in the language.** `"\n".join(lines) + "\n"` is refused
   (`+` is not defined for `String`) and wants a `let` and an interpolation;
   `kv.get` then `kv.put` is a lost update under concurrency, which is the
   host's to fix (`kv.increment`), and Cove has no way to say "these two host
   calls are one transaction".

What worked without friction is worth a line too: `Step`/`ParkedVm` did
exactly what ADR 0080 says, across threads, with no change to the tenant's
source; `PreparedProgram` made isolate-per-request the obvious default; the
runtime's error, rendered with the tenant's `SourceMap`, is a good 500 body;
and `FnEntry::required_capabilities` was all it took to refuse a tenant at
deploy.

## Not done

- Keep-alive, chunked bodies, TLS: one request per connection, `Connection:
  close`. The request is read on a worker, so a slow client holds one.
- A real outbound fetch: `upstream` is a timer heap that answers after the
  chosen latency. Swapping it for an I/O thread doing real requests changes
  `run_timer` and nothing else.
- Timeouts for parked runs (item 8).
