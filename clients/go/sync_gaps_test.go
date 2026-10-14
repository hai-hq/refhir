package refhir

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"
)

// scriptedSync is a /sync server that does what a test tells it, to reach the moments a real
// server only reaches by a race: a push that lands before the answer to a change, and an upgrade
// that is refused after the session was up.
type scriptedSync struct {
	*httptest.Server

	mu       sync.Mutex
	attempts []time.Time // when each upgrade was asked for
	bases    []uint64    // the baseVersion of each ModifyQuerySet
	// refuse says how to turn away upgrade n (counted from 1): a status, and a Retry-After value.
	// A status of zero accepts it.
	refuse func(n int) (int, string)
	// serve runs an accepted connection until it returns.
	serve func(conn *websocket.Conn, n int)
}

func newScriptedSync(t *testing.T) *scriptedSync {
	t.Helper()
	s := &scriptedSync{}
	mux := http.NewServeMux()
	mux.HandleFunc("/sync", func(w http.ResponseWriter, r *http.Request) {
		s.mu.Lock()
		s.attempts = append(s.attempts, time.Now())
		n := len(s.attempts)
		s.mu.Unlock()
		if s.refuse != nil {
			if status, wait := s.refuse(n); status != 0 {
				if wait != "" {
					w.Header().Set("Retry-After", wait)
				}
				http.Error(w, "refused", status)
				return
			}
		}
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer conn.CloseNow()
		s.serve(conn, n)
	})
	s.Server = httptest.NewServer(mux)
	t.Cleanup(s.Close)
	return s
}

func (s *scriptedSync) attemptCount() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.attempts)
}

// readQuerySet reads the next ModifyQuerySet, recording its base. It returns the ids it adds.
func (s *scriptedSync) readQuerySet(ctx context.Context, conn *websocket.Conn) ([]uint64, []string, bool) {
	_, data, err := conn.Read(ctx)
	if err != nil {
		return nil, nil, false
	}
	var msg struct {
		BaseVersion   uint64 `json:"baseVersion"`
		Modifications []struct {
			Type    string `json:"type"`
			QueryID uint64 `json:"queryId"`
		} `json:"modifications"`
	}
	if json.Unmarshal(data, &msg) != nil {
		return nil, nil, false
	}
	s.mu.Lock()
	s.bases = append(s.bases, msg.BaseVersion)
	s.mu.Unlock()
	ids := make([]uint64, 0, len(msg.Modifications))
	kinds := make([]string, 0, len(msg.Modifications))
	for _, mod := range msg.Modifications {
		ids = append(ids, mod.QueryID)
		kinds = append(kinds, mod.Type)
	}
	return ids, kinds, true
}

// transition builds a Transition holding one item for each modification.
func transition(start, end uint64, ids []uint64, kinds []string) []byte {
	mods := make([]map[string]any, 0, len(ids))
	for i, id := range ids {
		if kinds[i] == "Remove" {
			mods = append(mods, map[string]any{"type": "QueryRemoved", "queryId": id})
			continue
		}
		mods = append(mods, map[string]any{
			"type": "QueryUpdated", "queryId": id, "value": map[string]any{"query": id}, "logLines": []any{},
		})
	}
	out, _ := json.Marshal(map[string]any{
		"type": "Transition", "startVersion": start, "endVersion": end, "modifications": mods,
	})
	return out
}

