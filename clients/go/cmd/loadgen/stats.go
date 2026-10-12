package main

import (
	"fmt"
	"math"
	"math/rand"
	"sort"
	"strconv"
	"strings"
	"time"
)

// Operation classes. The --mix flag chooses between the first three; "verify"
// is the extra GET issued by --verify-ryw and is reported separately.
const (
	classRead   = "read"
	classSearch = "search"
	classWrite  = "write"
	classVerify = "verify"
)

// ---------------------------------------------------------------------------
// Mix parsing

// mix holds relative weights for each workload class. Weights are normalised
// when picking, so "read=6,search=2.5,write=1.5" works as well as percentages.
type mix struct {
	Read, Search, Write float64
}

// parseMix parses "read=60,search=25,write=15". Unspecified classes are zero.
func parseMix(s string) (mix, error) {
	var m mix
	if strings.TrimSpace(s) == "" {
		return m, fmt.Errorf("empty mix")
	}
	seen := map[string]bool{}
	for _, part := range strings.Split(s, ",") {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		key, val, ok := strings.Cut(part, "=")
		if !ok {
			return m, fmt.Errorf("mix entry %q: want name=percent", part)
		}
		key = strings.ToLower(strings.TrimSpace(key))
		if seen[key] {
			return m, fmt.Errorf("mix entry %q given twice", key)
		}
		seen[key] = true
		w, err := strconv.ParseFloat(strings.TrimSpace(val), 64)
		if err != nil {
			return m, fmt.Errorf("mix entry %q: %v", part, err)
		}
		if math.IsNaN(w) || math.IsInf(w, 0) || w < 0 {
			return m, fmt.Errorf("mix entry %q: weight must be a finite number >= 0", part)
		}
		switch key {
		case classRead:
			m.Read = w
		case classSearch:
			m.Search = w
		case classWrite:
			m.Write = w
		default:
			return m, fmt.Errorf("mix entry %q: unknown class (want read, search, write)", key)
		}
	}
	if m.total() <= 0 {
		return m, fmt.Errorf("mix weights sum to zero")
	}
	return m, nil
}

func (m mix) total() float64 { return m.Read + m.Search + m.Write }

// pick returns the class for one draw from r.
func (m mix) pick(r *rand.Rand) string {
	x := r.Float64() * m.total()
	switch {
	case x < m.Read:
		return classRead
	case x < m.Read+m.Search:
		return classSearch
	default:
		if m.Write > 0 {
			return classWrite
		}
		if m.Search > 0 {
			return classSearch
		}
		return classRead
	}
}

// ---------------------------------------------------------------------------
// Percentiles

// percentile returns the nearest-rank percentile p (0..100) of an ascending
// sorted slice. An empty slice yields 0.
func percentile(sorted []time.Duration, p float64) time.Duration {
	n := len(sorted)
	if n == 0 {
		return 0
	}
	if p <= 0 {
		return sorted[0]
	}
	if p >= 100 {
		return sorted[n-1]
	}
	// p*n first, then /100: exact for integer p, so no off-by-one from 0.07*100.
	rank := int(math.Ceil(p * float64(n) / 100))
	if rank < 1 {
		rank = 1
	}
	if rank > n {
		rank = n
	}
	return sorted[rank-1]
}

// ---------------------------------------------------------------------------
// Per-op accumulation (one set per worker, merged after the run)

const maxErrorSamples = 5

type opStats struct {
	class   string
	count   int // completed requests, successful or not
	errors  int // transport errors + non-2xx
	status  map[int]int
	durs    []time.Duration // latencies of 2xx responses only
	samples []string
}

func newOpStats(class string) *opStats {
	return &opStats{class: class, status: map[int]int{}}
}

func (s *opStats) record(r response) {
	s.count++
	if r.err != nil {
		s.errors++
		s.status[0]++
		s.addSample(r.err.Error())
		return
	}
	s.status[r.status]++
	if r.status < 200 || r.status > 299 {
		s.errors++
		s.addSample(fmt.Sprintf("HTTP %d: %s", r.status, snippet(r.body, 160)))
		return
	}
	s.durs = append(s.durs, r.dur)
}

func (s *opStats) addSample(msg string) {
	if len(s.samples) < maxErrorSamples {
		s.samples = append(s.samples, msg)
	}
}

func (s *opStats) merge(o *opStats) {
	s.count += o.count
	s.errors += o.errors
	for k, v := range o.status {
		s.status[k] += v
	}
	s.durs = append(s.durs, o.durs...)
	for _, m := range o.samples {
		s.addSample(m)
	}
}

func snippet(b []byte, max int) string {
	s := strings.Join(strings.Fields(string(b)), " ")
	if len(s) > max {
		s = s[:max] + "..."
	}
	return s
}

// ---------------------------------------------------------------------------
// Report types

type latencyReport struct {
	Min  float64 `json:"min"`
	Mean float64 `json:"mean"`
	P50  float64 `json:"p50"`
	P95  float64 `json:"p95"`
	P99  float64 `json:"p99"`
	Max  float64 `json:"max"`
}

type opReport struct {
	Class        string         `json:"class,omitempty"`
	Count        int            `json:"count"`
	Errors       int            `json:"errors"`
	OpsPerSec    float64        `json:"ops_per_s"`
	OKPerSec     float64        `json:"ok_ops_per_s"`
	Status       map[string]int `json:"status"`
	LatencyMS    latencyReport  `json:"latency_ms"` // successful (2xx) requests only
	ErrorSamples []string       `json:"error_samples,omitempty"`
}

func ms(d time.Duration) float64 {
	return math.Round(float64(d)/float64(time.Millisecond)*1000) / 1000
}

func buildOpReport(s *opStats, elapsed time.Duration, withClass bool) opReport {
	sort.Slice(s.durs, func(i, j int) bool { return s.durs[i] < s.durs[j] })
	r := opReport{
		Count:        s.count,
		Errors:       s.errors,
		Status:       map[string]int{},
		ErrorSamples: s.samples,
	}
	if withClass {
		r.Class = s.class
	}
	for code, n := range s.status {
		key := strconv.Itoa(code)
		if code == 0 {
			key = "transport_error"
		}
		r.Status[key] = n
	}
	if sec := elapsed.Seconds(); sec > 0 {
		r.OpsPerSec = round2(float64(s.count) / sec)
		r.OKPerSec = round2(float64(len(s.durs)) / sec)
	}
	if n := len(s.durs); n > 0 {
		var sum time.Duration
		for _, d := range s.durs {
			sum += d
		}
		r.LatencyMS = latencyReport{
			Min:  ms(s.durs[0]),
			Mean: ms(sum / time.Duration(n)),
			P50:  ms(percentile(s.durs, 50)),
			P95:  ms(percentile(s.durs, 95)),
			P99:  ms(percentile(s.durs, 99)),
			Max:  ms(s.durs[n-1]),
		}
	}
	return r
}

func round2(x float64) float64 { return math.Round(x*100) / 100 }
