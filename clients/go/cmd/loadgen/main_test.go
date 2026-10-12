package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"math/rand"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestPercentile(t *testing.T) {
	var s []time.Duration
	for i := 1; i <= 100; i++ {
		s = append(s, time.Duration(i)*time.Millisecond)
	}
	cases := []struct {
		p    float64
		want time.Duration
	}{
		{0, 1 * time.Millisecond},
		{1, 1 * time.Millisecond},
		{7, 7 * time.Millisecond}, // 0.07*100 is 7.000000000000001 in float64
		{50, 50 * time.Millisecond},
		{95, 95 * time.Millisecond},
		{99, 99 * time.Millisecond},
		{99.9, 100 * time.Millisecond},
		{100, 100 * time.Millisecond},
		{150, 100 * time.Millisecond},
	}
	for _, c := range cases {
		if got := percentile(s, c.p); got != c.want {
			t.Errorf("percentile(1..100ms, %v) = %v, want %v", c.p, got, c.want)
		}
	}

	ten := s[:10]
	if got := percentile(ten, 50); got != 5*time.Millisecond {
		t.Errorf("p50 of 10 = %v, want 5ms", got)
	}
	if got := percentile(ten, 99); got != 10*time.Millisecond {
		t.Errorf("p99 of 10 = %v, want 10ms", got)
	}
	if got := percentile(s[:1], 95); got != time.Millisecond {
		t.Errorf("p95 of 1 = %v, want 1ms", got)
	}
	if got := percentile(nil, 50); got != 0 {
		t.Errorf("p50 of empty = %v, want 0", got)
	}
}

func TestParseMix(t *testing.T) {
	m, err := parseMix("read=60,search=25,write=15")
	if err != nil {
		t.Fatal(err)
	}
	if m != (mix{Read: 60, Search: 25, Write: 15}) {
		t.Fatalf("got %+v", m)
	}

	m, err = parseMix(" Write = 100 ")
	if err != nil || m != (mix{Write: 100}) {
		t.Fatalf("got %+v, %v", m, err)
	}

	for _, bad := range []string{
		"", "read", "read=x", "read=-1", "read=0,write=0", "scan=10", "read=1,read=2", "read=NaN", "read=Inf",
	} {
		if _, err := parseMix(bad); err == nil {
			t.Errorf("parseMix(%q): expected error", bad)
		}
	}
}

func TestMixPickDistribution(t *testing.T) {
	m, _ := parseMix("read=60,search=25,write=15")
	r := rand.New(rand.NewSource(1))
	counts := map[string]int{}
	const n = 100000
	for range n {
		counts[m.pick(r)]++
	}
	for class, want := range map[string]float64{classRead: 0.60, classSearch: 0.25, classWrite: 0.15} {
		got := float64(counts[class]) / n
		if got < want-0.02 || got > want+0.02 {
			t.Errorf("%s share = %.3f, want about %.2f", class, got, want)
		}
	}
	only, _ := parseMix("write=1")
	for range 100 {
		if c := only.pick(r); c != classWrite {
			t.Fatalf("write-only mix picked %s", c)
		}
	}
}

func TestStringListAndBase(t *testing.T) {
	var l stringList
	_ = l.Set("http://a:1, http://b:2")
	_ = l.Set("http://c:3")
	if len(l) != 3 || l[2] != "http://c:3" {
		t.Fatalf("got %v", l)
	}
	if b, err := normalizeBase("127.0.0.1:8080/"); err != nil || b != "http://127.0.0.1:8080" {
		t.Fatalf("got %q, %v", b, err)
	}
	if _, err := normalizeBase("ftp://x"); err == nil {
		t.Fatal("expected error for ftp scheme")
	}
}

func TestExtractID(t *testing.T) {
	r := response{body: []byte(`{"resourceType":"Patient","id":"abc"}`)}
	if got := extractID("Patient", r); got != "abc" {
		t.Fatalf("body id: got %q", got)
	}
	r = response{hdr: http.Header{"Location": {"http://x/Patient/xyz/_history/1"}}}
	if got := extractID("Patient", r); got != "xyz" {
		t.Fatalf("location id: got %q", got)
	}
	if got := extractID("Patient", response{}); got != "" {
		t.Fatalf("expected empty, got %q", got)
	}
}

