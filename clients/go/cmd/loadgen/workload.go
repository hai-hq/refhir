package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"math/rand"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

const (
	fhirJSON    = "application/fhir+json"
	hdrCommitTS = "X-Refhir-Commit-Ts" // response header on writes (may be absent)
	hdrMinTS    = "X-Refhir-Min-Ts"    // request header on reads (may be ignored)

	seedFamilies = 10 // distinct family names across seeded patients
)

// Operation names, as they appear in reports.
const (
	opRead      = "read_patient"
	opSearchFam = "search_patient_family"
	opSearchObs = "search_observation_subject"
	opPostPat   = "write_post_patient"
	opPostObs   = "write_post_observation"
	opPutPat    = "write_put_patient"
	opVerifyRYW = "ryw_verify_get"
	writeCycle  = 5 // POST Patient, POST Obs, POST Patient, POST Obs, PUT
)

var opClass = map[string]string{
	opRead:      classRead,
	opSearchFam: classSearch,
	opSearchObs: classSearch,
	opPostPat:   classWrite,
	opPostObs:   classWrite,
	opPutPat:    classWrite,
	opVerifyRYW: classVerify,
}

type config struct {
	Bases        []string
	WriteBase    string
	Duration     time.Duration
	Concurrency  int
	MixSpec      string
	Mix          mix
	SeedPatients int
	Token        string
	Out          string
	RYW          bool
	VerifyRYW    bool
	Timeout      time.Duration
	PrintJSON    bool
	Progress     time.Duration
	RandSeed     int64
}

type seedPatient struct {
	ID     string
	Family string
}

type response struct {
	status int
	hdr    http.Header
	body   []byte
	dur    time.Duration
	err    error
}

func (r response) ok() bool { return r.err == nil && r.status >= 200 && r.status <= 299 }

type runner struct {
	cfg    *config
	client *http.Client
	runID  string
	seeded []seedPatient
	rr     atomic.Uint64

	totalOps  atomic.Int64 // progress counters (mix classes only)
	totalErrs atomic.Int64
}

func newClient(cfg *config) *http.Client {
	conc := cfg.Concurrency
	tr := &http.Transport{
		Proxy:                 nil,
		MaxIdleConns:          conc * (len(cfg.Bases) + 1),
		MaxIdleConnsPerHost:   conc,
		IdleConnTimeout:       90 * time.Second,
		ResponseHeaderTimeout: cfg.Timeout,
		ExpectContinueTimeout: time.Second,
		DisableCompression:    true,
	}
	return &http.Client{Transport: tr, Timeout: cfg.Timeout}
}

// nextBase round-robins across all bases (used for reads, searches, verifies).
func (rn *runner) nextBase() string {
	i := rn.rr.Add(1) - 1
	return rn.cfg.Bases[i%uint64(len(rn.cfg.Bases))]
}

