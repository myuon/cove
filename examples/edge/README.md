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
  crunch     requires [-]  granted [-]  deployed: 227 fn, checked in 3.3 ms, prepared in 1.3 ms, isolate 2 us
  greedy     requires [kv, upstream]  granted [kv]  REFUSED: `greedy.handle` requires `upstream`, which cove.toml does not grant
  hello      requires [-]  granted [-]  deployed: 223 fn, checked in 28.3 ms, prepared in 1.8 ms, isolate 4 us
  impatient  requires [upstream]  granted [upstream]  deployed: 223 fn, checked in 21.9 ms, prepared in 2.4 ms, isolate 4 us
  proxy      requires [upstream]  granted [upstream]  fetch [127.0.0.1, localhost]  deployed: 222 fn, checked in 25.7 ms, prepared in 3.3 ms, isolate 3 us

listening on http://127.0.0.1:8787 — 4 worker thread(s), a fresh isolate per request, upstream latency 20..100 ms (parked), keep-alive (idle 5000 ms, 1000 requests per connection), a run queue per worker, with work stealing, a run is asked to yield after 2.0 ms while others wait, open-file limit 1048576

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
other six start. `fetch [...]` is `proxy`'s allowlist from
[`tenants/edge.toml`](tenants/edge.toml). `--fetchers N` sets the threads
that perform real fetches (default 4). Flags: `--workers N`, `--latency MIN..MAX` (ms),
`--pool N` (resident isolates instead of fresh ones), `--blocking-upstream`
(the control: `upstream.get` sleeps on the worker instead of parking),
`--quiet` (no `log.info` lines), `--no-keep-alive`, `--idle-timeout MS`,
`--max-requests N` (see [keep-alive](#keep-alive)), `--timeline PATH` (record
where every request ran; see [watching requests move between
threads](#watching-requests-move-between-threads)), `--scheduler steal|fifo`
and `--slice MS` (how the workers share their queue, and how long a run may
hold a worker while others wait; see [slicing long runs at
safepoints](#slicing-long-runs-at-safepoints)).

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
crunch     requires [-]  granted [-]  ok
greedy     requires [kv, upstream]  granted [kv]  REFUSED: `greedy.handle` requires `upstream`, which cove.toml does not grant
hello      requires [-]  granted [-]  ok
impatient  requires [upstream]  granted [upstream]  ok
proxy      requires [upstream]  granted [upstream]  fetch [127.0.0.1, localhost]  ok
checked 6 module(s), 11 file(s) against the server's schemas; 7 tenant(s), 1 refused
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
ok    crunch     crunch.aSizeOutOfRangeIsRefused
ok    crunch     crunch.countsThePrimesUpToN
ok    crunch     crunch.countsToTwentyThousandByDefault
ok    hello      hello.greetsTheWorldWhenNobodyIsNamed
ok    hello      hello.greetsWhoeverTheQueryNames
ok    impatient  aggregate.aServiceThatIsDownIsOneLineOfTheAnswer
ok    proxy      proxy.aHostOffTheAllowlistIsRefused
ok    proxy      proxy.httpsIsRefused
ran 10 test(s), 10 passed
```

`cove test` in `tenants/` runs the same nine tests and fails all nine with
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
| [`crunch`](tenants/crunch/crunch.cove) | — | CPU-heavy and never parks: the primes up to `?n=` (default 20000, about 4.5 ms on the VM; capped at 200000) by trial division, under `fuel = 30000000`; see [CPU-heavy and I/O-bound together](#cpu-heavy-and-io-bound-together) |
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
  "idle_poller": {"wakes": 31, "pollfds": 44, "in_poll_ms": 5401.233, "around_poll_ms": 1.012},
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
`idle_expired` how many it closed for waiting too long. `idle_poller` is
what the idle thread's `poll(2)` costs: how many times it came out of
`poll`, the `pollfd`s it had handed it, summed, and the wall time inside
`poll` and around it (rebuilding the array and sorting what it answered) —
[`compare/idle_cost.py`](compare/idle_cost.py) reads it. `fetches` and
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

Under `--rate`, latency is measured from when a request was written, which
is what the numbers on this page used. A request that has to wait for a free
connection then has that wait left out (coordinated omission).
`--from-intended` measures from request *i*'s intended start instead,
`--summary-out FILE` writes the run as JSON, and the send lag is always
printed. [`compare/`](compare/README.md) uses all three to measure this
server against the same service written in Go.

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

## Watching requests move between threads

`--timeline PATH` makes the server record, for every tenant request, when it
was accepted and queued, which worker started its run, where it parked and
at which host call, who answered (`timer` for a simulated `upstream.get`,
`fetcher` for a real `upstream.fetch`, `deadline` for a run cancelled at its
deadline), which worker resumed it, and when its response was written —
timestamps in microseconds since the server started, plus the run's fuel,
host calls and heap at `run_end` (`OwnedVm::meter`). `GET /_timeline` dumps
it (and rewrites `PATH`); `GET /_timeline?reset` forgets it.
[`host/src/timeline.rs`](host/src/timeline.rs) has one buffer per worker and
one for the parking lot, each locked only by its own thread except during a
dump, so recording adds no shared lock, counter or channel to the hot path.

One mixed run, one file, one picture:

```console
$ cargo run --release -p cove-edge -- --quiet --timeline /tmp/edge-timeline.json
$ cargo run --release -p cove-edge --bin cove-edge-load -- \
    --mix default --requests 300 --concurrency 50 --keep-alive --rate 400 \
    --timeline-out timeline.json
…
  aggregate  46 x 200
  counter    54 x 200
  hello      165 x 200
  impatient  9 x 200, 13 x 504
  proxy      13 x 200
timeline: 1900 events written to timeline.json
$ cargo run --release -p cove-edge --bin cove-edge-timeline -- timeline.json \
    -o timeline.html --perfetto timeline.perfetto.json --svg timeline.svg
313 requests over 1180.2 ms on 4 workers; 195 parks, at most 28 parked at once
resumed on a different worker: 134 of 182 (74%); timeouts (504): 13
latency p50 0.08 ms, p99 342.01 ms; wait for a worker at start p50 0.018 ms (max 0.11), at resume p50 0.011 ms (max 0.10)
workers busy: w0 0.3%, w1 0.3%, w2 0.3%, w3 0.3%; runs begun elsewhere while a request was parked: 44.1 on average
simulated answers put on the run queue after their due time by p50 3.71 ms, max 144.07 ms
```

`--mix default` is `hello=50,counter=20,aggregate=20,proxy=5,impatient=5`,
chosen per request from its index, so the same command asks the same
sequence: `proxy` fetches `hello` from the same server, and half of
`impatient`'s requests ask the `hang` service and are cancelled at its
300 ms deadline. `--rate 400` starts request *i* no sooner than *i*/400 s
into the run, so arrivals spread over 750 ms instead of all landing in the
first few; without it the 50 connections are all parked on slow tenants
within milliseconds and the picture is a wall at the left edge.

`timeline.html` is one file with no external resource: a summary row, the
legend, the figure, a hover readout for every row and run (request, tenant,
path, each run segment with its worker and duration, each park with its host
call, who answered, how long after its due time, and which worker resumed
it), and a table of every request. `timeline.svg` is the figure alone, with
its summary and legend inside it — this one, from the run above (it follows
the system's light or dark scheme):

![Worker swimlanes above, one lane per request below, on a shared time axis](timeline.svg)

The top view is the four workers, a bar wherever one is running a tenant's
isolate (a run of 20 µs is drawn 1.5 px wide); the bottom view is one row per
request in arrival order — a bar where it runs, a thin line in its tenant's
colour where it is parked, grey where it waits for a worker, a black diamond
where it resumed on a worker other than the one it parked on, and a red
cross labelled 504 where its deadline cancelled it.

What it shows, from this run:

- **Requests move freely.** 134 of the 182 resumes (74%) were on a different
  worker from the one the run parked on — `aggregate` 105 of 138, `impatient`
  20 of 31, `proxy` 9 of 13. That is what four interchangeable workers give
  by chance (three in four): the run queue has no affinity, and a `ParkedVm`
  needs none.
- **Parked requests cost the workers nothing.** Each worker was busy 0.3% of
  the 1.18 s; the median run segment is 20 µs and the longest 238 µs (a
  `hello`). While a request was parked, 10.8 other runs on average began on
  the very worker it had parked on, 7.8 of them other tenants' (up to 48), and
  at most 28 requests were parked at once on four threads.
- **No queueing at resume at this load.** An answer waited for a free worker
  11 µs at the median and 0.10 ms at worst, and a new request 18 µs (0.11 ms)
  — the grey segments are invisible at this scale.
- **Every 504 is `impatient`'s `hang`.** All 13 timeouts are
  `?services=weather,hang`, and all 9 `weather,stocks` requests answered 200.
  The timer cancelled them 6.5 ms after the 300 ms deadline at the median.
- **`proxy`'s fetches are short and nested.** Its parks lasted 0.17–0.26 ms,
  and the `hello` each one fetched is a row of its own (313 rows for 300
  client requests).
- **The surprise is the clock once the load stops.** While requests kept
  arriving, a simulated answer reached the run queue 3.3 ms after it was due
  at the median (28.6 ms at worst). After the last arrival, at 747 ms, the
  server is idle and the parking lot's `recv_timeout` starts waking late:
  21 answers p50 34 ms and up to 144 ms late, released in batches — the
  columns of diamonds at 860 ms (11 answers), 925 ms (6) and 965 ms (3) —
  and the last `impatient` request's deadline fired 147 ms late, which is
  why its 504 sits at 447 ms. It is the macOS idle-timer overshoot described
  under [numbers](#numbers), now visible per request.

The same timeline opens in Perfetto: go to <https://ui.perfetto.dev> and drag
`timeline.perfetto.json` in (or *Open trace file*; `chrome://tracing` reads it
too). Each worker is a thread track with a slice per run segment named
`tenant #id`; flow arrows join the segment that parked to the one that
resumed, so a migration is an arrow between two worker tracks; each request
has an async track of its own with its queued, parked and
ready-but-waiting intervals nested inside it; the accept, parking-lot and
fetch-pool tracks carry an instant per accepted connection and per answer;
and counter tracks draw `parked runs` (park to resume), `waiting on I/O`
(park to answer), `workers running` and `waiting for a worker` over time —
the last three are the strips of the [next
section](#cpu-heavy-and-io-bound-together). In
Perfetto's SQL page, `select count(*) from flow` for the run above answers
195, one per park.

**What recording costs: nothing measurable.** Same machine as above, load
average 4 to 5, three rounds each alternating a server without and with
`--timeline` (the recorded run reset before and dumped after, outside the
timed window): `hello` with keep-alive, 50,000 requests at 32 in flight,
53,822 / 55,326 / 54,825 req/s without and 53,573 / 54,528 / 57,030 with;
the default mix, 5,000 requests at 500 in flight, 7,417 / 7,410 / 7,441
against 7,420 / 7,428 / 7,409. That `hello` run is the worst case — four
events per request and nothing to wait on — and its spread between rounds is
larger than any difference between the modes.

The stress run is the same command at `--requests 5000 --concurrency 500`
and no `--rate`: 5,267 requests in the timeline, 3,627 parks, 500 parked at
once, 2,648 of 3,503 resumes (76%) on another worker, 124 timeouts, each
worker busy 6.2%, the wait for a worker at most 2.25 ms at start and 2.13 ms
at resume, and the timer 0.22 ms late at the median (25.9 ms at worst) — a
busy timer thread wakes on time. 32,716 events are 2.8 MB of timeline, a
4.4 MB page (rows shrank to 4 px, 3 px since; the figure is drawn for the 300-request
run) and an 8.7 MB Perfetto trace, all three written in 0.23 s.
`host/tests/timeline.rs` runs a small mix in-process with recording on and
checks the timeline's shape: every request has its run start and end, every
park an answer and a resume or a cancel on a recorded worker, the Perfetto
JSON parses with one flow start and one finish per park, and the page holds
the figure and every tenant in its legend.

## CPU-heavy and I/O-bound together

The picture above looks serial, and it is not: at `--rate 400` a run is
about 20 µs and the workers are 0.3% busy, so two of them are rarely running
at the same instant — 7% of the busy time — and a bar 1.5 px wide cannot show
that. Saturated, the same server is parallel: `hello` with keep-alive and 64
in flight answered 38.6k / 58.4k / 84.8k req/s on 1 / 2 / 4 workers, with two
or more running 52% of the busy time on four. To make it visible the load has
to *use* the workers, so there is a tenant that does nothing else.

[`crunch`](tenants/crunch/crunch.cove) counts the primes up to `?n=` by trial
division and answers the count and the largest — pure Cove, granted nothing,
never parked: about 4.5 ms at the default `n=20000`, 15 ms at 50000, 38 ms at
100000, 66 ms at 150000. **It runs on the VM**: an edge isolate is an
`OwnedVm`, which has no native tier (that is `cove run --backend native`'s
alone), so those are encoded-VM times. Its tests, `cove-edge test crunch`,
check the prime-counting function at 2, 100, 10000 and 20000 and refuse a
size that is not a number or is out of range.

`--mix cpu-io` is `crunch=35,aggregate=30,proxy=10,hello=20,impatient=5`,
with each `crunch` asking one of `n` = 20000, 50000, 100000, 150000 (chosen
per request from its index, like the rest of the mix). The sizes are larger
than "a few milliseconds" on purpose: four workers kept busy for two seconds
is eight seconds of CPU, which at 4.5 ms a request is 1,800 `crunch`es and a
picture nobody can read; at the mix's mean of 30 ms it is about 180.

```console
$ cargo run --release -p cove-edge -- --quiet --timeline /tmp/edge-timeline.json
$ cargo run --release -p cove-edge --bin cove-edge-load -- \
    --mix cpu-io --requests 500 --concurrency 200 --keep-alive --rate 330 \
    --timeline-out timeline.json
500 requests to a mix of crunch=35,aggregate=30,proxy=10,hello=20,impatient=5, 200 in flight, 8 client thread(s), connections kept alive
  answered 500 (481 with 200), 0 failed to connect or read, in 1.93 s: 259 req/s
  connections opened: 200
  latency ms: p50 70.2  p90 324.4  p99 386.5  max 459.7
  aggregate  153 x 200               79.1 req/s  p50  267.8 ms  p99  399.6 ms
  crunch     180 x 200               93.1 req/s  p50   50.1 ms  p99  123.4 ms
  hello      83 x 200                42.9 req/s  p50   17.6 ms  p99   77.4 ms
  impatient  13 x 200, 19 x 504      16.6 req/s  p50  327.8 ms  p99  459.7 ms
  proxy      52 x 200                26.9 req/s  p50   11.7 ms  p99  147.2 ms
…
$ cargo run --release -p cove-edge --bin cove-edge-timeline -- timeline.json \
    -o timeline.html --perfetto timeline.perfetto.json --svg timeline-cpu-io.svg
552 requests over 1932.2 ms on 4 workers; 575 parks, at most 40 parked at once
…
pool CPU utilisation 70.2%; time with N workers running: 0: 20.6% · 1: 3.6% · 2: 8.7% · 3: 8.5% · 4: 58.5%; at most 40 waiting on I/O and 50 waiting for a worker at once
  aggregate   153 requests  p50  266.72 ms  p99  399.44 ms  worker time      9.3 ms
  crunch      180 requests  p50   49.73 ms  p99  122.04 ms  worker time   5405.6 ms
  hello       135 requests  p50   12.45 ms  p99   76.61 ms  worker time      3.3 ms
  impatient    32 requests  p50  327.16 ms  p99  458.99 ms  worker time      2.0 ms
  proxy        52 requests  p50   11.24 ms  p99  146.24 ms  worker time      1.9 ms
```

(`cove-edge-load` now prints each tenant's rate and client-side p50/p99, and
`cove-edge-timeline` the server-side ones, from queued to written. The
server sees 135 `hello`s, the client 83: the other 52 are `proxy`'s fetches.)

![Worker swimlanes, then three strips — workers running, parked runs waiting on I/O, the run queue — then a lane per request, on one time axis](timeline-cpu-io.svg)

Under the swimlanes are three small charts with one axis each, never a dual
axis, in neutral ink because they count every tenant together: **workers
running** at each instant (0 to 4, a gridline per worker), **parked runs
waiting on I/O** (from the park to the answer), and **requests ready to run
with no worker free** (queued and not started, or answered and not yet
resumed: the run queue). The HTML adds a tile per level — the share of the
span with 0, 1, 2, 3 and 4 workers running — the pool's CPU utilisation, a
per-tenant table, and a hover readout on the strips; the Perfetto export has
the same three as counter tracks, beside `parked runs`. The strips count a
20 µs run at its true length; the 1.5 px minimum is only for the bars.
(`timeline.svg` above was drawn before the strips existed.)

**Four workers, then one**, the same command against `--workers 1`
(`cove-edge-load`'s columns, then `cove-edge-timeline`'s):

| | 4 workers | 1 worker |
| --- | ---: | ---: |
| wall clock, 500 requests | 1.93 s | 5.59 s |
| throughput | 259 req/s | 89 req/s |
| `crunch` (180 × 200) | 93.1 req/s, p50 50.1 / p99 123.4 ms | 32.2 req/s, p50 1,003 / p99 1,872 ms |
| `aggregate` (153 × 200) | 79.1 req/s, p50 268 / p99 400 ms | 27.3 req/s, p50 3,636 / p99 4,647 ms |
| `hello` (83 × 200) | 42.9 req/s, p50 17.6 / p99 77.4 ms | 14.8 req/s, p50 1,006 / p99 1,867 ms |
| `proxy` | 52 × 200, p50 11.7 / p99 147 ms | 7 × 200, **45 × 504**, p50 2,606 ms |
| `impatient` | 13 × 200, 19 × 504, p50 328 ms | **32 × 504**, p50 2,299 ms |
| pool CPU utilisation | 70.2% | 94.4% |
| share of the span with 0 / 1 / 2 / 3 / 4 running | 20.6 / 3.6 / 8.7 / 8.5 / 58.5% | 5.6 / 94.4% |
| at most waiting on I/O at once | 40 | 62 |
| at most waiting for a worker at once | 50 | 212 |
| at most parked (park to resume) | 40 | 130 |
| wait for a worker at start, p50 (max) | 14.6 ms (83.4) | 973 ms (1,930) |
| timeouts (504) | 19 | 77 |

The offered load is the same in both — 330 arrivals a second, of which
`crunch`'s want about 2.9 workers — so one worker falls behind and four keep
up with room to spare. Without a rate limit, holding 64 in flight for 1,000
requests, the pool's capacity is **92 req/s on one worker and 328 on four
(3.6×)**: CPU 97.6% against 89.7%, four running 88.5% of that run's span.

What it shows:

- **CPU work occupies the workers in parallel.** Four workers ran at once
  58.5% of the span and two or more 76%; of the 20.6% with none running,
  389 ms is after the last `crunch` ended at 1,542 ms, when only the last
  upstreams are outstanding, and 10 ms is before it. Running four at a time did not slow the
  runs down: a `crunch` took the same time on four workers as on one —
  4.25 against 4.11 ms at `n=20000`, 66.9 against 65.0 at 150000, 2–3% —
  so they were not taking turns on a core.
- **The I/O waits overlap the CPU work.** For the 1.13 s that all four
  workers were running, 21 runs on average were parked waiting on an
  upstream at the same time, and up to 39 (40 at most over the whole span);
  a parked run costs a worker nothing: `aggregate`'s 153 requests, three
  upstream calls each, took 9.3 ms of worker time between them.
- **The run queue is FIFO and a run is never pre-empted, and that is
  head-of-line blocking.** A `hello` runs for 19 µs (p50), but under this
  load it waited 12.4 ms for a worker at the median and 83 ms at worst,
  because what was ahead of it in the queue, or on every worker, was a
  `crunch` of up to 68 ms. The same arrivals without `crunch`
  (`--mix aggregate=30,proxy=10,hello=20,impatient=5 --requests 325 --rate 215`)
  answer `hello` in 0.04 ms at the median, waiting 0.01 ms. A resumed run
  queues the same way — an answer waited 4.5 ms at the median for a worker,
  three times per `aggregate` — so `aggregate`'s median went from 186 ms to
  267 ms. Nothing here is a bug; it is what a FIFO queue in front of
  run-to-completion workers does. A cure would be a scheduler's, not a
  tenant's: a quantum on long runs (the VM's fuel safepoint is a natural
  place to yield), a queue per cost class, or a worker kept for short work.
  The first is what [the next section](#slicing-long-runs-at-safepoints)
  does — and since it is now the default, the server commands above
  reproduce this section's numbers only with `--scheduler fifo --slice 0`.
- **With one worker the queue is the whole latency.** A request waited
  973 ms at the median to start; `hello`'s run is still 19 µs. A deadline
  starts with the run, not on arrival, so `impatient`'s 300 ms became a
  2.3 s median: about a second queued before it started, its 300 ms, and
  the cancel itself waiting its turn in the same queue. And `proxy` fetches
  `hello` from this same server, so its fetch waits behind the `crunch`es
  too: 45 of 52 passed their 500 ms deadline and were answered 504 — a
  service that calls itself turns its own queue into its own timeout.

## Slicing long runs at safepoints

The head-of-line blocking above has a scheduler's cure, and the runtime now
has the mechanism for it: [ADR
0084](../../docs/adr/0084-a-run-may-yield-at-a-safepoint.md) lets an
embedder ask a run to give its thread up at its next safepoint — the VM takes
one every 1,024 instructions, at loop backedges and calls alike — and hands
the run back as a `YieldedVm` that any thread can `resume()`, no answer
needed. A yielded run answers what an uninterrupted one answers, in the same
instructions, for the same fuel, with the same trace. The policy is all in
[`host/src/server.rs`](host/src/server.rs) and
[`host/src/runq.rs`](host/src/runq.rs), and it is Go's, cut down:

- **A run queue per worker, a global injection queue, and work stealing**
  (`--scheduler steal`, the default; `--scheduler fifo` is the old single
  queue). Work from outside a worker — a connection the idle thread found
  readable, a parked run the lot has an answer for, a run that yielded — goes
  on the global queue, because neither the idle thread nor the lot is a
  worker, and whichever worker comes free first should take it. A pipelined
  request goes on the queue of the worker that answered the one before it.
  A worker takes from its own queue, then a fair share of the global one
  (`len / workers + 1`, the rest kept locally), then half of a random
  victim's queue; every 61st take looks at the global queue first, so a busy
  local queue cannot starve it. There is no `runnext` slot: nothing here
  readies work for itself often enough to want one.
- **A monitor thread and a time slice** (`--slice MS`, default 2, `0` never
  asks). Each worker publishes the run it is running and when that run's
  turn began; every quarter slice the monitor raises the `YieldRequest` of a
  run that has had a whole slice — **only while something is waiting for a
  worker**, because with nothing waiting a yield is a resume's cost for
  nothing. The run yields within a stride, and goes to the back of the global
  queue, as a preempted goroutine goes to Go's: a slice is a turn, not a
  pause. A run inside a host's callback or beside a running task cannot
  yield; it declines and yields at the first safepoint where it can (no
  tenant here does either).
- The timeline records `yield {worker}`, `continue {worker}` and `steal
  {from, worker}`; the picture shows a yield as the end of a bar with a small
  tick hanging under it, and the run queue strip counts a yielded run as
  waiting for a worker until it is continued.

```console
$ cargo run --release -p cove-edge -- --quiet --scheduler steal --slice 2 \
    --timeline /tmp/edge-timeline.json
$ cargo run --release -p cove-edge --bin cove-edge-load -- \
    --mix cpu-io --requests 500 --concurrency 200 --keep-alive --rate 330 \
    --timeline-out timeline.json
500 requests to a mix of crunch=35,aggregate=30,proxy=10,hello=20,impatient=5, 200 in flight, 8 client thread(s), connections kept alive
  answered 500 (481 with 200), 0 failed to connect or read, in 1.88 s: 267 req/s
  connections opened: 200
  latency ms: p50 71.7  p90 245.8  p99 325.5  max 420.4
  aggregate  153 x 200               81.6 req/s  p50  213.0 ms  p99  362.0 ms
  crunch     180 x 200               96.0 req/s  p50   41.6 ms  p99  182.5 ms
  hello      83 x 200                44.3 req/s  p50    4.0 ms  p99   15.6 ms
  impatient  13 x 200, 19 x 504      17.1 req/s  p50  306.7 ms  p99  402.3 ms
  proxy      52 x 200                27.7 req/s  p50    9.1 ms  p99   23.7 ms
…
$ cargo run --release -p cove-edge --bin cove-edge-timeline -- timeline.json \
    --svg timeline-sliced.svg
…
yields at a safepoint: 1278 (999 continued on another worker), waiting p50 2.848 ms (max 20.47) to continue; requests' jobs stolen: 8
```

![The same arrivals as the picture above, with a 2 ms slice: every long crunch bar is cut into turns, and the run queue strip stays low](timeline-sliced.svg)

The same arrivals as [the picture above](#cpu-heavy-and-io-bound-together),
four ways, on the same machine one after another. Client-side columns from
`cove-edge-load`, server-side ones from `cove-edge-timeline`; the figure in
parentheses is a second run of the same command:

| | (a) FIFO, no slice | (b) stealing, no slice | (c) stealing, 2 ms slice | (d) stealing, 1 ms slice |
| --- | ---: | ---: | ---: | ---: |
| `hello` p50 / **p99** | 19.8 / **77.0** ms (25.6 / 85.2) | 21.2 / **99.9** ms (28.4 / 107.1) | 4.0 / **15.6** ms (4.1 / 13.6) | 2.8 / **9.0** ms (3.1 / 9.2) |
| `aggregate` **p50** / p99 | **275** / 403 ms (292) | **282** / 429 ms (300) | **213** / 362 ms (215) | **206** / 399 ms (207) |
| `proxy` p99 | 150 ms | 176 ms | 24 ms | 15 ms |
| `crunch` throughput, p50 / p99 | 94.0 req/s, 51.6 / 118 ms | 94.0 req/s, 48.5 / 153 ms | 96.0 req/s, 41.6 / 183 ms (p99 228) | 96.3 req/s, 42.0 / 197 ms |
| `crunch` worker time per request | 30.4 ms (31.0) | 30.6 ms (31.3) | 30.5 ms (31.4) | 30.9 ms (31.1) |
| wait for a worker at start, p50 (max) | 16.1 ms (85.0) | 14.5 ms (157) | 2.1 ms (19.9) | 1.4 ms (9.1) |
| at most waiting for a worker at once | 51 | 53 | 24 | 21 |
| pool CPU utilisation | 71.6% | 72.1% | 73.6% | 74.7% |
| yields (continued on another worker) | 0 | 0 | 1,278 (999) | 2,787 (2,203) |
| a yielded run's wait to continue, p50 | – | – | 2.85 ms | 1.54 ms |
| steals (jobs taken) | 0 | 26 (30) | 8 (8) | 8 (8) |
| wall clock, throughput | 1.91 s, 261 req/s | 1.92 s, 261 req/s | 1.88 s, 267 req/s | 1.87 s, 267 req/s |
| `impatient` | 13 × 200, 19 × 504 | the same | the same | the same |

**Unthrottled** (`--requests 1000 --concurrency 64 --keep-alive`, no
`--rate`), the pool's capacity, three runs each: (a) 325, 303, 320 req/s;
(b) 318, 306, 311; (c) 330, 332, 330; (d) 343, 317, 329. In the first of each
(the one recorded with `--timeline`) pool CPU was 87.8%, 88.0%, 91.0% and
95.5%, `hello`'s p99 193, 244, 37 and 24 ms, and `crunch`'s p99 212, 234,
549 and 660 ms.

What it shows:

- **The slice is what ends the head-of-line blocking.** `hello`'s p99 falls
  from 77–85 ms to 14–16 ms at 2 ms and to 9 ms at 1 ms, and its median
  from 20–26 ms to 3–4. A short request now waits for at most a few slices
  of the runs ahead of it, not for whole runs: the longest wait for a worker
  at start went from 85 ms to 20 and then 9, and the run queue's peak
  halved. `proxy` gains most, because its fetch is a `hello` behind the same
  queue: p99 150 ms to 15. `aggregate`'s median falls by a quarter, 275 ms
  to 206–213, because each of its three resumes waited behind `crunch`es too.
- **Work stealing alone did nothing for it, and was a little worse.** (b) is
  (a) within the noise at the median and worse at the tail (`hello` p99
  100–107 against 77–85, longest start wait 157 ms against 85). Stealing
  balances *queued* work between workers, and the problem is not
  imbalance: every worker is busy with a long run. Worse, a worker that
  takes its share of the global queue and then starts a 60 ms `crunch` holds
  that share in its own queue until another worker is idle enough to steal
  it — which, under this load, is rarely: 26 steals in the whole run. With a
  slice, the holder comes back to its own queue every 2 ms, and steals fall
  to 8. The queues are the shape a scheduler needs once there is a slice to
  make turns of; they are not the cure.
- **What slicing costs is the long runs' latency, not their CPU.** A
  `crunch` took the same worker time in all four, 30.4–31.4 ms a request —
  the variation between two runs of one configuration is as large as the
  variation between configurations — and 2,787 yields at about 3 µs a
  resume (ADR 0080's measurement of a park) would be 8 ms of 5.5 s of worker
  time, 0.15%, which is below what this can see. Throughput did not fall:
  `crunch` 94–96 req/s, the whole mix 261–267 req/s, and unthrottled
  capacity 303–343 req/s in every configuration, the sliced ones at the top
  of that range. What does move is that long runs now share the workers, so
  each finishes later: `crunch`'s p99 rose from 118 ms to 183–228 at 2 ms
  and 197 at 1 ms, and unthrottled from 212 ms to 549–660. Its median
  *fell*, 52 ms to 42, because a short `crunch` is no longer stuck behind a
  long one. That is processor sharing's trade, and it is the right one for
  a server whose short requests are its interactive ones: the
  shortest-running requests gain the most, the longest lose some.
- **A shorter slice buys a little more for a little more.** 1 ms against
  2 ms: `hello` p99 9 against 14–16 ms, twice the yields, the same CPU per
  `crunch`, and `crunch`'s p99 197 against 183–228 ms. Most yields (78–79%)
  continue on another worker, so each one also moves a machine to a cold
  cache; at this run length that is not visible either.
- **`impatient`'s 504s are unchanged**, 19 of 32, because its deadline is
  spent waiting on an upstream, not on a worker.

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
