// Load generator for a running ReFHIR server (plain FHIR REST, stdlib only).
//
//	go run ./cmd/loadgen --base http://127.0.0.1:8080 --duration 20s --concurrency 32 \
//	    --mix read=60,search=25,write=15 --seed-patients 200 --out result.json
//
// Writes go to --write-base (default: the first --base); reads and searches are
// round-robined across every --base. See docs/perf/README.md.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

const defaultBase = "http://127.0.0.1:8080"

// stringList is a repeatable flag that also accepts comma-separated values.
type stringList []string

func (s *stringList) String() string { return strings.Join(*s, ",") }

func (s *stringList) Set(v string) error {
	for _, p := range strings.Split(v, ",") {
		if p = strings.TrimSpace(p); p != "" {
			*s = append(*s, p)
		}
	}
	return nil
}

func normalizeBase(b string) (string, error) {
	b = strings.TrimRight(strings.TrimSpace(b), "/")
	if !strings.Contains(b, "://") {
		b = "http://" + b
	}
	u, err := url.Parse(b)
	if err != nil || u.Host == "" || (u.Scheme != "http" && u.Scheme != "https") {
		return "", fmt.Errorf("invalid base URL %q", b)
	}
	return b, nil
}

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	code := run(ctx, os.Args[1:], os.Stdout, os.Stderr)
	stop()
	os.Exit(code)
}