func TestAnswerThatFollowsAPushIsAccepted(t *testing.T) {
	s := newScriptedSync(t)
	s.serve = func(conn *websocket.Conn, _ int) {
		ctx := context.Background()
		version := uint64(0)
		for change := 0; ; change++ {
			ids, kinds, ok := s.readQuerySet(ctx, conn)
			if !ok {
				return
			}
			if change == 1 {
				// A write landed between the client sending this change and the server reading it:
				// the push goes out first, so the answer is not at the version the change was based on.
				push := transition(version, version+1, []uint64{1}, []string{"Add"})
				version++
				if conn.Write(ctx, websocket.MessageText, push) != nil {
					return
				}
			}
			start := version
			version++
			if conn.Write(ctx, websocket.MessageText, transition(start, version, ids, kinds)) != nil {
				return
			}
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	// Three watches, one after another. Each is sent once the one before has been answered, so a
	// session that never saw the answer to the second one never sends the third.
	for i := 0; i < 3; i++ {
		watch, err := client.Watch(ctx, "Patient/search", map[string]any{})
		if err != nil {
			t.Fatalf("watch %d: %v", i+1, err)
		}
		t.Cleanup(func() { watch.Close() })
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	// The third change is based on the version after the push and the answer: 0, then 1, then 3.
	want := []uint64{0, 1, 3}
	if len(s.bases) != len(want) {
		t.Fatalf("bases = %v, want %v", s.bases, want)
	}
	for i := range want {
		if s.bases[i] != want[i] {
			t.Fatalf("bases = %v, want %v", s.bases, want)
		}
	}
}

func TestRefusedTokenEndsTheSessionInsteadOfRetrying(t *testing.T) {
	s := newScriptedSync(t)
	s.refuse = func(n int) (int, string) {
		if n == 1 {
			return 0, ""
		}
		return http.StatusUnauthorized, ""
	}
	s.serve = func(conn *websocket.Conn, _ int) {
		ctx := context.Background()
		ids, kinds, ok := s.readQuerySet(ctx, conn)
		if !ok {
			return
		}
		_ = conn.Write(ctx, websocket.MessageText, transition(0, 1, ids, kinds))
		// The token is revoked: the server ends the connection, as it does when a token stops
		// standing, and then refuses the next upgrade.
		conn.Close(websocket.StatusPolicyViolation, "the token expired or was revoked")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL, Bearer("old"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	watch, err := client.Watch(ctx, "Patient/search", map[string]any{})
	if err != nil {
		t.Fatal(err)
	}
	_, err = watch.Next(ctx)
	var refused *UpgradeError
	if !errors.As(err, &refused) || refused.Status != http.StatusUnauthorized {
		t.Fatalf("the watch ended with %v, want a 401 UpgradeError", err)
	}
	// The session is over: it says why to whoever asks next, and does not dial again.
	if _, err := client.Mutate(ctx, "Patient/create", map[string]any{}); !errors.As(err, &refused) {
		t.Fatalf("Mutate after the refusal returned %v, want the UpgradeError", err)
	}
	time.Sleep(700 * time.Millisecond)
	if got := s.attemptCount(); got != 2 {
		t.Fatalf("the client asked %d times, want 2 (one that worked, one refused)", got)
	}
}

func TestRetryAfterHoldsBackTheNextDial(t *testing.T) {
	s := newScriptedSync(t)
	s.refuse = func(n int) (int, string) {
		if n == 2 {
			return http.StatusTooManyRequests, "1"
		}
		return 0, ""
	}
	s.serve = func(conn *websocket.Conn, n int) {
		ctx := context.Background()
		for {
			ids, kinds, ok := s.readQuerySet(ctx, conn)
			if !ok {
				return
			}
			if conn.Write(ctx, websocket.MessageText, transition(0, 1, ids, kinds)) != nil {
				return
			}
			if n == 1 {
				conn.Close(websocket.StatusGoingAway, "")
				return
			}
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	watch, err := client.Watch(ctx, "Patient/search", map[string]any{})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { watch.Close() })
	// The first connection is closed; the second is turned away with Retry-After: 1; the third
	// works, and the watch is delivered again on it.
	if _, err := watch.Next(ctx); err != nil {
		t.Fatal(err)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.attempts) < 3 {
		t.Fatalf("only %d upgrades were asked for", len(s.attempts))
	}
	if gap := s.attempts[2].Sub(s.attempts[1]); gap < 900*time.Millisecond {
		t.Fatalf("the dial after a 429 with Retry-After: 1 came %v later", gap)
	}
}

func TestRetryAfterParsing(t *testing.T) {
	for _, tc := range []struct {
		value string
		want  time.Duration
	}{
		{"", 0},
		{"3", 3 * time.Second},
		{"  5 ", 5 * time.Second},
		{"-2", 0},
		{"soon", 0},
		{"100000", maxRetryAfter},
	} {
		if got := retryAfter(tc.value); got != tc.want {
			t.Errorf("retryAfter(%q) = %v, want %v", tc.value, got, tc.want)
		}
	}
	when := time.Now().Add(20 * time.Second).UTC().Format(http.TimeFormat)
	if got := retryAfter(when); got < 15*time.Second || got > 21*time.Second {
		t.Errorf("retryAfter(date 20s ahead) = %v", got)
	}
	if got := retryAfter(time.Now().Add(-time.Hour).UTC().Format(http.TimeFormat)); got != 0 {
		t.Errorf("retryAfter(date in the past) = %v, want 0", got)
	}
}
