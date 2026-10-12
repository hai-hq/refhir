package refhir

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strconv"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"
)

// fakeServer answers REST calls with a scripted X-Refhir-Commit-Ts and answers
// /sync mutations with a scripted ts. It records the X-Refhir-Min-Ts of each
// REST request.
type fakeServer struct {
	*httptest.Server

	mu       sync.Mutex
	restTs   []string // commit ts header for the next REST responses, popped in order
	mutateTs []string // ts for the next mutation responses, popped in order
	minTs    []string // X-Refhir-Min-Ts seen per REST request ("" when absent)
	present  []bool
}

func newFakeServer(t *testing.T) *fakeServer {
	t.Helper()
	fake := &fakeServer{}
	mux := http.NewServeMux()
	mux.HandleFunc("/sync", fake.serveSync)
	mux.HandleFunc("/", fake.serveREST)
	fake.Server = httptest.NewServer(mux)
	t.Cleanup(fake.Close)
	return fake
}

func (f *fakeServer) script(rest, mutate []string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.restTs = rest
	f.mutateTs = mutate
}

func (f *fakeServer) seen() ([]string, []bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string(nil), f.minTs...), append([]bool(nil), f.present...)
}

func (f *fakeServer) serveREST(w http.ResponseWriter, r *http.Request) {
	values, present := r.Header[http.CanonicalHeaderKey(headerMinTs)]
	f.mu.Lock()
	if present {
		f.minTs = append(f.minTs, values[0])
	} else {
		f.minTs = append(f.minTs, "")
	}
	f.present = append(f.present, present)
	var ts string
	if len(f.restTs) > 0 {
		ts, f.restTs = f.restTs[0], f.restTs[1:]
	}
	f.mu.Unlock()
	if ts != "" {
		w.Header().Set(headerCommitTs, ts)
	}
	w.Header().Set("Content-Type", "application/fhir+json")
	w.Write([]byte(`{}`))
}

func (f *fakeServer) serveSync(w http.ResponseWriter, r *http.Request) {
	conn, err := websocket.Accept(w, r, nil)
	if err != nil {
		return
	}
	defer conn.CloseNow()
	for {
		_, data, err := conn.Read(r.Context())
		if err != nil {
			return
		}
		var msg struct {
			Type      string `json:"type"`
			RequestID uint64 `json:"requestId"`
		}
		if json.Unmarshal(data, &msg) != nil || msg.Type != "Mutation" {
			continue
		}
		f.mu.Lock()
		var ts string
		if len(f.mutateTs) > 0 {
			ts, f.mutateTs = f.mutateTs[0], f.mutateTs[1:]
		}
		f.mu.Unlock()
		reply := map[string]any{
			"type":      "MutationResponse",
			"requestId": msg.RequestID,
			"success":   true,
			"result":    map[string]any{},
		}
		if ts != "" {
			reply["ts"] = ts
		} else {
			reply["ts"] = nil
		}
		out, _ := json.Marshal(reply)
		if conn.Write(r.Context(), websocket.MessageText, out) != nil {
			return
		}
	}
}

