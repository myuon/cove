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
`cargo t` runs `host/tests/`, which start the server in-process on a
free port and ask it over TCP, and call `check` and `test` as functions.

## Running it

```console
$ cargo run --release -p cove-edge -- --port 8787
cove-edge: deploying tenants from …/examples/edge/tenants
  aggregate  requires [upstream]  granted [upstream]  deployed: 223 fn, checked in 43.8 ms, prepared in 2.9 ms, isolate 5 us
  counter    requires [kv, log]  granted [kv, log]  deployed: 220 fn, checked in 27.5 ms, prepared in 2.7 ms, isolate 12 us
  greedy     requires [kv, upstream]  granted [kv]  REFUSED: `greedy.handle` requires `upstream`, which cove.toml does not grant
  hello      requires [-]  granted [-]  deployed: 223 fn, checked in 28.3 ms, prepared in 1.8 ms, isolate 4 us
  impatient  requires [upstream]  granted [upstream]  deployed: 223 fn, checked in 21.9 ms, prepared in 2.4 ms, isolate 4 us
  proxy      requires [upstream]  granted [upstream]  fetch [127.0.0.1, localhost]  deployed: 222 fn, checked in 25.7 ms, prepared in 3.3 ms, isolate 3 us

listening on http://127.0.0.1:8787 — 4 worker thread(s), a fresh isolate per request, upstream latency 20..100 ms (parked), keep-alive (idle 5000 ms, 1000 requests per connection), open-file limit 1048576

try:
  curl -s 'http://127.0.0.1:8787/aggregate/'
  curl -s 'http://127.0.0.1:8787/counter/home'
  curl -s 'http://127.0.0.1:8787/hello/?name=Cove'
  curl -s 'http://127.0.0.1:8787/proxy/?url=http://127.0.0.1:8787/hello/'
  curl -s http://127.0.0.1:8787/_stats
```

(The check times are from a machine also running another build; on an idle
one they were 12–17 ms.)

