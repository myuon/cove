package main

// `edge-go bench`: stage 1's Go column — the time one call of the handler
// takes, in-process, in the format cove-edge-compare prints.
//
//	go: the handler with the query map built once (cove-edge-compare's `vm`)
//	go+map: the query map built per call too (its `edge` builds the request)

import (
	"flag"
	"fmt"
	"os"
	"sort"
	"time"
)

var sink string

type benchCase struct {
	label string
	query map[string]string
	call  func(map[string]string) string
}

func bench(args []string) {
	flags := flag.NewFlagSet("bench", flag.ExitOnError)
	batches := flags.Int("batches", 7, "batches per case; the median is printed")
	minMs := flags.Int("min-ms", 300, "the least a batch lasts")
	_ = flags.Parse(args)

	crunchBody := func(query map[string]string) string {
		_, body := crunch(query)
		return body
	}
	helloBody := func(query map[string]string) string { return hello("GET", "/", query) }
	cases := []benchCase{
		{"crunch n=2000", map[string]string{"n": "2000"}, crunchBody},
		{"crunch n=20000", map[string]string{"n": "20000"}, crunchBody},
		{"crunch n=150000", map[string]string{"n": "150000"}, crunchBody},
		{"hello name=Cove", map[string]string{"name": "Cove"}, helloBody},
	}
	fmt.Println("mode    case              median ns/call   min..max ns/call   calls/batch")
	for _, c := range cases {
		query := c.query
		reused := func() string { return c.call(query) }
		fresh := func() string {
			built := make(map[string]string, len(query))
			for k, v := range query {
				built[k] = v
			}
			return c.call(built)
		}
		row("go", c.label, measure(reused, time.Duration(*minMs)*time.Millisecond, *batches))
		row("go+map", c.label, measure(fresh, time.Duration(*minMs)*time.Millisecond, *batches))
		fmt.Printf("        %-17s answer: %q\n", c.label, reused())
	}
	if sink == "never" {
		os.Exit(3)
	}
}

func measure(call func() string, min time.Duration, batches int) [4]float64 {
	calls := 1
	for {
		started := time.Now()
		for i := 0; i < calls; i++ {
			sink = call()
		}
		if time.Since(started) >= min/4 {
			break
		}
		calls *= 2
	}
	calls *= 4
	per := make([]float64, batches)
	for b := range per {
		started := time.Now()
		for i := 0; i < calls; i++ {
			sink = call()
		}
		per[b] = float64(time.Since(started).Nanoseconds()) / float64(calls)
	}
	sort.Float64s(per)
	return [4]float64{per[len(per)/2], per[0], per[len(per)-1], float64(calls)}
}

func row(mode, label string, m [4]float64) {
	fmt.Printf("%-7s %-17s %14.0f   %8.0f..%-8.0f   %.0f\n", mode, label, m[0], m[1], m[2], m[3])
}
