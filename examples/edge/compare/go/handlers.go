package main

// The tenants of examples/edge, written the ordinary way: a function per
// endpoint, plain Go values in and out. Each one answers the bytes its Cove
// counterpart answers for the requests the load tests make; nothing here is
// isolated, metered or granted anything — that is the point of the baseline.

import (
	"fmt"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// --------------------------------------------------------------- crunch

// crunch is tenants/crunch/crunch.cove's handle, line for line.
func crunch(query map[string]string) (int, string) {
	text, ok := query["n"]
	if !ok {
		text = "20000"
	}
	asked, err := strconv.ParseInt(text, 10, 64)
	if err != nil {
		asked = -1
	}
	if asked < 2 || asked > 200000 {
		return 400, "?n= must be a whole number from 2 to 200000\n"
	}
	count := primesUpTo(asked)
	largest := largestPrimeUpTo(asked)
	return 200, fmt.Sprintf("%d primes up to %d, the largest %d\n", count, asked, largest)
}

// primesUpTo is how many primes there are from 2 to n.
func primesUpTo(n int64) int64 {
	count := int64(1)
	candidate := int64(3)
	for candidate <= n {
		if isOddPrime(candidate) {
			count++
		}
		candidate += 2
	}
	return count
}

// largestPrimeUpTo is the largest prime no greater than n, found counting
// down.
func largestPrimeUpTo(n int64) int64 {
	candidate := n - 1 + n%2
	for candidate > 2 {
		if isOddPrime(candidate) {
			return candidate
		}
		candidate -= 2
	}
	return 2
}

// isOddPrime is whether an odd candidate above 1 has no odd divisor up to
// its root.
func isOddPrime(candidate int64) bool {
	divisor := int64(3)
	for divisor*divisor <= candidate {
		if candidate%divisor == 0 {
			return false
		}
		divisor += 2
	}
	return true
}

// ---------------------------------------------------------------- hello

// hello is tenants/hello/hello.cove's handle, without `/spin`: an ordinary
// Go handler has no fuel to stop a loop with, so the endpoint whose only
// purpose is to show a budget stopping one is not ported.
func hello(method, path string, query map[string]string) string {
	name, ok := query["name"]
	if !ok {
		name = "world"
	}
	return "Hello, " + name + "! (" + method + " " + path + ")\n"
}

// -------------------------------------------------------------- counter

// counterStore is counter's kv store: a map behind a lock, as the edge
// server's `kv` host is.
type counterStore struct {
	mu    sync.Mutex
	visit map[string]string
}

func (s *counterStore) handle(method, path string, quiet bool) string {
	key := "visits:" + path
	s.mu.Lock()
	seen, err := strconv.ParseInt(s.visit[key], 10, 64)
	if err != nil {
		seen = 0
	}
	now := seen + 1
	s.visit[key] = strconv.FormatInt(now, 10)
	s.mu.Unlock()
	if !quiet {
		fmt.Printf("[counter] %s %s -> %d\n", method, path, now)
	}
	return fmt.Sprintf("%s has been visited %d time(s)\n", path, now)
}

// ------------------------------------------------------------- upstream

// latency is the simulated upstream's range, as the edge server's
// `--latency MIN..MAX`.
type latency struct {
	min, max time.Duration
}

// pick is hosts.rs's `Latency::pick`: a splitmix step over a per-call
// counter, so the same calls see the same latencies on both servers.
func (l latency) pick(seed uint64) time.Duration {
	span := uint64((l.max - l.min) / time.Microsecond)
	if span == 0 {
		return l.min
	}
	z := seed + 0x9e3779b97f4a7c15
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	z ^= z >> 31
	return l.min + time.Duration(z%(span+1))*time.Microsecond
}

// calls numbers every simulated upstream call, which seeds its latency —
// the server's `calls` counter.
var calls atomic.Uint64

// upstreamLatency is hosts.rs's `upstream_latency`: `hang` answers after an
// hour.
func upstreamLatency(service string, l latency) time.Duration {
	if service == "hang" {
		return time.Hour
	}
	return l.pick(calls.Add(1) - 1)
}

// upstreamAnswer is hosts.rs's `upstream_answer`.
func upstreamAnswer(service string, took time.Duration) (string, error) {
	var body string
	switch {
	case service == "weather":
		body = "sunny, 21C"
	case service == "stocks":
		body = "COVE +3.2%"
	case service == "news":
		body = "parked isolates resume on any thread"
	case service == "":
		return "", fmt.Errorf("no service named")
	case strings.HasPrefix(service, "fail"):
		return "", fmt.Errorf("`%s` is down", service)
	default:
		body = "ok"
	}
	return fmt.Sprintf("%s [%d ms]", body, took.Milliseconds()), nil
}