`requires` is what the checker derived from each entry's call graph
(`FnEntry::required_capabilities`); `granted` is `allow` in `tenants/cove.toml`,
and nothing else. `hello` requires nothing: building an `edge.Response`
initializes a type the `edge` schema declares, which is not a call into the
host (see [what was awkward](#what-was-awkward) item 2). `greedy` asks
for `upstream` without being granted it, so it is refused **at deploy** and the
other five start. `fetch [...]` is `proxy`'s allowlist from
[`tenants/edge.toml`](tenants/edge.toml). `--fetchers N` sets the threads
that perform real fetches (default 4). Flags: `--workers N`, `--latency MIN..MAX` (ms),
`--pool N` (resident isolates instead of fresh ones), `--blocking-upstream`
(the control: `upstream.get` sleeps on the worker instead of parking),
`--quiet` (no `log.info` lines), `--no-keep-alive`, `--idle-timeout MS`,
`--max-requests N` (see [keep-alive](#keep-alive)).

## Checking and testing a tenant

`cove check` and `cove test` cannot see the server's host modules (issue #151,
closed by decision; [what was awkward](#what-was-awkward) item 3), so the
server ships its own. `check` prints the diagnostics `cove check` would,
checked against the schemas the server registers, then each tenant's verdict
— the same admission the server makes at deploy:

```console
$ cargo run --release -p cove-edge -- check
aggregate  requires [upstream]  granted [upstream]  ok
counter    requires [kv, log]  granted [kv, log]  ok
greedy     requires [kv, upstream]  granted [kv]  REFUSED: `greedy.handle` requires `upstream`, which cove.toml does not grant
hello      requires [-]  granted [-]  ok
impatient  requires [upstream]  granted [upstream]  ok
proxy      requires [upstream]  granted [upstream]  fetch [127.0.0.1, localhost]  ok
checked 5 module(s), 9 file(s) against the server's schemas; 6 tenant(s), 1 refused
$ echo $?
1
```

No warnings: `cove check` in the same directory reports nineteen. `test` runs
every `test fn` of each tenant's module the way `cove test` does — lowered
as an entry, on the VM, an `Err` is a failure pointing at its assertion — but
with the server's hosts (`kv` in memory, empty per test; `log` silent;
`upstream.get` at `--latency`, zero by default; `upstream.fetch` real and
filtered by the tenant's allowlist), the tenant's grant rather than
the test's derived one, and the tenant's limits. `aggregate` and `impatient`
share a module, so its test runs under each:

```console
$ cargo run --release -p cove-edge -- test
ok    aggregate  aggregate.aServiceThatIsDownIsOneLineOfTheAnswer
ok    counter    counter.countsEachPathApart
ok    hello      hello.greetsTheWorldWhenNobodyIsNamed
ok    hello      hello.greetsWhoeverTheQueryNames
ok    impatient  aggregate.aServiceThatIsDownIsOneLineOfTheAnswer
ok    proxy      proxy.aHostOffTheAllowlistIsRefused
ok    proxy      proxy.httpsIsRefused
ran 7 test(s), 7 passed
```

`cove test` in `tenants/` runs the same six tests and fails all six with
`cove::test::no_host`. A test that reaches a capability its tenant is not
granted fails before it runs, naming the grant: ``test `bad.logs` requires
`log`, which cove.toml does not grant tenant `bad` ``.

## The tenants

| tenant | grant | what it shows |
| --- | --- | --- |
| [`hello`](tenants/hello/hello.cove) | — | pure; `/hello/spin` loops until the tenant's `fuel = 2000000` stops it |
| [`counter`](tenants/counter/counter.cove) | `kv`, `log` | state that outlives the isolate lives behind a capability, per tenant |
| [`aggregate`](tenants/aggregate/aggregate.cove) | `upstream` | three slow calls in a row; the run parks at each; `max_host_calls = 8` per request, however many are in flight |
| `impatient` | `upstream` | `aggregate`'s code under `deadline = "300ms"`: a request parked at an upstream that has not answered by then is cancelled and answered 504 |
| [`proxy`](tenants/proxy/proxy.cove) | `upstream`, fetch `127.0.0.1`, `localhost` | `upstream.fetch(?url=)`: a real HTTP request from the fetch pool, to the hosts [`edge.toml`](tenants/edge.toml) allows it; `deadline = "500ms"` |
| [`greedy`](tenants/greedy/greedy.cove) | `kv` | over-reaches for `upstream` and is not deployed |

The contract is a host module, [`edge`](host/src/hosts.rs), whose schema
declares `Request { method, path, query: Map<String, String>, body }` and
`Response { status, contentType, body }` and no operations. It is handed to
the checker with the other three, so a tenant that misspells a field is
refused at deploy with the checker's own diagnostic. `upstream` has two
operations, both `Result<String, Error>` and both answered pending: `get`, a
simulated service by name (what the load tests use), and `fetch`, a real URL.

## A curl walkthrough

Every response below is what the server above answered, copied verbatim.
curl speaks HTTP/1.1, so every connection is kept alive; two URLs on one
command line share one connection:

```console
$ curl -i 'http://localhost:8787/hello/?name=Cove'
HTTP/1.1 200 OK
Content-Type: text/plain
Content-Length: 21
Connection: keep-alive

Hello, Cove! (GET /)
$ curl -sv 'http://localhost:8787/hello/?name=one' 'http://localhost:8787/hello/?name=two' 2>&1 | grep -E '^\* (Connected|Re-using)|^Hello'
* Connected to localhost (127.0.0.1) port 8787
Hello, one! (GET /)
* Re-using existing connection with host localhost
Hello, two! (GET /)
```

A tenant's bug is one failed request. The error is the runtime's own
diagnostic, with the tenant's source line:

```console
$ curl -i 'http://localhost:8787/hello/spin'
HTTP/1.1 500 Internal Server Error
Content-Type: text/plain
Content-Length: 365
Connection: keep-alive

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

`impatient` is the same code under `deadline = "300ms"`, and the `hang`
service answers after an hour. The run parks at `hang` and its deadline keeps
running; the timer wakes at the deadline rather than the answer, cancels the
parked run (`ParkedVm::cancel`, [ADR
0082](../../docs/adr/0082-a-parked-run-keeps-its-deadline.md)), and the
request is answered 504 with the runtime's own stop — the same error a running
run past its deadline reports — instead of holding its socket for an hour:

```console
$ curl -i 'http://localhost:8787/impatient/?services=weather,hang'
HTTP/1.1 504 Gateway Timeout
Content-Type: text/plain
Content-Length: 306
Connection: keep-alive

error[cove::runtime]: execution stopped: wall-clock deadline of 300ms exceeded
  --> aggregate/aggregate.cove:17:11
   |
17 |     match upstream.get(service) {
   |           ^^^^^^^^^^^^^^^^^^^^^
  rule: ADR 0001: CPU, time, concurrency, and host-call limits are runtime controls, not termination proofs.
```

`/_stats` counts these as `timeouts`.

`proxy` makes a **real** outbound request: `upstream.fetch(url)` is an HTTP/1.1
`GET` that one of the server's fetch threads (`--fetchers`, default 4;
[`host/src/fetch.rs`](host/src/fetch.rs)) performs while the run is parked,
answered `Pending` exactly as `upstream.get` is. Here it asks another tenant
on the same server — the parked run holds no worker, so a worker is free to
serve `hello` while `proxy` waits for it:

```console
$ curl -i 'http://localhost:8787/proxy/?url=http://127.0.0.1:8787/hello/?name=proxy'
HTTP/1.1 200 OK
Content-Type: text/plain
Content-Length: 68
Connection: keep-alive

http://127.0.0.1:8787/hello/?name=proxy said:
Hello, proxy! (GET /)
```

Where it may go is not the tenant's to decide. `cove.toml` grants `upstream`
— outbound calls at all — and [`tenants/edge.toml`](tenants/edge.toml) names
the hosts `fetch` may reach; the server's `upstream` host checks every URL
against that list at the boundary, so a URL the tenant builds at run time
cannot get past it. Off the list, the call is answered at once with an `Err`,
before anything is sent and without parking, and `proxy` answers it as it
answers any failed fetch:

```console
$ curl -i 'http://localhost:8787/proxy/?url=http://example.com/'
HTTP/1.1 502 Bad Gateway
Content-Type: text/plain
Content-Length: 80
Connection: keep-alive

`example.com` is not on tenant `proxy`'s fetch allowlist (127.0.0.1, localhost)
$ curl -s 'http://localhost:8787/proxy/?url=http://localhost:8787/nobody/'
`http://localhost:8787/nobody/` answered 404: no tenant named `nobody`
```

`https://` is refused the same way (no TLS here), and a tenant named in no
`edge.toml` table — `aggregate`, granted `upstream` — may fetch from nowhere.

A fetch of an upstream that never answers is `impatient`'s case with a real
socket: `proxy`'s `deadline = "500ms"` passes while the run is parked, the
parking lot cancels it, and — because nothing else wants it — **aborts the
fetch**, shutting the socket so the upstream sees its request abandoned
instead of a connection held for the fetch's 30 s read timeout. Here the
upstream is `nc`, which takes the request and says nothing:

```console
$ sleep 20 | nc -l 127.0.0.1 9999 &
$ curl -i 'http://localhost:8787/proxy/?url=http://127.0.0.1:9999/slow'
HTTP/1.1 504 Gateway Timeout
Content-Type: text/plain
Content-Length: 289
Connection: keep-alive

error[cove::runtime]: execution stopped: wall-clock deadline of 500ms exceeded
  --> proxy/proxy.cove:13:9
   |
13 |   match upstream.fetch(url) {
   |         ^^^^^^^^^^^^^^^^^^^
  rule: ADR 0001: CPU, time, concurrency, and host-call limits are runtime controls, not termination proofs.
$ curl -s http://localhost:8787/_stats | grep fetches
  "fetches": 2,
  "fetches_aborted": 1,
```

ADR 0082 deferred how a host learns that a cancelled run's pending work is
no longer wanted, and the answer here needed **no runtime API**. The
embedder already holds the request: the server takes it out of the parked
run (`ParkedVm::take_request`), gives the fetch an id and hands it to its own
pool, so when it cancels the run it tells the pool `abort(id)` — dropped if
still queued, its socket shut down if on the wire (`host/src/fetch.rs`,
`run_lot` in `host/src/server.rs`). A simulated `upstream.get` needs nothing:
its timer entry is the run's and goes with it. An embedder that leaves the
request inside the run has the other half already: `ParkedVm::cancel` drops
an untaken request, so a host can observe it with `Drop` on its request type.
`host/tests/fetch.rs` holds the upstream's side of it: removing the one
`abort` call turns that test red, the upstream still waiting ten seconds
later.

The refused tenant, and one that does not exist:

```console
$ curl -i 'http://localhost:8787/greedy/'
HTTP/1.1 503 Service Unavailable
Content-Type: text/plain
Content-Length: 102
Connection: keep-alive

tenant `greedy` was not deployed: `greedy.handle` requires `upstream`, which cove.toml does not grant
$ curl -i 'http://localhost:8787/nobody/'
HTTP/1.1 404 Not Found
Content-Type: text/plain
Content-Length: 25
Connection: keep-alive

no tenant named `nobody`
```

`/_stats` (add `?reset` to forget the peaks and the latency samples):

```console
$ curl -s http://localhost:8787/_stats
{
  "uptime_s": 5.5,
  "workers": 4,
  "isolates": "a fresh isolate per request",
  "served": 10,
  "errors": 2,
  "in_flight": 0,
  "in_flight_peak": 1,
  "parked": 0,
  "parked_peak": 1,
  "parks": 8,
  "timeouts": 1,
  "queue_peak": 1,
  "connections": 12,
  "keep_alive_reuses": 1,
  "idle_connections": 0,
  "idle_expired": 0,
  "fetches": 0,
  "fetches_aborted": 0,
  "live_isolates": 0,
  "pooled_isolates": 0,
  "isolate_heap_bytes": {"mean": 716, "max": 1760},
  "latency_ms": {"p50": 0.22, "p99": 467.78, "max": 467.78, "samples": 10},
  "rss_kib": 37568,
  "peak_rss_kib": 37568,
  "tenants": {
    "aggregate": {"state": "deployed", "served": 2, "errors": 0},
    …
```

`connections` and `keep_alive_reuses` count accepted connections and the
requests that arrived on one an earlier request had opened;
`idle_connections` is how many are waiting in the idle thread now, and
`idle_expired` how many it closed for waiting too long. `fetches` and
`fetches_aborted` are the fetch pool's.

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
requests in flight on eight threads. `--keep-alive` makes each of the
`--concurrency` connections ask its next request as soon as the last is
answered, instead of opening a connection per request.

### Keep-alive

HTTP/1.1 connections stay open by default: a response says `Connection:
keep-alive` unless the client asked for `close` (or spoke HTTP/1.0 without
asking for `keep-alive`), the connection has made `--max-requests` requests
(default 1,000; the last is answered `close`), or the server runs with
`--no-keep-alive`. A connection that waits longer than `--idle-timeout`
(default 5 s) for its next request is closed. Pipelined requests are answered
in order.

No thread waits on an idle connection. Between requests — and before the
first, so a client that connects and says nothing holds no worker either — a
connection is handed to one `edge-idle` thread ([`host/src/idle.rs`](host/src/idle.rs))
that `poll(2)`s every idle socket at once through the `libc` crate already in
the tree; one that becomes readable goes back on the run queue, and whichever
worker takes it reads the request. A worker still reads a request that has
started arriving with a 5 s timeout, so a client that sends half a head and
stops holds one for that long.

Same machine and setup as the table below, 10,000 requests, 1,000 in flight,
4 workers, two rounds each (the machine was also running another agent's
build, load average 6 to 10, so read the ratios rather than the totals):

| workload | connections opened | throughput | p50 / p99 |
| --- | ---: | ---: | ---: |
| `hello`, a connection per request | 10,000 | 25,438 / 20,735 req/s | 16.5 / 38.8, 21.2 / 51.0 ms |
| `hello`, **keep-alive** | 1,000 | **86,930 / 75,911 req/s** | 8.3 / 27.0, 10.0 / 30.8 ms |
| `aggregate`, a connection per request | 10,000 | 4,843 / 4,973 req/s | 182.5 / 271.9, 181.5 / 275.5 ms |
| `aggregate`, **keep-alive** | 1,000 | 4,921 / 4,634 req/s | 181.0 / 275.0, 182.0 / 270.0 ms |

Re-measured on this branch's tip — after the parking lot and the fetch pool
— at load average 13.7, the same picture: `hello` 24,623 / 13,112 req/s with a
connection per request against 81,151 / 50,354 with keep-alive; `aggregate`
4,542 / 4,872 against 4,934 / 4,561.

`hello` is where the connection was the cost, and keep-alive is three to four
times the throughput. `aggregate` is bound by its 180 ms floor — 1,000 in
flight over 0.18 s is 5,500 req/s at best — and the connection was never its
cost. At 32 in flight `hello` is 19,661 to 27,204 req/s with a connection per
request against 58,558 to 64,718 with keep-alive (three rounds each), so
handing every new connection through the idle thread costs no more than the
earlier table's 30,455 within this machine's noise.

**The half-minute wait between 10,000-connection runs is gone, and it was
not where it looked.** Two back-to-back runs of 20,000 requests at 10,000 in
flight (50 ms upstream) both answered every request with a connection per
request. With keep-alive the first version failed the second run's 13,718
connects with `EADDRNOTAVAIL`: once the client is the side that closes, *it*
holds the `TIME_WAIT`s, each for twice macOS's 15 s MSL, on an ephemeral range
of 16,384 ports. The generator now drops a kept-alive connection it has no
more use for with a reset (`SO_LINGER` 0), and four back-to-back runs —
keep-alive, keep-alive, a connection per request, keep-alive — answered all
80,000 requests, each keep-alive run opening 10,000 connections for 20,000
requests at 22,800–23,000 req/s against 13,600 without.

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

The rows above were measured before keep-alive, with a connection per
request. They used to come with a warning to wait half a minute between two
10,000-connection runs; see [keep-alive](#keep-alive) for why that is no
longer needed. The generator still gives up on a request after 30 s and
counts it, rather than hanging.

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
3. **No `cove` command can be handed the server's schemas — answered by
   `cove-edge check` and `cove-edge test`.** Issue #151 was closed by
   decision: `cove` will not read a serialized schema, because it would be a
   second description of a module whose first is Rust, and because `cove
   test` would still need the implementation. So `cove check` in `tenants/`
   still reports nineteen warnings (four `unchecked_host`, fifteen `host_type`:
   every `edge.Request` and `edge.Response` unchecked), `cove test` there
   fails every test with `cove::test::no_host` ("requires the `edge`
   capability, which no host module provides"), and `cove run hello` fails
   with `cove::lower::unknown_type`, which says nothing about the real reason
   (the entry takes an `edge.Request` only the server can supply). Those
   stay, and they are accurate. The embedder's answer is its own toolchain,
   as [`cove-rules-check`](../rules/host/src/bin/check.rs) is the rules
   example's: [`host/src/toolchain.rs`](host/src/toolchain.rs) runs the
   compile and the admission the server runs at deploy (`deploy::compile`,
   `deploy::admit`, the same lowering), and runs each tenant's `test fn`s on
   the VM with the server's own hosts, the tenant's grant and its limits —
   see [checking and testing a tenant](#checking-and-testing-a-tenant). It
   took about 400 lines, a fifth of them `cove test`'s failure reporting
   copied, because `cove-cli`'s runner is a binary's private module: the
   precedent says "the embedder ships its own", and what it ships is the
   reporting as well as the one line that hands over the schemas.
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
   `hello.handle(request: edge.Request)` is not a runnable entry. The fetch
   allowlist then had nowhere to go: `cove_sema::config::parse` rejects a key
   it does not know in a `[run.<name>]` table — right for `cove run`, whose
   typo it catches — so an embedder's own per-tenant policy lives in a second
   file, [`tenants/edge.toml`](tenants/edge.toml), with its own strict parser
   (an unknown key, or a tenant `cove.toml` does not name, is refused). Two
   files describing one tenant is the shape a `[run]` table that tolerated an
   embedder's namespace (`[run.proxy.edge]`, say) would avoid.
5. **Each tenant re-checked the standard library** — fixed in two steps
   since. A ten-line handler reaches 218–221 functions, and deploying it cost
   14–17 ms of checking, all of it the standard library
   `cove_sema::stdlib::install` attaches to every package: deploying `hello`
   100 times in one process, by difference of prefix passes, was parse 5.6 ms,
   resolve 2.6, type-check 5.8, lower 0.8 and prepare under 0.3 — 15.0 ms,
   and the tenant's own share of the front end too small to measure. The
   parse is now done once per process (`stdlib::attach` keeps it), which
   makes the same deploy 9.1 ms: parse 0.3, resolve 2.9, type-check 5.1,
   lower 0.8. A thousand tenants would be about nine seconds rather than
   fifteen. **Fixed** since, as
   [issue 569](https://github.com/myuon/cove/issues/569)'s first stage
   ([ADR 0083](../../docs/adr/0083-the-standard-library-is-checked-once-per-process.md)):
   the library's modules are resolved and checked once per process and every
   later tenant links against them, which makes the same deploy about 3.9 ms
   (compile 9.5 → 2.6 ms). What is left of the compile is the package-wide
   passes, still run over the library's modules and the tenant's together.
6. **Composing a one-module package by hand is copied code.**
   `host/src/deploy.rs`'s `load` is the same walk as
   `examples/rules/host/src/lib.rs:913` (`collect`), for the same reason: an
   embedder decides what is in its package, and `cove_sema::package::load`
   loads a whole package root. A "load this directory as module `m`" helper
   would serve both.
7. **A pending answer is built as a `Value` to become a `Transfer`.**
   `upstream.get` and `upstream.fetch` answer `Result<String, Error>`; the
   parking lot builds `Value::ok(Value::string(…))` — an `Rc` value on a
   thread that never runs Cove — only to call `Transfer::of` on it
   (`transfer` in `host/src/server.rs`), because `Transfer` has no
   constructors for `Ok`/`Err` and its `Enum` case would mean guessing the
   builtin's type name. The real fetch made it a second call site, not a
   different shape.
8. **A parked run's deadline did not fire while it was parked — fixed
   ([ADR 0082](../../docs/adr/0082-a-parked-run-keeps-its-deadline.md)).**
   `Limits`' deadline bounds the run "parked time included", but nothing woke
   a parked run to stop it, there was no way to end one but dropping it, and
   a run resumed past its deadline went on until its next safepoint read the
   clock. Now `ParkedVm::time_left` says what the deadline leaves,
   `ParkedVm::cancel` ends the run with the budget's own stop and a trace
   that says so, and `resume` past the deadline fails at once. The runtime
   still wakes nothing: the timer thread here holds every parked run until
   the earlier of its answer and its deadline (`Timed::wake`,
   `host/src/server.rs`), and a run whose deadline came first is cancelled
   and answered 504 — see `impatient` above. What ADR 0082 deferred — telling
   the host the cancelled run's pending work is no longer wanted — needed
   nothing from the runtime: the embedder took the request out
   (`ParkedVm::take_request`) and handed it to its host itself, so it aborts
   the fetch by the id it gave it (see `proxy` above).
9. **Small things in the language.** `"\n".join(lines) + "\n"` is refused
   (`+` is not defined for `String`) and wants a `let` and an interpolation;
   `kv.get` then `kv.put` is a lost update under concurrency, which is the
   host's to fix (`kv.increment`), and Cove has no way to say "these two host
   calls are one transaction".
10. **An embedder's test runner copies `cove test`'s reporting, and cannot
    run a test parked.** `cove-cli`'s runner is a binary's private module, so
    `cove-edge test` re-states its failure rules — an `Err` is a failure, the
    assertion's span if the message is the assertion's, the lowering error as
    the test's own — in about eighty lines of `host/src/toolchain.rs`; a
    library "run this `DeclaredTest` on this registry" would serve `cove
    test`, `cove-edge test` and the rules example's missing half alike. And
    `assertion_failure` is on `Vm` but not on `OwnedVm`, so a test that wants
    its assertion's span runs on a borrowed `Vm` through the hosts' blocking
    path (`HostApi::call`); the parked path (`call_parkable`, the server's)
    is exercised by the server's tests and not by any `test fn`.

What worked without friction is worth a line too: `Step`/`ParkedVm` did
exactly what ADR 0080 says, across threads, with no change to the tenant's
source; `PreparedProgram` made isolate-per-request the obvious default; the
runtime's error, rendered with the tenant's `SourceMap`, is a good 500 body;
`FnEntry::required_capabilities` was all it took to refuse a tenant at
deploy; and `Compiler::with_schemas` plus the deploy's own admission was all a
checker needed.

## Not done

- Chunked bodies and TLS. A request that has started arriving is read on a
  worker, so a client that sends half a head holds one for up to 5 s; idle
  connections cost no thread, but the idle thread's `poll` is O(idle
  connections) per wake-up, which a server with far more than ten thousand
  would replace with `epoll`/`kqueue`.
- The fetch pool is a few blocking threads, so more concurrent fetches than
  `--fetchers` queue; multiplexing them as the idle thread multiplexes
  connections would change `host/src/fetch.rs` and nothing else. `http://`
  only, no redirects, no request body or headers from the tenant, a 1 MiB
  response cap, and the allowlist matches the host as written on any port.
- A client that disconnects while its run is parked is not noticed until
  the answer is written: the run is not cancelled early, and its fetch is
  not aborted. The idle thread could watch parked connections for a hang-up
  as it watches idle ones, and cancel through the same `abort(id)`.
- An abort reaches a fetch that is queued or connected; one still in its
  connect (at most 5 s) finishes connecting and is then dropped unsent.