func (rn *runner) do(ctx context.Context, method, base, path string, body []byte, minTS string) response {
	var rd io.Reader
	if body != nil {
		rd = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, base+path, rd)
	if err != nil {
		return response{err: err}
	}
	req.Header.Set("Accept", fhirJSON)
	if body != nil {
		req.Header.Set("Content-Type", fhirJSON)
	}
	if rn.cfg.Token != "" {
		req.Header.Set("Authorization", "Bearer "+rn.cfg.Token)
	}
	if minTS != "" {
		req.Header.Set(hdrMinTS, minTS)
	}
	start := time.Now()
	resp, err := rn.client.Do(req)
	if err != nil {
		return response{dur: time.Since(start), err: err}
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(io.LimitReader(resp.Body, 16<<20))
	dur := time.Since(start)
	if err != nil {
		return response{status: resp.StatusCode, hdr: resp.Header, dur: dur, err: err}
	}
	return response{status: resp.StatusCode, hdr: resp.Header, body: b, dur: dur}
}

// ---------------------------------------------------------------------------
// Resource builders

func seedFamily(runID string, n int) string { return fmt.Sprintf("Lgs%s%d", runID, n%seedFamilies) }

func patientBody(id, family, given string) []byte {
	m := map[string]any{
		"resourceType": "Patient",
		"active":       true,
		"name":         []any{map[string]any{"family": family, "given": []string{given}}},
	}
	if id != "" {
		m["id"] = id
	}
	b, _ := json.Marshal(m)
	return b
}

func observationBody(patientID string, value int) []byte {
	b, _ := json.Marshal(map[string]any{
		"resourceType":      "Observation",
		"status":            "final",
		"code":              map[string]any{"coding": []any{map[string]any{"system": "http://loinc.org", "code": "8480-6"}}},
		"subject":           map[string]any{"reference": "Patient/" + patientID},
		"effectiveDateTime": "2020-01-02",
		"valueQuantity":     map[string]any{"value": value, "unit": "mmHg"},
	})
	return b
}

// extractID reads the assigned id from the response body, falling back to the
// Location header (".../Patient/<id>/_history/<v>").
func extractID(resourceType string, r response) string {
	var v struct {
		ID string `json:"id"`
	}
	if json.Unmarshal(r.body, &v) == nil && v.ID != "" {
		return v.ID
	}
	if r.hdr == nil {
		return ""
	}
	parts := strings.Split(r.hdr.Get("Location"), "/")
	for i, p := range parts {
		if p == resourceType && i+1 < len(parts) {
			return parts[i+1]
		}
	}
	return ""
}

func givenMarker(body []byte) string {
	var v struct {
		Name []struct {
			Given []string `json:"given"`
		} `json:"name"`
	}
	if json.Unmarshal(body, &v) != nil || len(v.Name) == 0 || len(v.Name[0].Given) == 0 {
		return ""
	}
	return v.Name[0].Given[0]
}

// ---------------------------------------------------------------------------
// Seeding

func (rn *runner) seed(ctx context.Context, logf func(string, ...any)) error {
	n := rn.cfg.SeedPatients
	if n == 0 {
		return nil
	}
	out := make([]seedPatient, n)
	okFlags := make([]bool, n)
	var firstErr atomic.Value
	var next atomic.Int64
	var wg sync.WaitGroup
	workers := min(rn.cfg.Concurrency, 32)
	for range workers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for ctx.Err() == nil {
				i := int(next.Add(1) - 1)
				if i >= n {
					return
				}
				fam := seedFamily(rn.runID, i)
				r := rn.do(ctx, http.MethodPost, rn.cfg.WriteBase, "/Patient", patientBody("", fam, "seed"), "")
				if !r.ok() {
					var msg string
					if r.err != nil {
						msg = r.err.Error()
					} else {
						msg = fmt.Sprintf("HTTP %d: %s", r.status, snippet(r.body, 200))
					}
					firstErr.CompareAndSwap(nil, msg)
					continue
				}
				id := extractID("Patient", r)
				if id == "" {
					firstErr.CompareAndSwap(nil, "POST /Patient response carried no id and no Location header")
					continue
				}
				out[i] = seedPatient{ID: id, Family: fam}
				okFlags[i] = true
			}
		}()
	}
	wg.Wait()
	if err := ctx.Err(); err != nil {
		return err
	}
	for i, ok := range okFlags {
		if ok {
			rn.seeded = append(rn.seeded, out[i])
		}
	}
	if len(rn.seeded) == 0 {
		return fmt.Errorf("seeding failed, no patients created: %v", firstErr.Load())
	}
	if len(rn.seeded) < n {
		logf("warning: seeded %d of %d patients (first error: %v)", len(rn.seeded), n, firstErr.Load())
	}
	return nil
}

// ---------------------------------------------------------------------------
// Worker

type rywCounters struct {
	WritesWithCommitTS int // write responses that carried X-Refhir-Commit-Ts
	ReadsWithMinTS     int // GETs that were sent X-Refhir-Min-Ts
	Checks             int // verify GETs completed (any HTTP status)
	NotFound           int // verify GET answered 404
	Stale              int // verify GET answered 2xx with an older version
	SkippedNoID        int // write response had no id to verify
}

func (c *rywCounters) add(o rywCounters) {
	c.WritesWithCommitTS += o.WritesWithCommitTS
	c.ReadsWithMinTS += o.ReadsWithMinTS
	c.Checks += o.Checks
	c.NotFound += o.NotFound
	c.Stale += o.Stale
	c.SkippedNoID += o.SkippedNoID
}

type worker struct {
	id     int
	rn     *runner
	rng    *rand.Rand
	stats  map[string]*opStats
	ryw    rywCounters
	lastTS string // newest commit ts this worker has observed
	nWrite int
	owned  []int // indices into rn.seeded this worker alone may PUT
}

func newWorker(rn *runner, id int) *worker {
	w := &worker{
		id:    id,
		rn:    rn,
		rng:   rand.New(rand.NewSource(rn.cfg.RandSeed + int64(id)*7919)),
		stats: map[string]*opStats{},
	}
	// Exclusive PUT ownership lets --verify-ryw tell "stale" from "someone else won".
	for i := id; i < len(rn.seeded); i += rn.cfg.Concurrency {
		w.owned = append(w.owned, i)
	}
	return w
}

// run loops until the deadline (or ctx cancellation); in-flight requests finish.
func (w *worker) run(ctx context.Context, deadline time.Time) {
	for ctx.Err() == nil && time.Now().Before(deadline) {
		switch w.rn.cfg.Mix.pick(w.rng) {
		case classRead:
			w.read(ctx)
		case classSearch:
			w.search(ctx)
		default:
			w.write(ctx)
		}
	}
}

func (w *worker) rec(ctx context.Context, op string, r response) bool {
	if r.err != nil && ctx.Err() != nil {
		return false // interrupted by shutdown, not a server problem
	}
	s := w.stats[op]
	if s == nil {
		s = newOpStats(opClass[op])
		w.stats[op] = s
	}
	s.record(r)
	if opClass[op] != classVerify {
		w.rn.totalOps.Add(1)
		if !r.ok() {
			w.rn.totalErrs.Add(1)
		}
	}
	return true
}

