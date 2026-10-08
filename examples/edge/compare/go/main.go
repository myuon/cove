// edge-go: examples/edge's service written the ordinary way in Go —
// net/http, a goroutine per connection, time.Timer for the simulated
// upstream, context.WithTimeout for a deadline — as the baseline the Cove
// server is compared against (../README.md).
//
//	go build -o edge-go . && GOMAXPROCS=4 ./edge-go -port 8788 -quiet
//	./edge-go bench [-batches 7] [-min-ms 300]
//
// The paths and the bodies are the edge server's: /hello/, /crunch/,
// /counter/, /aggregate/, /impatient/ (aggregate under a 300 ms deadline,
// answered 504 past it), /proxy/ (a real GET of ?url= on an allowlist of
// 127.0.0.1 and localhost, under 500 ms), and /_stats, which cove-edge-load
// resets before a run and prints after it.
//
// What is not here, on purpose: no isolate per request, no
// host-call budget, no capability check, no record/replay, no preemption
// other than the Go runtime's own. Those are the differences the comparison
// is about; see the conditions table in ../README.md.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"runtime"
	"strconv"
	"strings"
	"sync/atomic"
	"syscall"
	"time"
)

type server struct {
	quiet   bool
	latency latency
	counter *counterStore
	client  *http.Client
	started time.Time

	served       atomic.Int64
	errors       atomic.Int64
	timeouts     atomic.Int64
	inFlight     atomic.Int64
	inFlightPeak atomic.Int64
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "bench" {
		bench(os.Args[2:])
		return
	}
	port := flag.Int("port", 8788, "port to listen on")
	host := flag.String("host", "127.0.0.1", "address to listen on")
	quiet := flag.Bool("quiet", false, "no counter log lines")
	lat := flag.String("latency", "20..100", "simulated upstream latency MIN..MAX in ms")
	flag.Parse()

	min, max, err := parseLatency(*lat)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	raiseOpenFiles()
	s := &server{
		quiet:   *quiet,
		latency: latency{min: min, max: max},
		counter: &counterStore{visit: map[string]string{}},
		client: &http.Client{Transport: &http.Transport{
			// The ordinary tuning: the default of two idle connections per
			// host makes a server that fetches from itself under load open
			// and close a connection per fetch.
			MaxIdleConns:        1024,
			MaxIdleConnsPerHost: 1024,
			IdleConnTimeout:     5 * time.Second,
		}},
		started: time.Now(),
	}
	srv := &http.Server{
		Addr:        net.JoinHostPort(*host, strconv.Itoa(*port)),
		Handler:     s,
		IdleTimeout: 5 * time.Second,
		ReadTimeout: 5 * time.Second,
	}
	fmt.Printf("edge-go listening on http://%s — GOMAXPROCS %d, upstream latency %v..%v\n",
		srv.Addr, runtime.GOMAXPROCS(0), min, max)
	if err := srv.ListenAndServe(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func parseLatency(text string) (time.Duration, time.Duration, error) {
	lo, hi, ok := strings.Cut(text, "..")
	if !ok {
		hi = lo
	}
	a, err1 := strconv.Atoi(lo)
	b, err2 := strconv.Atoi(hi)
	if err1 != nil || err2 != nil || b < a {
		return 0, 0, fmt.Errorf("-latency is MIN..MAX in ms, not %q", text)
	}
	return time.Duration(a) * time.Millisecond, time.Duration(b) * time.Millisecond, nil
}

// raiseOpenFiles lifts the soft open-file limit to the hard one, as the edge
// server does, so that ten thousand sockets fit.
func raiseOpenFiles() {
	var limit syscall.Rlimit
	if syscall.Getrlimit(syscall.RLIMIT_NOFILE, &limit) == nil {
		limit.Cur = limit.Max
		if limit.Cur > 1<<20 {
			limit.Cur = 1 << 20
		}
		_ = syscall.Setrlimit(syscall.RLIMIT_NOFILE, &limit)
	}
}

func (s *server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	path := strings.TrimPrefix(r.URL.Path, "/")
	name, rest, found := strings.Cut(path, "/")
	rest = "/" + rest
	if !found {
		rest = "/"
	}
	if name == "_stats" {
		s.stats(w, r)
		return
	}
	n := s.inFlight.Add(1)
	for {
		peak := s.inFlightPeak.Load()
		if n <= peak || s.inFlightPeak.CompareAndSwap(peak, n) {
			break
		}
	}
	defer s.inFlight.Add(-1)

	query := map[string]string{}
	for key, values := range r.URL.Query() {
		query[key] = values[0]
	}
	var status int
	var body string
	switch name {
	case "hello":
		status, body = 200, hello(r.Method, rest, query)
	case "crunch":
		status, body = crunch(query)
	case "counter":
		status, body = 200, s.counter.handle(r.Method, rest, s.quiet)
	case "aggregate":
		status, body = s.aggregate(r.Context(), query, 5*time.Second)
	case "impatient":
		status, body = s.aggregate(r.Context(), query, 300*time.Millisecond)
	case "proxy":
		status, body = s.proxy(r.Context(), query)
	case "":
		status, body = 200, "edge-go: /hello/ /crunch/ /counter/ /aggregate/ /impatient/ /proxy/ /_stats\n"
	default:
		status, body = 404, fmt.Sprintf("no tenant named `%s`\n", name)
	}
	switch {
	case status == 504:
		s.timeouts.Add(1)
		s.errors.Add(1)
	case status >= 500:
		s.errors.Add(1)
	}
	s.served.Add(1)
	w.Header().Set("Content-Type", "text/plain")
	w.Header().Set("Content-Length", strconv.Itoa(len(body)))
	w.WriteHeader(status)
	_, _ = io.WriteString(w, body)
}

// aggregate is tenants/aggregate/aggregate.cove's handle: the services in
// turn, each a simulated upstream call, under a deadline from the start of
// the handler (the edge server's deadline starts with the run).
func (s *server) aggregate(parent context.Context, query map[string]string, deadline time.Duration) (int, string) {
	ctx, cancel := context.WithTimeout(parent, deadline)
	defer cancel()
	wanted, ok := query["services"]
	if !ok {
		wanted = "weather,stocks,news"
	}
	var lines []string
	for _, service := range strings.Split(wanted, ",") {
		took := upstreamLatency(service, s.latency)
		timer := time.NewTimer(took)
		select {
		case <-timer.C:
		case <-ctx.Done():
			timer.Stop()
			return 504, fmt.Sprintf("deadline of %v exceeded\n", deadline)
		}
		answer, err := upstreamAnswer(service, took)
		if err != nil {
			lines = append(lines, fmt.Sprintf("%s: unavailable (%s)", service, err))
		} else {
			lines = append(lines, service+": "+answer)
		}
	}
	return 200, strings.Join(lines, "\n") + "\n"
}

// allowlist is tenants/edge.toml's `[proxy] fetch`.
var allowlist = map[string]bool{"127.0.0.1": true, "localhost": true}

// proxy is tenants/proxy/proxy.cove's handle with the server's fetch: a real
// GET, refused before it is sent unless http:// and on the allowlist, under
// a 500 ms deadline.
func (s *server) proxy(parent context.Context, query map[string]string) (int, string) {
	target := query["url"]
	parsed, err := url.Parse(target)
	if err != nil || parsed.Scheme != "http" {
		return 502, fmt.Sprintf("`%s` is not an http:// URL\n", target)
	}
	if !allowlist[parsed.Hostname()] {
		return 502, fmt.Sprintf("`%s` is not on tenant `proxy`'s fetch allowlist (127.0.0.1, localhost)\n", parsed.Hostname())
	}
	ctx, cancel := context.WithTimeout(parent, 500*time.Millisecond)
	defer cancel()
	request, err := http.NewRequestWithContext(ctx, "GET", target, nil)
	if err != nil {
		return 502, err.Error() + "\n"
	}
	response, err := s.client.Do(request)
	if err != nil {
		if errors.Is(err, context.DeadlineExceeded) {
			return 504, "deadline of 500ms exceeded\n"
		}
		return 502, err.Error() + "\n"
	}
	defer response.Body.Close()
	fetched, err := io.ReadAll(io.LimitReader(response.Body, 1<<20))
	if err != nil {
		if errors.Is(err, context.DeadlineExceeded) {
			return 504, "deadline of 500ms exceeded\n"
		}
		return 502, err.Error() + "\n"
	}
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		first, _, _ := strings.Cut(string(fetched), "\n")
		return 502, fmt.Sprintf("`%s` answered %d: %s\n", target, response.StatusCode, first)
	}
	return 200, target + " said:\n" + string(fetched)
}

// stats is a cut-down /_stats: what cove-edge-load resets and prints.
func (s *server) stats(w http.ResponseWriter, r *http.Request) {
	var usage syscall.Rusage
	_ = syscall.Getrusage(syscall.RUSAGE_SELF, &usage)
	var memory runtime.MemStats
	runtime.ReadMemStats(&memory)
	body := fmt.Sprintf("{\n  \"uptime_s\": %.1f,\n  \"gomaxprocs\": %d,\n  \"served\": %d,\n  \"errors\": %d,\n  \"timeouts\": %d,\n  \"in_flight\": %d,\n  \"in_flight_peak\": %d,\n  \"goroutines\": %d,\n  \"heap_inuse_kib\": %d,\n  \"sys_kib\": %d,\n  \"peak_rss_kib\": %d,\n  \"cpu_user_s\": %.3f,\n  \"cpu_sys_s\": %.3f\n}\n",
		time.Since(s.started).Seconds(), runtime.GOMAXPROCS(0), s.served.Load(), s.errors.Load(),
		s.timeouts.Load(), s.inFlight.Load(), s.inFlightPeak.Load(), runtime.NumGoroutine(),
		memory.HeapInuse/1024, memory.Sys/1024, maxRSSKiB(&usage),
		seconds(usage.Utime), seconds(usage.Stime))
	if _, reset := r.URL.Query()["reset"]; reset {
		s.served.Store(0)
		s.errors.Store(0)
		s.timeouts.Store(0)
		s.inFlightPeak.Store(s.inFlight.Load())
	}
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Content-Length", strconv.Itoa(len(body)))
	_, _ = io.WriteString(w, body)
}

func seconds(t syscall.Timeval) float64 {
	return float64(t.Sec) + float64(t.Usec)/1e6
}

// maxRSSKiB is ru_maxrss in KiB: macOS reports bytes, Linux KiB.
func maxRSSKiB(usage *syscall.Rusage) int64 {
	if runtime.GOOS == "darwin" {
		return usage.Maxrss / 1024
	}
	return usage.Maxrss
}