// ---------------------------------------------------------------------------
// End-to-end against an in-test dummy FHIR server (no real network).

type dummy struct {
	mu        sync.Mutex
	store     map[string][]byte // "Patient/<id>" -> body
	seq       int64
	ts        int64
	notFound  bool // simulate a lagging replica: every GET /{type}/{id} is 404
	minTSSeen atomic.Int64
	writes    atomic.Int64
}

func newDummy(notFound bool) (*dummy, *httptest.Server) {
	d := &dummy{store: map[string][]byte{}, notFound: notFound}
	mux := http.NewServeMux()
	mux.HandleFunc("GET /metadata", func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(`{"resourceType":"CapabilityStatement"}`))
	})
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("verbose") != "1" {
			w.Write([]byte("ok"))
			return
		}
		fmt.Fprintf(w, `{"status":"ok","writes":%d}`, d.writes.Load())
	})
	create := func(w http.ResponseWriter, r *http.Request, rt string) {
		var m map[string]any
		if err := json.NewDecoder(r.Body).Decode(&m); err != nil {
			http.Error(w, "bad json", http.StatusBadRequest)
			return
		}
		d.mu.Lock()
		d.seq++
		d.ts++
		id := fmt.Sprintf("%s-%d", strings.ToLower(rt), d.seq)
		m["id"] = id
		body, _ := json.Marshal(m)
		d.store[rt+"/"+id] = body
		ts := d.ts
		d.mu.Unlock()
		d.writes.Add(1)
		w.Header().Set(hdrCommitTS, fmt.Sprint(ts))
		w.WriteHeader(http.StatusCreated)
		w.Write(body)
	}
	mux.HandleFunc("POST /Patient", func(w http.ResponseWriter, r *http.Request) { create(w, r, "Patient") })
	mux.HandleFunc("POST /Observation", func(w http.ResponseWriter, r *http.Request) { create(w, r, "Observation") })
	mux.HandleFunc("PUT /Patient/{id}", func(w http.ResponseWriter, r *http.Request) {
		body, _ := readAll(r)
		d.mu.Lock()
		d.ts++
		d.store["Patient/"+r.PathValue("id")] = body
		ts := d.ts
		d.mu.Unlock()
		d.writes.Add(1)
		w.Header().Set(hdrCommitTS, fmt.Sprint(ts))
		w.Write(body)
	})
	get := func(rt string) http.HandlerFunc {
		return func(w http.ResponseWriter, r *http.Request) {
			if r.Header.Get(hdrMinTS) != "" {
				d.minTSSeen.Add(1)
			}
			d.mu.Lock()
			body, ok := d.store[rt+"/"+r.PathValue("id")]
			d.mu.Unlock()
			if !ok || d.notFound {
				http.Error(w, `{"resourceType":"OperationOutcome"}`, http.StatusNotFound)
				return
			}
			w.Write(body)
		}
	}
	mux.HandleFunc("GET /Patient/{id}", get("Patient"))
	mux.HandleFunc("GET /Observation/{id}", get("Observation"))
	bundle := func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get(hdrMinTS) != "" {
			d.minTSSeen.Add(1)
		}
		w.Write([]byte(`{"resourceType":"Bundle","type":"searchset","total":0}`))
	}
	mux.HandleFunc("GET /Patient", bundle)
	mux.HandleFunc("GET /Observation", bundle)
	return d, httptest.NewServer(mux)
}

func readAll(r *http.Request) ([]byte, error) {
	var b bytes.Buffer
	_, err := b.ReadFrom(r.Body)
	return b.Bytes(), err
}

func runLoadgen(t *testing.T, args ...string) (*report, string) {
	t.Helper()
	outPath := filepath.Join(t.TempDir(), "out.json")
	args = append(args, "--out", outPath, "--json=false", "--progress", "0", "--rand-seed", "7")
	var stdout, stderr bytes.Buffer
	if code := run(context.Background(), args, &stdout, &stderr); code != 0 {
		t.Fatalf("run exited %d\nstdout:\n%s\nstderr:\n%s", code, stdout.String(), stderr.String())
	}
	data, err := os.ReadFile(outPath)
	if err != nil {
		t.Fatal(err)
	}
	var rep report
	if err := json.Unmarshal(data, &rep); err != nil {
		t.Fatalf("decode %s: %v", outPath, err)
	}
	return &rep, stdout.String()
}

