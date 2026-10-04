# edge against Go: what the isolates cost

[`examples/edge`](../README.md) serves each request in a fresh Cove isolate on
four worker threads, under fuel, a deadline and a host-call budget, with
capabilities checked at deploy and at the boundary. This directory asks what
that costs against **the same service written the ordinary way in Go** —
`net/http`, a goroutine per connection, `time.Timer` for the simulated
upstream, `context.WithTimeout` for a deadline, and nothing else
([`go/`](go/), std only). The Go server is the performance baseline. What
Cove adds is listed under [conditions](#conditions) as a difference, not
emulated in the Go code, so the price of it shows up in the numbers.

The comparison is split in two so that the VM's speed and the servers' HTTP
and scheduling are not mixed up:

- **Stage 1, function level**: the same handler, the same input and the
  same output, timed in-process. No sockets, no process start.
- **Stage 2, service level**: both servers on the same CPUs, at the same
  arrival rates, with the same mix and the same simulated upstream waits,
  driven by the same load generator (`cove-edge-load`).

All figures were measured on 2026-10-04 on one machine: an Intel i7-10700K
(8 cores, 16 hardware threads, x86-64) with 32 GB, macOS 26.6.2, rustc
1.98.1 `--release`, and Go 1.23.2. Other agents' builds were running on the
same machine, and the load average was **2.5 to 9** over the session. Each
row records the load average before it ran (in `results/*.jsonl`).

## Reproducing

```console
$ cargo build --release -p cove-edge --features native     # the servers, the load generator, cove-edge-compare
$ (cd examples/edge/compare/go && go build -o edge-go .)
$ ./target/release/cove-edge-compare --batches 7 --min-ms 300 > examples/edge/compare/results/stage1-cove.txt
$ (cd examples/edge/compare/go && ./edge-go bench -batches 7 -min-ms 300 > ../results/stage1-go.txt)
$ python3 examples/edge/compare/sweep.py capacity --reps 3
$ python3 examples/edge/compare/sweep.py capacity --reps 3 --scenario aggregate cpu-io --concurrency 10000 1024
$ python3 examples/edge/compare/sweep.py sweep --reps 3          # about 25 minutes
$ python3 examples/edge/compare/sweep.py connections --reps 3
$ python3 examples/edge/compare/sweep.py waiting --reps 3
$ python3 examples/edge/compare/charts.py [--png DIR]            # the SVGs here; PNGs via headless Chrome
$ python3 examples/edge/compare/charts.py --tables               # the tables below
```

`sweep.py` appends to `results/*.jsonl`, which hold every run behind this
page. It starts each server alone for each scenario, with `--workers 4` or
`GOMAXPROCS=4`, warms every endpoint once, and stops it afterwards. The edge
server and the Go server take turns, so if the machine's load changes it
lands on both. `--features native` matters to `cove-edge-compare` and to
`cove-edge --backend native` (`sweep.py --servers cove-native`, [below](#the-native-backend-adr-0085));
without `--backend native` the server's isolates run encoded, as every row
above this section's does.

The Go module is not part of the Cargo workspace and nothing in the gate
builds it. `go/edge-go` is ignored by git.

## Conditions

| | Cove `examples/edge` | Go `compare/go` |
| --- | --- | --- |
| endpoints, bodies | `hello`, `crunch`, `counter`, `aggregate`, `impatient`, `proxy` | the same paths and the same 200 bodies (checked byte for byte with `curl`). Error bodies differ: Go's 504 is one line, not the runtime's diagnostic |
| `crunch` algorithm | `tenants/crunch/crunch.cove` | the same loops line for line, `int64` |
| simulated upstream | `upstream.get`: 20–100 ms from `Latency::pick` (splitmix over a call counter), parked | the same function, the same counter, the same services; a `time.Timer` per call in the request's goroutine |
| deadlines | `impatient` 300 ms, `proxy` 500 ms, `aggregate` 5 s, from the start of the run; the parking lot cancels a run whose deadline passes → 504 | `context.WithTimeout` from the start of the handler → 504 |
| `proxy` fetch | 4 fetcher threads, a new connection per fetch (`Connection: close`) | `http.Client` with keep-alive (`MaxIdleConnsPerHost` 1024: the default of 2 is a known trap). The same allowlist check, as a plain map lookup |
| CPU | 4 worker threads, plus accept, idle poller (`poll(2)`), parking lot (timer), slice monitor, 4 fetchers: **13 threads** idle | `GOMAXPROCS=4` (4 Ps), plus the runtime's netpoller (kqueue) and sysmon, and threads for blocking syscalls: **5 threads** idle |
| scheduling | a run queue per worker, work stealing, a 2 ms slice at VM safepoints (ADR 0084) | Go's scheduler: per-P queues, work stealing, async preemption at 10 ms |
| keep-alive | idle 5 s, 1,000 requests per connection | `IdleTimeout` 5 s, no per-connection cap |
| **isolation** | a fresh isolate (`OwnedVm`) per request, with its own heap. A tenant cannot see another's memory | none: one address space, shared heap |
| **budgets** | fuel per request (`crunch` 30 M, `hello` 2 M), a host-call limit, a wall-clock deadline, all enforced by the runtime | none. `/hello/spin` is not ported, because an ordinary Go handler has nothing that stops a loop |
| **capabilities** | checked at deploy from the call graph (`greedy` is refused), and at the boundary on every host call | none |
| **record/replay, timeline** | host calls are recordable, and `--timeline` is available (off in these runs) | none |
| **yield** | a long run gives its worker up at a safepoint when others wait | the Go runtime preempts goroutines; there is nothing to opt into |

The load generator, `cove-edge-load`, runs on the same machine with 8
threads, each polling its share of the sockets without blocking. When nothing
progresses it sleeps 200 µs. **That sleep and the shared machine set a floor
of about 1.5 ms (p50) and 3 ms (p99) under which it measures itself rather
than either server.** At low load both servers sit on the floor; the edge
server's own `/_stats` says its server-side hello latency is 0.05 ms at the
median. Under heavy load the generator stops sleeping and the floor falls,
which is why some p50s *drop* as the rate rises.

### Changes to the load generator

`--rate N` used to measure latency from when a request was actually written.
If every one of the `--concurrency` connections was busy when request *i*
was due, it was written later and its wait went unrecorded: coordinated
omission. The flags below are new, and the old behaviour stays the default
because `examples/edge/README.md`'s numbers were measured with it:

- `--from-intended` measures from request *i*'s intended start, *i* / rate
  into the run, which is what an open-loop arrival process sees. Every
  Stage 2 sweep uses it.
- The send lag is always reported under `--rate` (p50, p99 and max, and
  how many requests were sent more than 1 ms late), so a run where the
  connection cap bound shows it.
- `--summary-out FILE` writes the run as JSON. It includes the latency both
  from the intended start and from the send (`p99_sent_ms`), overall and
  per tenant.
- A kept-alive connection idle for 2 s is dropped and reopened when next
  needed. Without this, a connection idle past a server's 5 s timeout
  failed its next request with "the response has no head" — the
  generator's failure, counted against the server.

The sweeps size the connection pool per scenario: about twice the arrivals
of one typical latency, and at least 64 (`POOL_SECONDS` in `sweep.py`).
They do not use the largest pool that could ever be needed, because idle
kept-alive connections are not free on the edge server (see
[connections](#connections)). Above a server's capacity the pool binds, and
the send lag shows it. The latency still counts from the intended start.

## Stage 1: the same function

`cove-edge-compare` ([`host/src/bin/compare.rs`](../host/src/bin/compare.rs))
times each case three ways:

- **edge**: what the server pays per request, less the socket. It builds the
  `edge.Request` value, takes a fresh isolate (`Deployed::isolate`), runs it
  under the tenant's budget (`invoke_within_parkable`), and reads the body
  back out.
- **vm**: one reused `Vm` over the same lowered program.
- **native**: the same, on `compile_native`'s machine code.

`edge-go bench` ([`go/bench.go`](go/bench.go)) times **go**, the handler with
its query map built once, and **go+map**, which builds the map per call too.
Every mode's answer is compared with the first one's before timing, and all
of them print the same bytes: `"2262 primes up to 20000, the largest
19997\n"`, `"Hello, Cove! (GET /)\n"` and so on (in
[`results/stage1-*.txt`](results/)). Each figure is the median of 7 batches
of at least 300 ms. The spread between batches was under 2% except where
noted.

| case | Cove edge path | Cove VM, reused | Cove native | Go | Go+map | **edge / Go** | **native / Go** |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `crunch n=2000` | 203.60 µs | 198.10 µs | 67.09 µs | 43.69 µs | 44.26 µs | **4.7×** | **1.5×** |
| `crunch n=20000` | 4.23 ms | 4.40 ms | 1.33 ms | 997.40 µs | 998.57 µs | **4.2×** | **1.3×** |
| `crunch n=150000` | 65.68 ms | 66.71 ms | 20.49 ms | 15.55 ms | 15.53 ms | **4.2×** | **1.3×** |
| `hello name=Cove` | 8.11 µs | 3.88 µs | 3.83 µs | 54 ns | 217 ns | **150.2×** | **70.9×** |

The native tier compiled every function each case reaches (8 of 8 for
`crunch`, 5 of 5 for `hello`).

![Stage 1: time per call on a log scale, Cove VM, Cove native and Go for each case](stage1.svg)

**`crunch` is the VM.** A fresh isolate adds nothing measurable to a call
of 0.2 ms or more: edge and the reused VM are within 4%, in either order.
What is left is 4.2× Go on the encoded VM and **1.3× on the native tier**
from `n` = 20,000 up (4.7× and 1.5× at 2,000, where the 8 µs per-call cost
is 4% of the call). Past that the ratio does not depend on `n`, so it is
per iteration, not per call.

**`hello` is fixed cost.** It takes 8.1 µs per request against Go's 54 ns
(217 ns building the map). The native tier does not help (3.83 against
3.88 µs), because the time is not spent in the handler. A handler that
answers a constant `edge.Response` and never reads the request (a
throwaway tenant directory outside the repository; its timings are in `results/stage1-cove-noop.txt`) takes **6.65 µs** on the
edge path and **2.98 µs** on a reused VM. So, per request:

| part | µs | how it was found |
| --- | ---: | --- |
| a fresh isolate, the `edge.Request` value built, the budget | 3.7–4.2 | edge path − reused VM, for the constant handler and for `hello` |
| one `invoke` that does nothing: the `Request` into the VM, a `Response` out | 3.0 | the constant handler on a reused VM |
| `hello`'s own work (a map lookup, `unwrapOr`, a three-part interpolation) | 0.9 | `hello` − the constant handler, reused VM |

`hello`'s own work is about 17× Go's 54 ns, a plausible VM ratio for string
code. The other 7 µs is the price of entering and leaving an isolate, and
the next section shows it is not the larger part of the server's
per-request cost.

## Stage 2: the same service

### Capacity

Closed loop, unthrottled, keep-alive, at the stated number in flight. Each
figure is the median of 3 runs (range in parentheses). CPU is the server
process's user+sys over the run (`ps -o time`), divided by the requests
answered.

| scenario | in flight | Cove req/s | Go req/s | Go / Cove | Cove CPU µs/req | Go CPU µs/req | Cove p50 / p99 ms | Go p50 / p99 ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| hello | 64 | 84,235 (81,574–86,650) | 122,725 (122,716–124,420) | 1.46 | 41 | 23 | 0.6 / 2.0 | 0.4 / 1.9 |
| hello | 256 | 119,242 (117,127–120,390) | 163,543 (158,353–163,608) | 1.37 | 41 | 23 | 2.1 / 4.1 | 1.4 / 4.0 |
| crunch | 16 | 881 (867–883) | 3,696 (3,696–3,713) | 4.20 | 4560 | 1078 | 18.1 / 29.6 | 4.0 / 10.2 |
| crunch | 64 | 878 (867–879) | 3,691 (3,635–3,697) | 4.20 | 4570 | 1078 | 74.0 / 121.6 | 15.0 / 45.7 |
| aggregate | 2,000 | 10,307 (10,051–10,390) | 10,482 (10,474–10,482) | 1.02 | 187 | 109 | 181.3 / 269.9 | 181.0 / 269.8 |
| aggregate | 5,000 | 24,519 (24,433–24,707) | 24,438 (24,395–24,478) | 1.00 | 174 | 92 | 182.4 / 271.1 | 182.8 / 272.0 |
| aggregate | 10,000 | 33,500 (32,785–33,778) | 44,245 (43,983–44,246) | 1.32 | 152 | 71 | 269.7 / 363.6 | 195.2 / 335.0 |
| cpu-io | 64 | 335 (330–340) | 807 (802–807) | 2.41 | 11585 | 2610 | 192.3 / 540.3 | 10.0 / 301.7 |
| cpu-io | 256 | 347 (345–351) | 1,345 (1,341–1,347) | 3.87 | 11550 | 2600 | 577.9 / 2727.5 | 112.2 / 479.8 |
| cpu-io | 1,024 | 338 (336–344) | 1,393 (1,390–1,404) | 4.12 | 11842 | 2724 | 1808.8 / 10891.2 | 688.5 / 1342.7 |

- **`crunch`: 4.20× at the server, 4.24× in Stage 1.** For CPU-heavy work
  the server adds nothing to the VM's gap. 4.56 ms of CPU per request on
  the edge server against 4.23 ms per call in Stage 1, and 1.08 ms against
  1.00 ms for Go.
- **`hello`: 1.37–1.46×**, at **41 µs of server CPU per request against
  Go's 23 µs**. Stage 1 accounts for 8 µs of the 41. The edge server's
  `--timeline` on a hello run at 10,000 req/s gives 14.8 µs of worker time
  per request (run start to run end, which includes building the request
  value and the response) and 0.05 ms from queued to written at the median.
  So **about 26–33 µs per request is the edge host's own HTTP, run queue,
  idle-thread hand-off and write**, which is more than Go's whole
  per-request cost of 23 µs. The isolate is a fifth of the fixed cost, not
  most of it.
- **`aggregate`: equal while the load is concurrency-bound** (2,000 and
  5,000 in flight: 1.00–1.02×). At 10,000 in flight it is **1.32×**:
  33,500 against 44,245 req/s, with the edge server's p50 at 270 ms against
  195 ms. A request costs 152 µs of CPU against 71 µs: three parks, three
  resumes and three timer entries against three goroutine sleeps.
- **`cpu-io`: 2.4–4.1×**, rising with concurrency, because Go's closed loop
  at 64 in flight is bound by the mix's waits and not by its CPU. It is
  `crunch`'s ratio once enough is in flight.

### Latency against offered rate

Open loop, `--from-intended`, 4 s of arrivals per point (at least 1,000
requests), 3 runs per point. Each server was swept up to 1.15× its own
capacity and no further. Past saturation an open-loop run only measures how
long it lasted, so "–" means *not run*. The x axis is the same absolute
grid for both servers, so the point where a curve turns up is where that
server gets tight.

![p99 latency against offered rate, four panels: hello, crunch, aggregate, and hello within the cpu-io mix](sweep-p99.svg)

![p50 latency against offered rate, the same four panels](sweep-p50.svg)

Latencies in ms, median (range) of 3 runs. "from send" is the same requests measured from when they were written, which understates once the pool binds. CPU is server CPU per answered request, in µs.

#### hello alone

| offered req/s | Cove p50 | Cove p99 | Go p50 | Go p99 | p99 Cove / Go | Cove p99 from send | Go p99 from send | Cove CPU µs/req | Go CPU µs/req |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2,500 | 1.6 (1.6–1.6) | 3.0 (3.0–3.0) | 1.5 (1.5–1.5) | 3.0 (3.0–3.0) | 1.0x | 1.9 (1.9–1.9) | 1.9 (1.9–1.9) | 114 (113–114) | 80 (80–82) |
| 10,000 | 1.5 (1.5–1.5) | 3.0 (3.0–3.0) | 1.5 (1.4–1.5) | 3.0 (3.0–3.0) | 1.0x | 1.9 (1.9–1.9) | 1.9 (1.9–1.9) | 65 (64–65) | 43 (42–43) |
| 25,000 | 1.3 (1.2–1.3) | 2.9 (2.9–2.9) | 1.3 (1.2–1.3) | 2.9 (2.9–2.9) | 1.0x | 1.9 (1.9–1.9) | 1.9 (1.9–1.9) | 53 (52–54) | 32 (31–32) |
| 50,000 | 1.1 (1.1–1.1) | 3.1 (3.0–3.1) | 1.0 (1.0–1.0) | 2.9 (2.9–3.0) | 1.1x | 2.2 (2.2–2.2) | 2.0 (2.0–2.0) | 48 (48–49) | 28 (28–28) |
| 75,000 | 0.6 (0.5–0.6) | 3.2 (3.1–3.2) | 0.8 (0.7–0.8) | 3.0 (2.9–3.1) | 1.1x | 2.4 (2.4–2.5) | 2.3 (2.2–2.4) | 47 (46–47) | 27 (26–27) |
| 100,000 | 0.6 (0.5–0.6) | 7.2 (6.4–11.8) | 0.1 (0.1–0.1) | 3.1 (3.0–4.7) | 2.4x | 5.3 (5.0–5.6) | 2.6 (2.5–3.0) | 45 (44–45) | 28 (28–28) |
| 125,000 | 343.8 (312.5–349.2) | 697.5 (695.3–714.7) | 0.1 (0.1–0.7) | 5.5 (5.2–45.4) | 126.5x | 7.2 (7.2–7.4) | 4.2 (4.0–6.9) | 46 (46–47) | 27 (27–29) |
| 150,000 | – | – | 5.8 (4.3–9.9) | 28.1 (26.9–51.0) | – | – | 7.1 (6.8–7.3) | – | 26 (26–26) |
| 175,000 | – | – | 289.9 (282.8–349.4) | 668.8 (585.8–722.2) | – | – | 8.0 (7.9–8.3) | – | 26 (26–26) |

#### crunch n=20000 alone

| offered req/s | Cove p50 | Cove p99 | Go p50 | Go p99 | p99 Cove / Go | Cove p99 from send | Go p99 from send | Cove CPU µs/req | Go CPU µs/req |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 5.5 (5.4–5.5) | 7.3 (7.2–7.8) | 2.2 (2.1–2.2) | 4.1 (3.9–4.1) | 1.8x | 6.5 (6.3–6.7) | 3.0 (3.0–3.1) | 4560 (4560–4570) | 1290 (1270–1300) |
| 250 | 5.3 (5.3–5.3) | 7.0 (6.9–7.0) | 1.9 (1.9–1.9) | 3.9 (3.9–4.0) | 1.8x | 6.2 (6.1–6.3) | 3.0 (2.9–3.0) | 4410 (4400–4430) | 1210 (1200–1210) |
| 500 | 5.4 (5.3–5.4) | 7.3 (6.9–11.0) | 1.9 (1.9–1.9) | 3.6 (3.6–3.6) | 2.0x | 7.1 (6.2–10.7) | 2.8 (2.8–2.9) | 4540 (4430–4650) | 1170 (1165–1170) |
| 750 | 5.6 (5.5–5.6) | 7.3 (7.2–7.5) | 1.9 (1.9–1.9) | 3.8 (3.8–3.8) | 1.9x | 6.6 (6.4–6.7) | 2.8 (2.8–2.8) | 4643 (4593–4650) | 1137 (1137–1140) |
| 900 | 104.6 (95.0–119.5) | 224.1 (212.0–235.6) | 1.6 (1.5–1.7) | 3.4 (3.2–3.4) | 66.7x | 124.4 (118.2–124.5) | 2.8 (2.7–2.8) | 4678 (4661–4683) | 1125 (1122–1125) |
| 1,000 | 330.0 (313.0–346.6) | 657.1 (642.6–678.8) | 1.6 (1.6–1.6) | 3.1 (3.1–3.2) | 209.2x | 122.9 (122.6–127.1) | 2.7 (2.7–2.8) | 4653 (4642–4678) | 1108 (1108–1112) |
| 1,500 | – | – | 1.8 (1.8–1.8) | 3.3 (3.3–3.4) | – | – | 2.7 (2.7–2.8) | – | 1102 (1088–1107) |
| 2,500 | – | – | 1.8 (1.7–1.8) | 3.3 (3.2–3.3) | – | – | 2.8 (2.7–2.8) | – | 1090 (1086–1093) |
| 3,000 | – | – | 1.8 (1.8–1.8) | 3.5 (3.4–3.7) | – | – | 2.9 (2.9–3.3) | – | 1093 (1090–1096) |
| 3,500 | – | – | 2.2 (2.2–2.2) | 5.6 (4.6–6.0) | – | – | 5.1 (4.2–5.6) | – | 1095 (1092–1096) |
| 4,000 | – | – | 177.5 (175.6–186.1) | 359.6 (357.8–374.4) | – | – | 89.4 (87.9–91.6) | – | 1085 (1084–1087) |

#### aggregate alone (3 x 20–100 ms upstream)

| offered req/s | Cove p50 | Cove p99 | Go p50 | Go p99 | p99 Cove / Go | Cove p99 from send | Go p99 from send | Cove CPU µs/req | Go CPU µs/req |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 184.1 (183.8–184.1) | 272.2 (271.3–274.3) | 182.9 (182.9–183.2) | 269.5 (268.5–269.8) | 1.0x | 271.5 (270.4–274.0) | 269.2 (268.3–269.3) | 452 (452–455) | 245 (242–250) |
| 2,500 | 182.6 (182.4–182.7) | 272.0 (271.6–273.2) | 181.3 (181.1–182.1) | 270.6 (269.7–270.7) | 1.0x | 271.6 (271.4–272.8) | 270.4 (269.5–270.4) | 479 (475–480) | 181 (180–184) |
| 5,000 | 182.5 (182.4–182.5) | 272.1 (270.4–272.2) | 181.0 (180.9–181.1) | 269.9 (269.1–270.5) | 1.0x | 272.0 (270.4–272.0) | 269.7 (268.7–270.5) | 340 (337–342) | 138 (138–143) |
| 10,000 | 182.4 (182.0–182.5) | 271.3 (271.1–271.6) | 180.7 (180.6–180.8) | 269.6 (269.4–269.9) | 1.0x | 271.3 (271.1–271.6) | 269.6 (269.4–269.8) | 250 (249–251) | 119 (118–120) |
| 20,000 | 184.0 (183.9–184.1) | 273.0 (272.9–273.0) | 182.0 (182.0–186.9) | 270.8 (270.7–325.2) | 1.0x | 273.0 (272.9–273.0) | 270.8 (270.7–323.6) | 190 (190–190) | 103 (103–105) |
| 30,000 | 206.8 (206.5–207.3) | 410.4 (396.7–413.8) | 190.3 (184.7–191.8) | 409.9 (276.7–457.4) | 1.0x | 410.1 (396.6–413.8) | 409.8 (276.6–457.4) | 161 (160–161) | 89 (88–89) |
| 35,000 | 242.1 (212.2–254.3) | 415.1 (394.5–454.8) | 192.3 (192.3–193.8) | 389.7 (358.7–399.2) | 1.1x | 414.8 (393.9–444.0) | 389.5 (358.5–399.2) | 151 (151–152) | 82 (82–83) |
| 40,000 | – | – | 194.5 (194.2–194.6) | 371.3 (369.4–409.6) | – | – | 371.1 (367.2–409.4) | – | 77 (77–77) |
| 45,000 | – | – | 198.8 (198.2–199.8) | 361.5 (356.8–395.9) | – | – | 354.9 (352.4–378.4) | – | 72 (72–72) |

#### cpu-io mix: hello's latency

| offered req/s | Cove p50 | Cove p99 | Go p50 | Go p99 | p99 Cove / Go | Cove p99 from send | Go p99 from send | Cove CPU µs/req | Go CPU µs/req |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 50 | 1.6 (1.5–1.7) | 2.9 (2.9–2.9) | 1.7 (1.6–1.8) | 3.0 (2.9–3.0) | 1.0x | 1.9 (1.9–2.0) | 1.9 (1.9–1.9) | 11270 (11250–11370) | 2900 (2900–2910) |
| 100 | 1.7 (1.5–1.8) | 3.1 (3.0–3.5) | 1.6 (1.6–1.7) | 3.1 (2.9–3.2) | 1.0x | 1.9 (1.9–3.0) | 1.9 (1.9–1.9) | 11110 (11100–11150) | 2840 (2830–2850) |
| 200 | 1.7 (1.7–1.8) | 7.3 (6.5–8.6) | 1.6 (1.6–1.6) | 2.9 (2.8–3.0) | 2.5x | 6.6 (6.2–8.4) | 1.9 (1.9–2.0) | 11290 (11280–11310) | 2770 (2770–2790) |
| 250 | 2.0 (1.9–2.0) | 8.5 (8.4–10.3) | 1.5 (1.4–1.6) | 2.9 (2.8–2.9) | 3.0x | 8.3 (8.0–10.1) | 1.9 (1.9–1.9) | 11490 (11420–11640) | 2750 (2750–2750) |
| 300 | 2.8 (2.7–2.9) | 17.7 (16.8–18.4) | 1.4 (1.4–1.6) | 3.0 (2.9–3.0) | 6.0x | 16.9 (16.8–18.3) | 1.9 (1.9–1.9) | 11608 (11608–11633) | 2725 (2725–2742) |
| 350 | 9.2 (9.0–10.8) | 28.7 (28.6–35.3) | 1.6 (1.5–1.6) | 2.8 (2.7–2.9) | 10.1x | 28.2 (27.9–34.9) | 1.9 (1.9–1.9) | 11993 (11986–12014) | 2757 (2750–2757) |
| 400 | – | – | 1.6 (1.6–1.6) | 3.0 (3.0–3.0) | – | – | 2.0 (1.9–2.1) | – | 2725 (2725–2725) |
| 600 | – | – | 1.5 (1.5–1.5) | 4.3 (3.7–4.5) | – | – | 4.2 (3.4–4.2) | – | 2638 (2633–2650) |
| 1,000 | – | – | 1.6 (1.5–1.6) | 13.3 (12.9–14.5) | – | – | 12.8 (12.4–14.3) | – | 2587 (2580–2590) |
| 1,250 | – | – | 1.9 (1.9–1.9) | 26.1 (25.5–37.6) | – | – | 25.7 (24.9–37.3) | – | 2624 (2608–2636) |
| 1,500 | – | – | 13.9 (13.0–15.0) | 114.8 (112.0–123.9) | – | – | 114.8 (111.8–123.6) | – | 2633 (2632–2635) |

#### cpu-io mix, every tenant (p50 / p99 ms, median of 3)

| offered | server | aggregate | crunch | hello | impatient | proxy |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 50 | cove | 261.7 / 422.5 | 17.2 / 71.6 | 1.6 / 2.9 | 301.1 / 361.7 | 2.0 / 3.3 |
| 50 | go | 182.1 / 263.5 | 5.9 / 18.5 | 1.7 / 3.0 | 300.6 / 303.0 | 1.7 / 3.0 |
| 100 | cove | 221.7 / 335.0 | 16.8 / 71.2 | 1.7 / 3.1 | 301.1 / 341.5 | 1.8 / 3.2 |
| 100 | go | 187.6 / 272.3 | 5.7 / 18.1 | 1.6 / 3.1 | 300.7 / 302.9 | 1.6 / 2.9 |
| 200 | cove | 202.7 / 315.0 | 20.4 / 114.1 | 1.7 / 7.3 | 300.6 / 323.1 | 1.8 / 16.7 |
| 200 | go | 181.7 / 268.3 | 5.5 / 17.9 | 1.6 / 2.9 | 300.7 / 302.9 | 1.6 / 2.9 |
| 250 | cove | 202.0 / 347.1 | 30.7 / 158.3 | 2.0 / 8.5 | 300.7 / 316.9 | 2.1 / 16.2 |
| 250 | go | 180.5 / 264.9 | 5.4 / 17.8 | 1.5 / 2.9 | 300.6 / 302.8 | 1.6 / 2.9 |
| 300 | cove | 198.5 / 307.1 | 41.2 / 242.8 | 2.8 / 17.7 | 169.5 / 319.9 | 4.1 / 35.1 |
| 300 | go | 183.6 / 271.3 | 5.5 / 18.1 | 1.4 / 3.0 | 164.0 / 302.4 | 1.5 / 2.9 |
| 350 | cove | 226.4 / 332.0 | 87.6 / 410.3 | 9.2 / 28.7 | 201.7 / 338.6 | 23.9 / 71.5 |
| 350 | go | 183.1 / 268.6 | 5.9 / 18.0 | 1.6 / 2.8 | 171.1 / 302.5 | 1.5 / 3.0 |
| 400 | go | 181.7 / 268.8 | 6.2 / 17.7 | 1.6 / 3.0 | 178.9 / 302.9 | 1.5 / 2.8 |
| 600 | go | 185.2 / 270.7 | 5.9 / 18.1 | 1.5 / 4.3 | 187.9 / 306.0 | 1.5 / 5.7 |
| 1,000 | go | 185.5 / 276.0 | 9.1 / 23.4 | 1.6 / 13.3 | 191.9 / 312.8 | 1.6 / 15.2 |
| 1,250 | go | 192.0 / 287.1 | 9.8 / 36.9 | 1.9 / 26.1 | 195.9 / 324.3 | 2.2 / 41.9 |
| 1,500 | go | 234.8 / 402.7 | 19.6 / 125.7 | 13.9 / 114.8 | 277.1 / 422.6 | 32.0 / 277.1 |

What the curves say:

- **`hello`: indistinguishable up to 75,000 req/s.** Both servers are on
  the generator's floor. Cove's p99 lifts to 7 ms at 100,000 (Go 3.1) and
  Cove saturates at 125,000, where it answered about 105,000 req/s. Go stays
  at 5.5 ms through 125,000 and saturates at 175,000. So Cove gets tight at
  **about 80% of Go's rate**, and the 1.4× capacity ratio is the whole
  story. A request at either server costs far less than the client can
  resolve.
- **`crunch`: flat, then a wall, as a CPU-bound queue should be.** Cove's
  p99 is 7–7.3 ms up to 750 req/s (85% of its capacity) and 224 ms at 900.
  Go's is 3.4–4.1 ms up to 3,000 and 5.6 ms at 3,500. Below saturation the
  difference is the run itself: 4.2 against 1.0 ms. The knee is at the same
  fraction of each server's capacity, so the slice and the run queues add no
  tail of their own.
- **`aggregate`: the same up to 20,000 req/s** (p99 271 against 270 ms, all
  of it upstream). At 30,000 to 35,000 Cove's p50 rises to 207–242 ms
  against Go's 190–192, and Cove saturates past 35,000 while Go answers
  45,000 at p50 199 ms.
- **The mix: Cove's `hello` gets tight at 200 req/s, Go's at about 600.**
  Cove's p99 for `hello` is 7.3 ms at 200 req/s, 18 ms at 300 and 29 ms at
  350, its saturation. Go's stays at 3 ms through 400 req/s and reaches
  13 ms at 1,000 and 115 ms at 1,500. Measured *against each server's own
  capacity* the two are alike: Cove at 72% (250 req/s) has `hello` p99
  8.5 ms, and Go at 72% (1,000 req/s) has 13.3 ms. The edge scheduler
  shares four workers between long and short runs about as well as Go's,
  and the absolute gap is `crunch` taking 4.2× longer.
- **At low rates in the mix, Cove's `aggregate` is 80 ms slower** (p50 262
  against 182 ms at 50 req/s, and 222 against 188 at 100). The gap closes as
  the load rises (199 against 184 at 300). This is the macOS idle-timer
  overshoot `examples/edge/README.md` describes under "numbers": the
  parking lot waits in `recv_timeout`, and when the process is mostly idle
  that wakes late. Go's timers wake on time at the same load.
  `impatient`'s p99 is 362 against 303 ms for the same reason.

### Connections

The same 10,000 `hello` requests a second, spread over more and more
kept-alive connections (4 s, 3 runs, median). This experiment was added
because the first sweep, run with a pool of a second's worth of arrivals,
showed the edge server's `hello` p50 *worse* at 10,000 req/s (8.6 ms) than at
saturation, and 453 µs of CPU per request at 2,500 req/s.

| connections | Cove p50 / p99 ms | Go p50 / p99 ms | Cove CPU µs/req | Go CPU µs/req |
| ---: | ---: | ---: | ---: | ---: |
| 64 | 1.39 / 2.93 | 1.35 / 2.92 | 66 | 46 |
| 256 | 1.23 / 2.92 | 1.32 / 2.92 | 94 | 47 |
| 1,000 | 1.33 / 4.17 | 1.31 / 2.94 | 135 | 51 |
| 4,000 | 3.82 / 11.82 | 0.74 / 2.99 | 142 | 61 |
| 10,000 | 8.16 / 22.05 | 0.06 / 2.64 | 143 | 62 |

**Up to 1,000 connections the two servers are the same**, and from 4,000 on
the edge server falls behind: 136× Go's p50 at 10,000 connections and 2.3×
its CPU per request. The cause is the one
[`host/src/idle.rs`](../host/src/idle.rs) names: the idle thread `poll(2)`s
every idle connection on every wake-up, so each request that goes idle costs
O(idle connections). Go's netpoller is kqueue, which is O(ready). Opening a
connection also costs something here: the generator reopened more of them
against the edge server (12,200 against 6,700 at 10,000) because its
connections were answered later and passed the generator's 2 s idle drop
more often.

### Waiting requests

`aggregate` with every upstream call at 1 s (so 3 s per request), N
requests at once, a connection each. RSS is sampled with `ps` every 20 ms;
growth is the peak minus the RSS after warm-up, divided by N. CPU is the
whole run's, divided by N.

![RSS growth and CPU per waiting request, 1,000 and 10,000 in flight](waiting.svg)

| in flight | server | answered 200 | RSS after warm-up MiB | peak RSS MiB | KiB per waiting request | CPU µs per request | p50 ms |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1,000 | cove | 1000 | 29.1 | 56.3 | 27.3 (25.8–29.0) | 140 (130–150) | 3294 |
| 1,000 | go | 1000 | 5.9 | 27.2 | 21.8 (21.7–21.8) | 150 (130–170) | 3001 |
| 10,000 | cove | 10000 | 29.7 | 303.3 | 28.0 (27.6–29.1) | 149 (148–152) | 3456 |
| 10,000 | go | 10000 | 5.8 | 227.2 | 22.7 (22.4–22.7) | 143 (139–148) | 3002 |

- **Memory: 27–28 KiB per waiting request on the edge server against
  22–23 KiB for Go**, 1.25×, at both 1,000 and 10,000. A parked run is
  18–22 KB by ADR 0080's and 0081's measurements, and the socket and
  buffers are the rest. A goroutine's stack and `net/http`'s per-connection
  buffers come to about the same on Go's side. Ten thousand waiting requests
  cost 303 MiB against 227. **The same class**: neither grows with anything
  but N.
- **CPU per request: equal**, 140–150 µs on both, connection setup
  included.
- **p50 3.29–3.46 s against 3.00 s.** The parking lot's timer overshoot
  again, about 100 ms on each of the three waits.
- At rest the edge server holds 29–30 MiB RSS against Go's 6 MiB, mostly the
  seven deployed tenants' prepared programs and the standard library's
  checked state. Idle CPU over 10 s was 0.08 s against 0.00 s.

## The native backend (ADR 0085)

[ADR 0085](../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md)
gave `OwnedVm` a native tier that can still be sliced: `PreparedProgram::with_native`
compiles a tenant once at deploy, every isolate shares the machine code, and a
run yields inside compiled code (at a backedge, an allocation or a call) and
resumes on another worker. `cove-edge --backend native` uses it; the scheduler,
the slice and the monitor are unchanged. These rows were measured on
2026-10-04, after the rest of this page, on the same machine, with other
agents' builds running: **load average 5 to 10** over the session (each row's
load is in its raw file). The rest of the page is unchanged and is the VM.

```console
$ cargo build --release -p cove-edge --features native
$ python3 examples/edge/compare/sweep.py capacity --reps 3 --scenario crunch cpu-io \
    --concurrency 16 64 256 --servers cove cove-native go --results examples/edge/compare/results/native
$ python3 examples/edge/compare/sweep.py sweep --reps 2 --scenario crunch \
    --servers cove cove-native go --results examples/edge/compare/results/native
$ examples/edge/compare/results/native/cpuio.sh 3 cpuio.txt        # the #588 mix, below
$ python3 examples/edge/compare/charts_native.py                     # the three SVGs here
```

Raw data: [`results/native/`](results/native/) — `capacity.jsonl`,
`sweep.jsonl` (and their console logs), `cpuio.txt`, and `hotpath.txt` for
Stage 1, with the scripts and parsers that made and read them.

### Stage 1 again: the edge path on the native backend

`cove-edge-compare` has three new rows: `edge-n`, the server's per-request
path on the native backend (a fresh isolate over the shared code, the
request value, the budget); and `edge+y` / `edge-n+y`, the same with a
monitor raising the run's yield request every 20 µs and each yield resumed
at once on the same thread. Medians of three interleaved rounds of 5 batches
(`results/native/hotpath.txt`); Go's column is the one above.

| case | edge (VM) | **edge-n** | native, reused `Vm` | Go | **edge-n / Go** | edge-n+y | yields per call |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `crunch n=2000` | 212.1 µs | 72.7 µs | 67.7 µs | 43.69 µs | **1.66×** | 73.2 µs | 0.4 |
| `crunch n=20000` | 4.37 ms | 1.35 ms | 1.34 ms | 997.40 µs | **1.36×** | 1.36 ms | 8.6 |
| `crunch n=150000` | 68.59 ms | 20.62 ms | 20.56 ms | 15.55 ms | **1.33×** | 20.69 ms | 127 |
| `hello name=Cove` | 8.46 µs | 8.73 µs | 4.13 µs | 54 ns | 162× | 8.84 µs | 0 |

- **The isolate costs the native tier nothing it did not cost the VM.**
  `edge-n` is within 1% of a reused native `Vm` from `n` = 20,000 up, as
  `edge` is of a reused VM: compiling is once per tenant, at deploy.
- **A yield and a resume cost about 0.6 µs** on the native tier, inferred as
  `edge-n+y − edge-n` over the yields per call, paired by round: 60, 76 and
  88 µs over 126.5–131.8 yields (0.47–0.69 µs each). That is the unwinding,
  the safepoint taken on resuming, and the re-entry of two compiled frames
  (`primesUpTo` and `isOddPrime`) through their resume prologues, on one
  thread with a warm cache. On another worker add the cold cache (the
  server's own yields below).
- `hello` gains nothing, as before: its time is not in compiled code.

### Capacity

Closed loop, as above; median of 3 runs (range in parentheses).

![Capacity: crunch at 16 and 64 in flight, cpu-io at 64 and 256, for Cove on the VM, Cove native and Go](native-capacity.svg)

| scenario | in flight | Cove VM | **Cove native** | Go | Go / native | native CPU µs/req | native p50 / p99 ms | Go p50 / p99 ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| crunch | 16 | 867 (865–906) | 2,707 (2,706–2,729) | 3,649 (2,671–3,671) | **1.35** | 1,480 | 5.8 / 9.2 | 4.1 / 10.1 |
| crunch | 64 | 880 (865–887) | 2,647 (2,621–2,733) | 3,638 (3,550–3,691) | **1.37** | 1,530 | 23.6 / 38.0 | 15.1 / 48.4 |
| cpu-io | 64 | 311 (280–333) | 754 (701–755) | 798 (767–806) | **1.06** | 3,720 | 16.7 / 307.8 | 10.6 / 302.2 |
| cpu-io | 256 | 342 (328–358) | 1,014 (983–1,020) | 1,297 (1,045–1,329) | **1.28** | 3,720 | 261.3 / 512.3 | 119.6 / 618.8 |

**`crunch` goes from 4.2× Go to 1.35×**, the same ratio as Stage 1's: the
server still adds nothing to a CPU-heavy run, and what is left is the
template compiler's code against Go's. CPU per request (`ps`, 10 ms
resolution) is 1.5 ms native against 4.6 ms on the VM and 1.1 ms for Go. The
`cpu-io` mix follows, to 1.06–1.28× Go from 2.4–4.1×, at 3.7 ms of CPU a
request against 11.6–13.5 ms on the VM and 2.7 ms for Go. (Go's own range at 16
in flight, 2,671–3,671, is one run under load; the other two agree.)

### Latency against a fixed rate: `crunch`

`--rate` with `--from-intended`, 2 repetitions, median p99 per rate
(`results/native/sweep.jsonl`).

![crunch p99 latency against offered rate: the VM saturates near 900 req/s, native near 2,600, Go near 3,600](native-crunch-p99.svg)

| offered req/s | Cove VM p50 / p99 | **Cove native p50 / p99** | Go p50 / p99 |
| ---: | ---: | ---: | ---: |
| 100 | 5.3 / 7.5 | 2.5 / 4.3 | 2.1 / 4.4 |
| 500 | 5.1 / 6.6 | 2.4 / 4.2 | 1.9 / 3.6 |
| 900 | 18.1 / 41.6 | 2.2 / 4.0 | 1.8 / 8.3 |
| 1,000 | saturated (288 / 836) | 2.3 / 4.0 | 1.6 / 3.2 |
| 1,500 | – | 2.2 / 4.0 | 1.8 / 3.2 |
| 2,500 | – | 4.0 / 26.9 | 1.8 / 3.4 |
| 3,000 | – | saturated (326 / 664) | 1.9 / 4.4 |
| 3,500 | – | – | 2.4 / 7.8 |

Below saturation the native backend's p50 is 0.4–0.6 ms above Go's, which is
the 1.35 ms run against Go's 1.0 ms; its p99 is within the load generator's
floor of Go's. It saturates near 2,600 req/s where Go saturates near 3,600,
which is the capacity ratio again.

### The `cpu-io` mix: does slicing still work?

The question for a native tier is whether it can still be interrupted, so
this is the edge README's #588 measurement — `--mix cpu-io --requests 500
--concurrency 200 --keep-alive --rate 330`, four workers, the stealing
scheduler — with `--backend` and `--slice` varied, three runs each, from the
intended start (`results/native/cpuio.txt`; the same runs without
`--from-intended` are in the file and agree).

![hello's p99 under the cpu-io mix: 99 ms on the VM unsliced, 15 ms sliced; 3 ms native at the #588 rate either way; at three times the rate 42 ms native unsliced and 14 ms sliced](native-cpu-io.svg)

| backend, slice | rate | `hello` p50 / **p99** | `crunch` p50 / p99 | `aggregate` p50 | yields per run |
| --- | ---: | ---: | ---: | ---: | ---: |
| VM, none | 330 | 17.3 / **99.4** ms | 46.1 / 142 ms | 267 ms | 0 |
| VM, 2 ms | 330 | 3.9 / **14.8** ms | 43.4 / 184 ms | 213 ms | 1,195 |
| native, none | 330 | 1.6 / **2.9** ms | 7.0 / 23.0 ms | 196 ms | 0 |
| native, 2 ms | 330 | 1.5 / **3.0** ms | 7.1 / 23.0 ms | 201 ms | 14–20 |
| native, none | 990 | 4.4 / **41.9** ms | 14.6 / 56.5 ms | 211 ms | 0 |
| native, 2 ms | 990 | 2.8 / **14.1** ms | 13.8 / 68.8 ms | 196 ms | 979–1,076 |

- **At the #588 rate the native pool is no longer busy enough to block.** A
  `crunch` takes about 7 ms instead of 30, so `hello` rarely finds every
  worker taken: 2.9 ms p99 with no slice, 3.0 ms with one, and the monitor
  asks for a yield 14 to 20 times a run. Faster code removed most of the
  head-of-line blocking by itself.
- **At three times the rate the blocking comes back, and slicing removes
  it.** With the pool as busy as the VM was at 330, unsliced native `hello`
  has a p99 of **41.9 ms** (36.1–49.8); with a 2 ms slice it is **14.1 ms**
  (12.3–15.5), the #588 level (15.6 ms, here 14.8 on the VM) at three times
  its throughput. The yields happen inside compiled code — on the VM the
  same slice yields about 1,200 times a run at a third of the rate — and
  `crunch`'s p99 pays for `hello`'s, 56.5 → 68.8 ms, as it did on the VM.
- So a native isolate that could not be interrupted would have been a
  regression for the interactive tenant at the load the native tier makes
  affordable, and it is not one.

## Diagnosis: where to work next

Ranked by how much of the gap each item explains for the work that has it.

1. **CPU-heavy work: the VM, and nothing else.** `crunch`'s gap is 4.2× in
   Stage 1 and 4.2× at the server, and its server CPU per request matches
   Stage 1 within 8%. The isolates, the boundary, the HTTP path and the
   scheduler add nothing measurable to a 4 ms run, and the slice does not
   cost throughput (the edge README measured this too). **The native tier
   already closes it to 1.3×**, but an edge isolate is an `OwnedVm` over a
   `PreparedProgram`, which has no native tier. (Done since: [ADR
   0085](../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md)
   and `--backend native`, measured [above](#the-native-backend-adr-0085) —
   `crunch` at 1.35× Go and the mix at 1.06–1.28×, still sliceable.) The first piece of work is
   the native tier for the embedding API (`PreparedProgram` →
   `NativeProgram` once per tenant, used by every `OwnedVm`). After that,
   the remaining 1.3× is the template compiler's own code quality. This
   would move `crunch`, and the `cpu-io` mix with it, from 4.2× to about
   1.3×. The mix's interactive latency would follow, since its gap is
   `crunch`'s run time behind which `hello` waits.
2. **Light work: a fixed per-request cost, and most of it is the host, not
   the isolate.** `hello` costs 41 µs of server CPU against Go's 23 µs, so
   capacity is 1.37–1.46× Go's. Of the 41 µs:
   - **7 µs** is entering and leaving an isolate: about 3.7–4.2 µs for the
     isolate, the request value and the budget, and 3.0 µs for an `invoke`
     that does nothing (moving the `Request` into the VM and the
     `Response` out).
   - **1 µs** is the handler.
   - **about 26–33 µs** is the edge host's own HTTP read and parse, the run
     queue, the idle-thread hand-off, and the write.

   For the runtime, the boundary (3 µs per call, with no native-tier
   benefit) and isolate creation (about 3 µs) are the targets. For the
   example server, the HTTP path matters more and is not a runtime
   question. Below 75,000 req/s none of this is visible in latency.
3. **Concurrency: two host-level I/O problems, and the scheduler is fine.**
   - **Idle connections**: the edge server is equal to Go up to 1,000
     kept-alive connections and 136× its p50 at 10,000, because the idle
     thread's `poll(2)` is O(idle connections) per wake-up. kqueue/epoll,
     as `idle.rs` already says, is the fix. It is the one result here
     where the gap *changes class* with load.
   - **Timer precision at low load**: the parking lot's `recv_timeout`
     overshoots on an idle macOS process. That adds 80 ms to `aggregate`'s
     median at 50 req/s and about 100 ms per wait on a 1 s upstream, where
     Go's timers are on time. A timer that is not a timed channel wait
     (`kevent` with `EVFILT_TIMER`, or a short spin before the deadline)
     would be the experiment.
   - **The scheduler and memory**: neither widens the gap. Against each
     server's own capacity, the mix's `hello` tail is the same shape on
     both, and a waiting request costs 1.25× Go's memory, flat from 1,000
     to 10,000. `aggregate` at 10,000 in flight is 1.32× Go's throughput
     and twice its CPU per request (park, resume, timer), which is worth a
     profile but is not a class change.

### Uncertainty

- The machine was shared with other agents' builds (load average 2.5–9).
  Repeated runs agreed closely: most spreads in the tables are a few
  percent. But every figure is one machine's, and macOS's.
- The generator shares the machine and has a floor of about 1.5/3 ms
  (p50/p99). Below that floor the two servers cannot be told apart here.
  The server-side figures (`/_stats`, `--timeline`) are the edge server's
  only. Go's server-side latency was not instrumented.
- CPU per request from `ps -o time` has 10 ms resolution. Runs are long
  enough that this is under 1%, but at low rates the figure includes each
  server's per-wake-up overhead, which is why CPU per request falls as the
  rate rises on both.
- The attribution of `hello`'s 41 µs is by difference between separate
  measurements (Stage 1, the timeline, `ps`), not one profile. The 26–33 µs
  host share is a remainder and carries the error of all three.
- The Go server is ordinary, not tuned beyond the transport's idle pool:
  no `GOGC` tuning, no pooling, no `fasthttp`. A tuned Go server would be
  faster. An ordinary one is the baseline that was asked for.