func run(ctx context.Context, args []string, stdout, stderr io.Writer) int {
	logf := func(format string, a ...any) { fmt.Fprintf(stderr, "loadgen: "+format+"\n", a...) }

	fs := flag.NewFlagSet("loadgen", flag.ContinueOnError)
	fs.SetOutput(stderr)
	var bases stringList
	fs.Var(&bases, "base", "ReFHIR base URL; repeatable or comma-separated (reads/searches round-robin across them; default "+defaultBase+")")
	writeBase := fs.String("write-base", "", "base URL that receives all writes (default: first --base)")
	duration := fs.Duration("duration", 20*time.Second, "how long to generate load (after seeding)")
	concurrency := fs.Int("concurrency", 32, "concurrent workers")
	mixSpec := fs.String("mix", "read=60,search=25,write=15", "workload mix as percentages: read=,search=,write=")
	seedN := fs.Int("seed-patients", 200, "Patients to create before the run (reads, searches, observations and PUTs target them)")
	token := fs.String("token", "", "optional bearer token")
	out := fs.String("out", "", "write the JSON report to this path")
	ryw := fs.Bool("read-your-writes", false, "send the last X-Refhir-Commit-Ts a worker saw as X-Refhir-Min-Ts on its next reads")
	verify := fs.Bool("verify-ryw", false, "after each write, immediately GET it from a round-robin base (with Min-Ts if known) and count 404/stale reads")
	timeout := fs.Duration("timeout", 30*time.Second, "per-request timeout")
	printJSON := fs.Bool("json", true, "also print the JSON report to stdout")
	progress := fs.Duration("progress", 5*time.Second, "progress line interval on stderr (0 disables)")
	randSeed := fs.Int64("rand-seed", 0, "RNG seed (0 = time-based)")
	if err := fs.Parse(args); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return 0
		}
		return 2
	}

	cfg := &config{
		Duration: *duration, Concurrency: *concurrency, MixSpec: *mixSpec, SeedPatients: *seedN,
		Token: *token, Out: *out, RYW: *ryw, VerifyRYW: *verify, Timeout: *timeout,
		PrintJSON: *printJSON, Progress: *progress, RandSeed: *randSeed,
	}
	if len(bases) == 0 {
		bases = stringList{defaultBase}
	}
	for _, b := range bases {
		nb, err := normalizeBase(b)
		if err != nil {
			logf("%v", err)
			return 2
		}
		cfg.Bases = append(cfg.Bases, nb)
	}
	cfg.WriteBase = cfg.Bases[0]
	if *writeBase != "" {
		wb, err := normalizeBase(*writeBase)
		if err != nil {
			logf("%v", err)
			return 2
		}
		cfg.WriteBase = wb
	}
	m, err := parseMix(cfg.MixSpec)
	if err != nil {
		logf("--mix: %v", err)
		return 2
	}
	cfg.Mix = m
	switch {
	case cfg.Concurrency < 1:
		logf("--concurrency must be >= 1")
		return 2
	case cfg.Duration <= 0:
		logf("--duration must be > 0")
		return 2
	case cfg.SeedPatients < 0:
		logf("--seed-patients must be >= 0")
		return 2
	case cfg.SeedPatients == 0 && (m.Read > 0 || m.Search > 0):
		logf("--seed-patients must be > 0 when the mix includes read or search")
		return 2
	}
	if cfg.RandSeed == 0 {
		cfg.RandSeed = time.Now().UnixNano()
	}

	rn := &runner{cfg: cfg, client: newClient(cfg), runID: strconv.FormatInt(time.Now().UnixNano()/1e6, 36)}
	defer rn.client.CloseIdleConnections()

	// Preflight: every base must answer /metadata at the transport level.
	for _, b := range cfg.Bases {
		pctx, cancel := context.WithTimeout(ctx, 10*time.Second)
		r := rn.do(pctx, http.MethodGet, b, "/metadata", nil, "")
		cancel()
		if r.err != nil {
			logf("cannot reach %s: %v", b, r.err)
			return 1
		}
		if !r.ok() {
			logf("warning: GET %s/metadata returned HTTP %d", b, r.status)
		}
	}

	if cfg.SeedPatients > 0 {
		logf("seeding %d patients via %s ...", cfg.SeedPatients, cfg.WriteBase)
		t0 := time.Now()
		if err := rn.seed(ctx, logf); err != nil {
			logf("%v", err)
			return 1
		}
		logf("seeded %d patients in %s", len(rn.seeded), time.Since(t0).Round(time.Millisecond))
	}

	before := fetchServerStats(ctx, rn)

	logf("running %s, concurrency %d, mix %s, bases %s, write-base %s", cfg.Duration, cfg.Concurrency, cfg.MixSpec, strings.Join(cfg.Bases, ","), cfg.WriteBase)
	start := time.Now()
	deadline := start.Add(cfg.Duration)
	workers := make([]*worker, cfg.Concurrency)
	var wg sync.WaitGroup
	for i := range workers {
		workers[i] = newWorker(rn, i)
		wg.Add(1)
		go func(w *worker) {
			defer wg.Done()
			w.run(ctx, deadline)
		}(workers[i])
	}
	done := make(chan struct{})
	if cfg.Progress > 0 {
		go func() {
			tick := time.NewTicker(cfg.Progress)
			defer tick.Stop()
			for {
				select {
				case <-done:
					return
				case <-tick.C:
					logf("%5.0fs  ops=%d errors=%d", time.Since(start).Seconds(), rn.totalOps.Load(), rn.totalErrs.Load())
				}
			}
		}()
	}
	wg.Wait()
	close(done)
	elapsed := time.Since(start)

	after := fetchServerStats(ctx, rn)

	rep := buildReport(rn, workers, elapsed, before, after)
	printSummary(stdout, rep)
	data, err := json.MarshalIndent(rep, "", "  ")
	if err != nil {
		logf("encode report: %v", err)
		return 1
	}
	if cfg.PrintJSON {
		fmt.Fprintf(stdout, "\n%s\n", data)
	}
	if cfg.Out != "" {
		if err := os.WriteFile(cfg.Out, append(data, '\n'), 0o644); err != nil {
			logf("write %s: %v", cfg.Out, err)
			return 1
		}
		logf("wrote %s", cfg.Out)
	}
	if rep.Total.Count > 0 && rep.Total.Count == rep.Total.Errors {
		return 1 // nothing succeeded
	}
	if ctx.Err() != nil {
		return 130
	}
	return 0
}

// ---------------------------------------------------------------------------
// Server stats