func TestEndToEndDummy(t *testing.T) {
	d, srv := newDummy(false)
	defer srv.Close()

	rep, summary := runLoadgen(t,
		"--base", srv.URL, "--duration", "400ms", "--concurrency", "4",
		"--mix", "read=40,search=30,write=30", "--seed-patients", "20",
		"--read-your-writes", "--verify-ryw")

	if rep.SeededPatients != 20 {
		t.Errorf("seeded = %d, want 20", rep.SeededPatients)
	}
	if rep.Total.Count == 0 || rep.Total.Errors != 0 {
		t.Fatalf("total = %+v", rep.Total)
	}
	for _, op := range []string{opRead, opSearchFam, opSearchObs, opPostPat, opPostObs, opPutPat, opVerifyRYW} {
		o, ok := rep.Ops[op]
		if !ok || o.Count == 0 {
			t.Errorf("op %s missing or empty: %+v", op, o)
			continue
		}
		if o.Errors != 0 || o.Status["200"]+o.Status["201"] != o.Count {
			t.Errorf("op %s: unexpected statuses %+v", op, o)
		}
		if o.LatencyMS.P50 > o.LatencyMS.P95 || o.LatencyMS.P95 > o.LatencyMS.P99 || o.LatencyMS.P99 > o.LatencyMS.Max {
			t.Errorf("op %s: latencies not monotonic: %+v", op, o.LatencyMS)
		}
	}
	if _, ok := rep.Classes[classVerify]; !ok {
		t.Error("verify class missing")
	}
	if rep.RYW.Checks == 0 || rep.RYW.Violations != 0 || rep.RYW.WritesWithCommitTS == 0 {
		t.Errorf("ryw = %+v", rep.RYW)
	}
	if d.minTSSeen.Load() == 0 || rep.RYW.ReadsWithMinTS == 0 {
		t.Errorf("Min-Ts never sent: server saw %d, report %d", d.minTSSeen.Load(), rep.RYW.ReadsWithMinTS)
	}
	var stats struct {
		Status string `json:"status"`
		Writes int    `json:"writes"`
	}
	if raw, ok := rep.ServerStats[srv.URL]; !ok || json.Unmarshal(raw, &stats) != nil || stats.Status != "ok" || stats.Writes == 0 {
		t.Errorf("server_stats = %s", rep.ServerStats[srv.URL])
	}
	if _, ok := rep.ServerBefore[srv.URL]; !ok {
		t.Error("server_stats_before missing")
	}
	t.Log("\n" + summary)
	if !strings.Contains(summary, "TOTAL (mix)") || !strings.Contains(summary, opPostObs) {
		t.Errorf("summary missing rows:\n%s", summary)
	}
}

func TestVerifyRYWDetectsLaggingReplica(t *testing.T) {
	_, primary := newDummy(false)
	defer primary.Close()
	_, replica := newDummy(true) // 404s every resource read
	defer replica.Close()

	rep, _ := runLoadgen(t,
		"--base", primary.URL+","+replica.URL, "--write-base", primary.URL,
		"--duration", "300ms", "--concurrency", "2",
		"--mix", "write=100", "--seed-patients", "5", "--verify-ryw")

	if rep.RYW.Checks == 0 {
		t.Fatalf("no verify checks: %+v", rep.RYW)
	}
	if rep.RYW.NotFound == 0 || rep.RYW.NotFound >= rep.RYW.Checks {
		t.Errorf("expected some but not all verifies to 404 (round-robin over 2 bases): %+v", rep.RYW)
	}
	if rep.RYW.Violations != rep.RYW.NotFound+rep.RYW.Stale {
		t.Errorf("violations inconsistent: %+v", rep.RYW)
	}
	if len(rep.ServerStats) != 2 {
		t.Errorf("want stats from both bases, got %d", len(rep.ServerStats))
	}
}

func TestRejectsBadFlags(t *testing.T) {
	for _, args := range [][]string{
		{"--mix", "bogus"},
		{"--concurrency", "0"},
		{"--seed-patients", "0", "--mix", "read=1"},
		{"--base", "ftp://x"},
	} {
		var out, errb bytes.Buffer
		if code := run(context.Background(), args, &out, &errb); code != 2 {
			t.Errorf("%v: exit %d, want 2 (stderr: %s)", args, code, errb.String())
		}
	}
}