// readTS is the Min-Ts to attach to the next read (only with --read-your-writes).
func (w *worker) readTS() string {
	if w.rn.cfg.RYW && w.lastTS != "" {
		w.ryw.ReadsWithMinTS++
		return w.lastTS
	}
	return ""
}

func (w *worker) noteTS(ts string) {
	if ts == "" {
		return
	}
	w.ryw.WritesWithCommitTS++
	cur, e1 := strconv.ParseUint(w.lastTS, 10, 64)
	nxt, e2 := strconv.ParseUint(ts, 10, 64)
	if e1 == nil && e2 == nil && nxt < cur {
		return
	}
	w.lastTS = ts
}

func (w *worker) randomSeeded() seedPatient {
	return w.rn.seeded[w.rng.Intn(len(w.rn.seeded))]
}

func (w *worker) read(ctx context.Context) {
	p := w.randomSeeded()
	ts := w.readTS()
	r := w.rn.do(ctx, http.MethodGet, w.rn.nextBase(), "/Patient/"+url.PathEscape(p.ID), nil, ts)
	w.rec(ctx, opRead, r)
}

func (w *worker) search(ctx context.Context) {
	ts := w.readTS()
	if w.rng.Intn(2) == 0 {
		fam := seedFamily(w.rn.runID, w.rng.Intn(min(seedFamilies, len(w.rn.seeded))))
		path := "/Patient?family=" + url.QueryEscape(fam) + "&_count=10"
		w.rec(ctx, opSearchFam, w.rn.do(ctx, http.MethodGet, w.rn.nextBase(), path, nil, ts))
		return
	}
	p := w.randomSeeded()
	path := "/Observation?subject=Patient/" + url.PathEscape(p.ID) + "&_count=10"
	w.rec(ctx, opSearchObs, w.rn.do(ctx, http.MethodGet, w.rn.nextBase(), path, nil, ts))
}

func (w *worker) write(ctx context.Context) {
	slot := w.nWrite % writeCycle
	w.nWrite++
	switch {
	case slot == 4 && len(w.owned) > 0:
		w.putPatient(ctx)
	case (slot == 1 || slot == 3) && len(w.rn.seeded) > 0:
		w.postObservation(ctx)
	default:
		w.postPatient(ctx)
	}
}

func (w *worker) postPatient(ctx context.Context) {
	fam := fmt.Sprintf("Lgw%sw%dn%d", w.rn.runID, w.id, w.nWrite)
	r := w.rn.do(ctx, http.MethodPost, w.rn.cfg.WriteBase, "/Patient", patientBody("", fam, "gen"), "")
	if !w.rec(ctx, opPostPat, r) || !r.ok() {
		return
	}
	ts := r.hdr.Get(hdrCommitTS)
	w.noteTS(ts)
	w.verify(ctx, "Patient", extractID("Patient", r), ts, "")
}

func (w *worker) postObservation(ctx context.Context) {
	p := w.randomSeeded()
	r := w.rn.do(ctx, http.MethodPost, w.rn.cfg.WriteBase, "/Observation", observationBody(p.ID, 80+w.rng.Intn(80)), "")
	if !w.rec(ctx, opPostObs, r) || !r.ok() {
		return
	}
	ts := r.hdr.Get(hdrCommitTS)
	w.noteTS(ts)
	w.verify(ctx, "Observation", extractID("Observation", r), ts, "")
}

func (w *worker) putPatient(ctx context.Context) {
	p := w.rn.seeded[w.owned[w.rng.Intn(len(w.owned))]]
	marker := fmt.Sprintf("upd%sw%dn%d", w.rn.runID, w.id, w.nWrite)
	r := w.rn.do(ctx, http.MethodPut, w.rn.cfg.WriteBase, "/Patient/"+url.PathEscape(p.ID), patientBody(p.ID, p.Family, marker), "")
	if !w.rec(ctx, opPutPat, r) || !r.ok() {
		return
	}
	ts := r.hdr.Get(hdrCommitTS)
	w.noteTS(ts)
	w.verify(ctx, "Patient", p.ID, ts, marker)
}

// verify immediately re-reads a just-written resource from the next base in the
// round-robin, sending the write's commit ts (if the server issued one).
func (w *worker) verify(ctx context.Context, resourceType, id, ts, wantGiven string) {
	if !w.rn.cfg.VerifyRYW {
		return
	}
	if id == "" {
		w.ryw.SkippedNoID++
		return
	}
	if ts != "" {
		w.ryw.ReadsWithMinTS++
	}
	r := w.rn.do(ctx, http.MethodGet, w.rn.nextBase(), "/"+resourceType+"/"+url.PathEscape(id), nil, ts)
	if !w.rec(ctx, opVerifyRYW, r) || r.err != nil {
		return
	}
	w.ryw.Checks++
	switch {
	case r.status == http.StatusNotFound:
		w.ryw.NotFound++
	case r.ok() && wantGiven != "" && givenMarker(r.body) != wantGiven:
		w.ryw.Stale++
	}
}
