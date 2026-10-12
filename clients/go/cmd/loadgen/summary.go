package main

import (
	"fmt"
	"io"
	"sort"
	"strings"
	"text/tabwriter"
)

var opOrder = []string{opRead, opSearchFam, opSearchObs, opPostPat, opPostObs, opPutPat, opVerifyRYW}

func orderedKeys(m map[string]opReport, preferred []string) []string {
	var keys []string
	seen := map[string]bool{}
	for _, k := range preferred {
		if _, ok := m[k]; ok {
			keys = append(keys, k)
			seen[k] = true
		}
	}
	var rest []string
	for k := range m {
		if !seen[k] {
			rest = append(rest, k)
		}
	}
	sort.Strings(rest)
	return append(keys, rest...)
}

func tableRow(tw *tabwriter.Writer, name string, r opReport) {
	l := r.LatencyMS
	fmt.Fprintf(tw, "%s\t%d\t%d\t%.1f\t%.2f\t%.2f\t%.2f\t%.2f\n",
		name, r.Count, r.Errors, r.OpsPerSec, l.P50, l.P95, l.P99, l.Max)
}

// printSummary writes the human-readable report. Latencies are milliseconds
// over successful (2xx) requests; ops/s counts every completed request.
func printSummary(w io.Writer, rep *report) {
	c := rep.Config
	fmt.Fprintf(w, "\nloadgen: %v for %.1fs, concurrency %v, mix %v, seeded %d patients\n",
		c["duration"], rep.ElapsedSeconds, c["concurrency"], c["mix"], rep.SeededPatients)
	fmt.Fprintf(w, "bases: %v   write-base: %v\n\n", c["bases"], c["write_base"])

	tw := tabwriter.NewWriter(w, 0, 0, 2, ' ', 0)
	fmt.Fprintln(tw, "OP\tCOUNT\tERRORS\tOPS/S\tp50 ms\tp95 ms\tp99 ms\tmax ms")
	tableRow(tw, "TOTAL (mix)", rep.Total)
	for _, k := range orderedKeys(rep.Classes, []string{classRead, classSearch, classWrite, classVerify}) {
		tableRow(tw, "class:"+k, rep.Classes[k])
	}
	for _, k := range orderedKeys(rep.Ops, opOrder) {
		tableRow(tw, "  "+k, rep.Ops[k])
	}
	tw.Flush()

	for _, k := range orderedKeys(rep.Ops, opOrder) {
		r := rep.Ops[k]
		if r.Errors == 0 {
			continue
		}
		var parts []string
		codes := make([]string, 0, len(r.Status))
		for code := range r.Status {
			codes = append(codes, code)
		}
		sort.Strings(codes)
		for _, code := range codes {
			parts = append(parts, fmt.Sprintf("%s=%d", code, r.Status[code]))
		}
		fmt.Fprintf(w, "\n%s errors: %d (status: %s)\n", k, r.Errors, strings.Join(parts, " "))
		for _, s := range r.ErrorSamples {
			fmt.Fprintf(w, "  e.g. %s\n", s)
		}
	}

	y := rep.RYW
	if y.Enabled || y.Verify {
		fmt.Fprintf(w, "\nread-your-writes: enabled=%v verify=%v writes_with_commit_ts=%d gets_with_min_ts=%d\n",
			y.Enabled, y.Verify, y.WritesWithCommitTS, y.ReadsWithMinTS)
		if y.Verify {
			fmt.Fprintf(w, "  verify checks=%d violations=%d (not_found=%d stale=%d) skipped_no_id=%d\n",
				y.Checks, y.Violations, y.NotFound, y.Stale, y.SkippedNoID)
		}
		if y.WritesWithCommitTS == 0 {
			fmt.Fprintln(w, "  note: no X-Refhir-Commit-Ts header seen on write responses; Min-Ts was not sent")
		}
	}

	if len(rep.ServerStats) == 0 {
		fmt.Fprintln(w, "\nserver stats: none (healthz?verbose=1 unavailable or not JSON)")
		return
	}
	fmt.Fprintln(w, "\nserver stats (healthz?verbose=1, after run):")
	for _, b := range rep.Config["bases"].([]string) {
		raw, ok := rep.ServerStats[b]
		if !ok {
			continue
		}
		s := string(raw)
		if len(s) > 1200 {
			s = s[:1200] + "..."
		}
		fmt.Fprintf(w, "  %s %s\n", b, s)
	}
}