func dialFake(t *testing.T, fake *fakeServer, opts ...Option) *Client {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	client, err := Dial(ctx, fake.URL, opts...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	return client
}

func doGet(t *testing.T, client *Client, path string) {
	t.Helper()
	if _, err := client.Do(context.Background(), http.MethodGet, path, nil); err != nil {
		t.Fatal(err)
	}
}

func TestReadYourWritesHeaderCapturedAndSent(t *testing.T) {
	fake := newFakeServer(t)
	client := dialFake(t, fake)
	fake.script([]string{"100", "", ""}, nil)

	if got := client.LastCommitTs(); got != 0 {
		t.Fatalf("initial LastCommitTs = %d", got)
	}
	// First request: nothing known yet, so no header; response reports 100.
	body := []byte(`{"resourceType":"Patient"}`)
	if _, err := client.Do(context.Background(), http.MethodPost, "/Patient", body); err != nil {
		t.Fatal(err)
	}
	if got := client.LastCommitTs(); got != 100 {
		t.Fatalf("LastCommitTs after write = %d, want 100", got)
	}
	// Next requests carry it, including ones whose response has no header.
	doGet(t, client, "/Patient/p1")
	doGet(t, client, "Patient/p1")

	minTs, present := fake.seen()
	if len(minTs) != 3 {
		t.Fatalf("requests = %d", len(minTs))
	}
	if present[0] {
		t.Fatalf("first request sent %s=%q", headerMinTs, minTs[0])
	}
	for i := 1; i < 3; i++ {
		if minTs[i] != "100" {
			t.Fatalf("request %d %s = %q, want 100", i, headerMinTs, minTs[i])
		}
	}
}

func TestReadYourWritesHeaderKeepsMax(t *testing.T) {
	fake := newFakeServer(t)
	client := dialFake(t, fake)
	// 500, then a stale 300 (must not lower), then 700 (raises), then garbage (ignored).
	fake.script([]string{"500", "300", "700", "abc", "-1", ""}, nil)

	doGet(t, client, "/a")
	doGet(t, client, "/b")
	if got := client.LastCommitTs(); got != 500 {
		t.Fatalf("after stale ts: %d, want 500", got)
	}
	doGet(t, client, "/c")
	if got := client.LastCommitTs(); got != 700 {
		t.Fatalf("after newer ts: %d, want 700", got)
	}
	doGet(t, client, "/d")
	doGet(t, client, "/e")
	doGet(t, client, "/f")
	if got := client.LastCommitTs(); got != 700 {
		t.Fatalf("after malformed ts: %d, want 700", got)
	}

	minTs, _ := fake.seen()
	want := []string{"", "500", "500", "700", "700", "700"}
	for i := range want {
		if minTs[i] != want[i] {
			t.Fatalf("request %d %s = %q, want %q (all %v)", i, headerMinTs, minTs[i], want[i], minTs)
		}
	}
}

func TestReadYourWritesLargeU64(t *testing.T) {
	fake := newFakeServer(t)
	client := dialFake(t, fake)
	big := strconv.FormatUint(18446744073709551615, 10)
	fake.script([]string{big, ""}, nil)

	doGet(t, client, "/a")
	doGet(t, client, "/b")
	if got := client.LastCommitTs(); got != 18446744073709551615 {
		t.Fatalf("LastCommitTs = %d", got)
	}
	if minTs, _ := fake.seen(); minTs[1] != big {
		t.Fatalf("sent %q, want %q", minTs[1], big)
	}
}

func TestReadYourWritesFromSyncMutation(t *testing.T) {
	fake := newFakeServer(t)
	client := dialFake(t, fake)
	fake.script([]string{"", ""}, []string{"250", "200", ""})
	ctx := context.Background()

	if _, err := client.Create(ctx, "Patient", json.RawMessage(`{"resourceType":"Patient"}`)); err != nil {
		t.Fatal(err)
	}
	if got := client.LastCommitTs(); got != 250 {
		t.Fatalf("after mutation: %d, want 250", got)
	}
	// Smaller ts later does not lower it; a null ts changes nothing.
	if _, err := client.Delete(ctx, "Patient", "p1"); err != nil {
		t.Fatal(err)
	}
	if _, err := client.Delete(ctx, "Patient", "p2"); err != nil {
		t.Fatal(err)
	}
	if got := client.LastCommitTs(); got != 250 {
		t.Fatalf("after stale/null mutation ts: %d, want 250", got)
	}
	doGet(t, client, "/Patient/p1")
	if minTs, _ := fake.seen(); minTs[0] != "250" {
		t.Fatalf("REST after mutation sent %q, want 250", minTs[0])
	}
}

func TestReadYourWritesDisabledSendsNothing(t *testing.T) {
	fake := newFakeServer(t)
	client := dialFake(t, fake, WithReadYourWrites(false))
	fake.script([]string{"100", "200", ""}, []string{"300"})

	doGet(t, client, "/a")
	if _, err := client.Create(context.Background(), "Patient", json.RawMessage(`{"resourceType":"Patient"}`)); err != nil {
		t.Fatal(err)
	}
	doGet(t, client, "/b")
	doGet(t, client, "/c")

	_, present := fake.seen()
	for i, sent := range present {
		if sent {
			t.Fatalf("request %d sent %s with read-your-writes disabled", i, headerMinTs)
		}
	}
	// Tracking itself stays available.
	if got := client.LastCommitTs(); got != 300 {
		t.Fatalf("LastCommitTs = %d, want 300", got)
	}
}

func TestReadYourWritesParseTS(t *testing.T) {
	cases := []struct {
		in   string
		want uint64
		ok   bool
	}{
		{`"42"`, 42, true},
		{`42`, 42, true},
		{` "7" `, 7, true},
		{`null`, 0, false},
		{``, 0, false},
		{`"x"`, 0, false},
		{`"-3"`, 0, false},
		{`"18446744073709551616"`, 0, false},
	}
	for _, tc := range cases {
		got, ok := parseTS([]byte(tc.in))
		if got != tc.want || ok != tc.ok {
			t.Errorf("parseTS(%q) = %d, %v; want %d, %v", tc.in, got, ok, tc.want, tc.ok)
		}
	}
}

func TestLastCommitTsConcurrent(t *testing.T) {
	var client Client
	var wg sync.WaitGroup
	for i := uint64(1); i <= 200; i++ {
		wg.Add(1)
		go func(ts uint64) {
			defer wg.Done()
			client.observeTs(ts)
		}(i)
	}
	wg.Wait()
	if got := client.LastCommitTs(); got != 200 {
		t.Fatalf("LastCommitTs = %d, want 200", got)
	}
}