// fetchServerStats GETs <base>/healthz?verbose=1 from every base. Best effort:
// failures and non-JSON bodies are skipped.
func fetchServerStats(ctx context.Context, rn *runner) map[string]json.RawMessage {
	stats := map[string]json.RawMessage{}
	for _, b := range rn.cfg.Bases {
		if _, dup := stats[b]; dup {
			continue
		}
		cctx, cancel := context.WithTimeout(ctx, 5*time.Second)
		r := rn.do(cctx, http.MethodGet, b, "/healthz?verbose=1", nil, "")
		cancel()
		if !r.ok() || !json.Valid(r.body) {
			continue
		}
		var compact bytes.Buffer
		if err := json.Compact(&compact, r.body); err == nil {
			stats[b] = json.RawMessage(compact.Bytes())
		}
	}
	return stats
}

// ---------------------------------------------------------------------------
// Report

type rywReport struct {
	Enabled            bool `json:"read_your_writes"`
	Verify             bool `json:"verify"`
	WritesWithCommitTS int  `json:"writes_with_commit_ts"`
	ReadsWithMinTS     int  `json:"gets_with_min_ts"`
	Checks             int  `json:"checks"`
	Violations         int  `json:"violations"`
	NotFound           int  `json:"violations_not_found"`
	Stale              int  `json:"violations_stale"`
	SkippedNoID        int  `json:"skipped_no_id"`
}

type report struct {
	Config         map[string]any             `json:"config"`
	StartedAt      string                     `json:"started_at"`
	ElapsedSeconds float64                    `json:"elapsed_s"`
	SeededPatients int                        `json:"seeded_patients"`
	Total          opReport                   `json:"total"` // read+search+write; excludes verify GETs
	Classes        map[string]opReport        `json:"classes"`
	Ops            map[string]opReport        `json:"ops"`
	RYW            rywReport                  `json:"ryw"`
	ServerBefore   map[string]json.RawMessage `json:"server_stats_before"`
	ServerStats    map[string]json.RawMessage `json:"server_stats"`
}

func buildReport(rn *runner, workers []*worker, elapsed time.Duration, before, after map[string]json.RawMessage) *report {
	cfg := rn.cfg
	merged := map[string]*opStats{}
	classes := map[string]*opStats{}
	total := newOpStats("")
	var ryw rywCounters
	for _, w := range workers {
		ryw.add(w.ryw)
		for op, s := range w.stats {
			if merged[op] == nil {
				merged[op] = newOpStats(s.class)
			}
			merged[op].merge(s)
			if classes[s.class] == nil {
				classes[s.class] = newOpStats(s.class)
			}
			classes[s.class].merge(s)
			if s.class != classVerify {
				total.merge(s)
			}
		}
	}
	rep := &report{
		Config: map[string]any{
			"bases":            cfg.Bases,
			"write_base":       cfg.WriteBase,
			"duration":         cfg.Duration.String(),
			"concurrency":      cfg.Concurrency,
			"mix":              cfg.MixSpec,
			"seed_patients":    cfg.SeedPatients,
			"read_your_writes": cfg.RYW,
			"verify_ryw":       cfg.VerifyRYW,
			"timeout":          cfg.Timeout.String(),
			"run_id":           rn.runID,
			"rand_seed":        cfg.RandSeed,
			"go":               runtime.Version(),
			"gomaxprocs":       runtime.GOMAXPROCS(0),
		},
		StartedAt:      time.Now().Add(-elapsed).UTC().Format(time.RFC3339),
		ElapsedSeconds: round2(elapsed.Seconds()),
		SeededPatients: len(rn.seeded),
		Total:          buildOpReport(total, elapsed, false),
		Classes:        map[string]opReport{},
		Ops:            map[string]opReport{},
		RYW: rywReport{
			Enabled: cfg.RYW, Verify: cfg.VerifyRYW,
			WritesWithCommitTS: ryw.WritesWithCommitTS, ReadsWithMinTS: ryw.ReadsWithMinTS,
			Checks: ryw.Checks, Violations: ryw.NotFound + ryw.Stale,
			NotFound: ryw.NotFound, Stale: ryw.Stale, SkippedNoID: ryw.SkippedNoID,
		},
		ServerBefore: before,
		ServerStats:  after,
	}
	for c, s := range classes {
		rep.Classes[c] = buildOpReport(s, elapsed, false)
	}
	for op, s := range merged {
		rep.Ops[op] = buildOpReport(s, elapsed, true)
	}
	return rep
}
