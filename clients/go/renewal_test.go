package refhir

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"
)

// counterSync is a /sync server over one number. A live query answers with it, a Mutation adds one
// to it and tells every socket that has a query, and a test can end every socket at will. It is
// the smallest server on which "the watch sees a write made after the redial" can be asserted.
type counterSync struct {
	*scriptedSync

	mu        sync.Mutex
	value     int
	conns     map[*websocket.Conn]*counterConn
	adds      []uint64 // the id of every Add received, over all sockets
	mutations int      // Mutations received
	refused   bool     // a 401 has been answered
}

type counterConn struct {
	version uint64
	live    map[uint64]bool
}

func newCounterSync(t *testing.T) *counterSync {
	t.Helper()
	c := &counterSync{scriptedSync: newScriptedSync(t), conns: map[*websocket.Conn]*counterConn{}}
	c.serve = c.handle
	return c
}

// expire makes every token but "B" stop standing, as a gateway's tokens do, and ends the sockets
// that were opened with the old one.
func (c *counterSync) expire() {
	c.scriptedSync.mu.Lock()
	c.authorize = func(token string) bool {
		if token == "B" {
			return true
		}
		c.mu.Lock()
		c.refused = true
		c.mu.Unlock()
		return false
	}
	c.scriptedSync.mu.Unlock()
	c.dropAll()
}

func (c *counterSync) dropAll() {
	c.mu.Lock()
	conns := make([]*websocket.Conn, 0, len(c.conns))
	for conn := range c.conns {
		conns = append(conns, conn)
	}
	c.mu.Unlock()
	for _, conn := range conns {
		conn.Close(websocket.StatusGoingAway, "the token expired or was revoked")
	}
}

func (c *counterSync) sawRefusal() bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.refused
}

func (c *counterSync) counts() (adds []uint64, mutations int) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]uint64(nil), c.adds...), c.mutations
}

func (c *counterSync) liveQueries() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	total := 0
	for _, conn := range c.conns {
		total += len(conn.live)
	}
	return total
}

func (c *counterSync) handle(conn *websocket.Conn, _ int) {
	ctx := context.Background()
	state := &counterConn{live: map[uint64]bool{}}
	c.mu.Lock()
	c.conns[conn] = state
	c.mu.Unlock()
	defer func() {
		c.mu.Lock()
		delete(c.conns, conn)
		c.mu.Unlock()
	}()
	for {
		_, data, err := conn.Read(ctx)
		if err != nil {
			return
		}
		var msg struct {
			Type          string `json:"type"`
			RequestID     uint64 `json:"requestId"`
			Modifications []struct {
				Type    string `json:"type"`
				QueryID uint64 `json:"queryId"`
			} `json:"modifications"`
		}
		if json.Unmarshal(data, &msg) != nil {
			return
		}
		switch msg.Type {
		case "ModifyQuerySet":
			c.mu.Lock()
			items := make([]map[string]any, 0, len(msg.Modifications))
			for _, mod := range msg.Modifications {
				if mod.Type == "Remove" {
					delete(state.live, mod.QueryID)
					items = append(items, map[string]any{"type": "QueryRemoved", "queryId": mod.QueryID})
					continue
				}
				state.live[mod.QueryID] = true
				c.adds = append(c.adds, mod.QueryID)
				items = append(items, map[string]any{
					"type": "QueryUpdated", "queryId": mod.QueryID,
					"value": map[string]any{"n": c.value}, "logLines": []any{},
				})
			}
			start := state.version
			state.version++
			frame := map[string]any{
				"type": "Transition", "startVersion": start, "endVersion": state.version, "modifications": items,
			}
			c.mu.Unlock()
			if writeFrame(ctx, conn, frame) != nil {
				return
			}
		case "Mutation":
			c.mu.Lock()
			c.value++
			c.mutations++
			value := c.value
			c.mu.Unlock()
			reply := map[string]any{
				"type": "MutationResponse", "requestId": msg.RequestID, "success": true,
				"result": map[string]any{"n": value}, "ts": "1",
			}
			if writeFrame(ctx, conn, reply) != nil {
				return
			}
			c.push()
		}
	}
}

// push tells every socket that holds queries what the number is now.
func (c *counterSync) push() {
	type target struct {
		conn  *websocket.Conn
		frame map[string]any
	}
	var targets []target
	c.mu.Lock()
	for conn, state := range c.conns {
		if len(state.live) == 0 {
			continue
		}
		items := make([]map[string]any, 0, len(state.live))
		for id := range state.live {
			items = append(items, map[string]any{
				"type": "QueryUpdated", "queryId": id, "value": map[string]any{"n": c.value}, "logLines": []any{},
			})
		}
		start := state.version
		state.version++
		targets = append(targets, target{conn, map[string]any{
			"type": "Transition", "startVersion": start, "endVersion": state.version, "modifications": items,
		}})
	}
	c.mu.Unlock()
	for _, t := range targets {
		_ = writeFrame(context.Background(), t.conn, t.frame)
	}
}

func writeFrame(ctx context.Context, conn *websocket.Conn, frame map[string]any) error {
	data, err := json.Marshal(frame)
	if err != nil {
		return err
	}
	return conn.Write(ctx, websocket.MessageText, data)
}

// until polls cond for up to five seconds.
func until(t *testing.T, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if cond() {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for %s", what)
}

// nextValue is the "n" of the next result a watch delivers.
func nextValue(t *testing.T, ctx context.Context, watch *Watch) int {
	t.Helper()
	raw, err := watch.Next(ctx)
	if err != nil {
		t.Fatal(err)
	}
	var value struct {
		N int `json:"n"`
	}
	if err := json.Unmarshal(raw, &value); err != nil {
		t.Fatal(err)
	}
	return value.N
}

func TestTokenSourceRenewsTheSessionAcrossARedial(t *testing.T) {
	s := newCounterSync(t)
	s.authorize = func(token string) bool { return token == "A" }
	// The gateway hands out A until the server has refused it, then B, as a cached token does.
	var asked atomic.Int32
	source := func(context.Context) (string, error) {
		asked.Add(1)
		if s.sawRefusal() {
			return "B", nil
		}
		return "A", nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL, WithTokenSource(source))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	watch, err := client.Watch(ctx, "Thing/read", map[string]any{"id": "1"})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { watch.Close() })

	// The token runs out: the server drops the socket and refuses A from now on.
	s.expire()
	until(t, "the session to dial again with B", func() bool {
		tokens := s.tokenList()
		return len(tokens) >= 3 && tokens[len(tokens)-1] == "B" && client.Connected()
	})
	tokens := s.tokenList()
	// A opened the session; the redial tried A again, was refused, asked the source once more and
	// dialled with B.
	if len(tokens) != 3 || tokens[0] != "A" || tokens[1] != "A" || tokens[2] != "B" {
		t.Fatalf("upgrades carried %v, want [A A B]", tokens)
	}

	// The watch was resubscribed under the same id: it is delivered again, and a write made after
	// the redial reaches it.
	if n := nextValue(t, ctx, watch); n != 0 {
		t.Fatalf("after the redial the watch saw %d, want 0", n)
	}
	result, err := client.Mutate(ctx, "Thing/update", map[string]any{"id": "1"})
	if err != nil {
		t.Fatalf("a mutation after the redial failed: %v", err)
	}
	if string(result) != `{"n":1}` {
		t.Fatalf("mutation result %s", result)
	}
	if n := nextValue(t, ctx, watch); n != 1 {
		t.Fatalf("the watch saw %d after the write, want 1", n)
	}
	if adds, _ := s.counts(); len(adds) != 2 || adds[0] != 1 || adds[1] != 1 {
		t.Fatalf("Adds received: %v, want the one query added twice under id 1", adds)
	}
}

func TestStaticBearerEndsWithTheRefusal(t *testing.T) {
	s := newCounterSync(t)
	s.authorize = func(token string) bool { return token == "A" }
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL, Bearer("A"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	watch, err := client.Watch(ctx, "Thing/read", map[string]any{"id": "1"})
	if err != nil {
		t.Fatal(err)
	}
	s.expire()
	_, err = watch.Next(ctx)
	var refused *UpgradeError
	if !errors.As(err, &refused) || refused.Status != http.StatusUnauthorized {
		t.Fatalf("the watch ended with %v, want a 401 UpgradeError", err)
	}
	// A constant source is not asked again for the token it was just refused with.
	if got := s.attemptCount(); got != 2 {
		t.Fatalf("%d upgrades, want 2 (the one that worked, the one refused)", got)
	}
}

func TestSecondRefusalWithARenewedTokenIsFinal(t *testing.T) {
	s := newCounterSync(t)
	s.authorize = func(token string) bool { return token == "A" }
	var asked atomic.Int32
	source := func(context.Context) (string, error) {
		n := asked.Add(1)
		return map[int32]string{1: "A", 2: "B", 3: "C"}[n], nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL, WithTokenSource(source))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	watch, err := client.Watch(ctx, "Thing/read", map[string]any{"id": "1"})
	if err != nil {
		t.Fatal(err)
	}
	s.dropAll()
	_, err = watch.Next(ctx)
	var refused *UpgradeError
	if !errors.As(err, &refused) || refused.Status != http.StatusUnauthorized {
		t.Fatalf("the watch ended with %v, want a 401 UpgradeError", err)
	}
	// Asked for the first dial (A), the redial (B) and once more after its 401 (C): no more.
	time.Sleep(300 * time.Millisecond)
	if got := asked.Load(); got != 3 {
		t.Fatalf("the source was asked %d times, want 3", got)
	}
	if _, err := client.Mutate(ctx, "Thing/update", map[string]any{}); !errors.As(err, &refused) {
		t.Fatalf("Mutate after the refusal returned %v, want the UpgradeError", err)
	}
}

func TestForbiddenIsFinalWithoutAskingAgain(t *testing.T) {
	s := newScriptedSync(t)
	s.refuse = func(n int) (int, string) {
		if n == 1 {
			return 0, ""
		}
		return http.StatusForbidden, ""
	}
	s.serve = func(conn *websocket.Conn, _ int) {
		ids, kinds, ok := s.readQuerySet(context.Background(), conn)
		if ok {
			_ = conn.Write(context.Background(), websocket.MessageText, transition(0, 1, ids, kinds))
		}
		conn.Close(websocket.StatusGoingAway, "")
	}
	var asked atomic.Int32
	source := func(context.Context) (string, error) {
		return string(rune('A' + asked.Add(1) - 1)), nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL, WithTokenSource(source))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	watch, err := client.Watch(ctx, "Thing/read", map[string]any{})
	if err != nil {
		t.Fatal(err)
	}
	_, err = watch.Next(ctx)
	var refused *UpgradeError
	if !errors.As(err, &refused) || refused.Status != http.StatusForbidden {
		t.Fatalf("the watch ended with %v, want a 403 UpgradeError", err)
	}
	if got := asked.Load(); got != 2 {
		t.Fatalf("the source was asked %d times, want 2 (first dial, redial)", got)
	}
}

func TestTokenSourceErrorsAreRetriedNotFinal(t *testing.T) {
	s := newCounterSync(t)
	var asked atomic.Int32
	source := func(context.Context) (string, error) {
		// The first call is the dial; the next two fail, as a gateway that is briefly down does.
		if n := asked.Add(1); n == 2 || n == 3 {
			return "", errors.New("the gateway is down")
		}
		return "A", nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL, WithTokenSource(source))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	watch, err := client.Watch(ctx, "Thing/read", map[string]any{"id": "1"})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { watch.Close() })
	s.dropAll()
	if n := nextValue(t, ctx, watch); n != 0 {
		t.Fatalf("after the redial the watch saw %d, want 0", n)
	}
	if got := asked.Load(); got != 4 {
		t.Fatalf("the source was asked %d times, want 4 (dial, two failures, the redial that worked)", got)
	}
	if _, err := client.Mutate(ctx, "Thing/update", map[string]any{"id": "1"}); err != nil {
		t.Fatalf("mutation after the session recovered: %v", err)
	}
}

func TestDialReturnsTheSourcesError(t *testing.T) {
	s := newCounterSync(t)
	broken := errors.New("no token today")
	_, err := Dial(context.Background(), s.URL, WithTokenSource(func(context.Context) (string, error) {
		return "", broken
	}))
	if !errors.Is(err, broken) {
		t.Fatalf("Dial returned %v, want the source's error", err)
	}
}

func TestTokenSourceIsUsedForRESTCalls(t *testing.T) {
	var mu sync.Mutex
	var seen []string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/sync" {
			conn, err := websocket.Accept(w, r, nil)
			if err == nil {
				defer conn.CloseNow()
				conn.Read(context.Background())
			}
			return
		}
		mu.Lock()
		seen = append(seen, r.Header.Get("Authorization"))
		mu.Unlock()
		w.Write([]byte(`{}`))
	}))
	t.Cleanup(server.Close)
	var asked atomic.Int32
	source := func(context.Context) (string, error) {
		return "token-" + string(rune('0'+asked.Add(1))), nil
	}
	ctx := context.Background()
	client, err := Dial(ctx, server.URL, WithTokenSource(source))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	for i := 0; i < 2; i++ {
		if _, err := client.Metadata(ctx); err != nil {
			t.Fatal(err)
		}
	}
	mu.Lock()
	defer mu.Unlock()
	// Token 1 opened the socket; each REST call asks again.
	if len(seen) != 2 || seen[0] != "Bearer token-2" || seen[1] != "Bearer token-3" {
		t.Fatalf("REST calls carried %v", seen)
	}
}

// holdRedials turns away every upgrade after the first with 503 until released, to keep a client
// dialling for as long as a test needs.
func holdRedials(s *counterSync) (release func()) {
	var open atomic.Bool
	s.refuse = func(n int) (int, string) {
		if n > 1 && !open.Load() {
			return http.StatusServiceUnavailable, ""
		}
		return 0, ""
	}
	return func() { open.Store(true) }
}

func TestMutateWhileRedialingFailsAtOnceAndIsNeverSent(t *testing.T) {
	s := newCounterSync(t)
	release := holdRedials(s)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	watch, err := client.Watch(ctx, "Thing/read", map[string]any{"id": "1"})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { watch.Close() })

	s.dropAll()
	until(t, "the client to notice the drop", func() bool { return !client.Connected() })
	started := time.Now()
	_, err = client.Mutate(ctx, "Thing/update", map[string]any{"id": "1"})
	if !errors.Is(err, ErrNotSent) {
		t.Fatalf("Mutate while redialling returned %v, want ErrNotSent", err)
	}
	if errors.Is(err, ErrDisconnected) {
		t.Fatal("a mutation that was never sent must not look like one that is in doubt")
	}
	if took := time.Since(started); took > time.Second {
		t.Fatalf("Mutate took %v to fail", took)
	}

	release()
	until(t, "the client to be back", client.Connected)
	if n := nextValue(t, ctx, watch); n != 0 {
		t.Fatalf("watch saw %d, want 0", n)
	}
	time.Sleep(200 * time.Millisecond)
	if _, mutations := s.counts(); mutations != 0 {
		t.Fatalf("the server received %d mutations, want none: the refused one went out after the redial", mutations)
	}
	// Once the client is back, it writes again.
	if _, err := client.Mutate(ctx, "Thing/update", map[string]any{"id": "1"}); err != nil {
		t.Fatal(err)
	}
}

func TestMutationInFlightWhenTheSocketDropsIsInDoubt(t *testing.T) {
	s := newScriptedSync(t)
	received := make(chan struct{})
	s.serve = func(conn *websocket.Conn, _ int) {
		_, _, err := conn.Read(context.Background())
		if err != nil {
			return
		}
		close(received)
		conn.Close(websocket.StatusGoingAway, "")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	_, err = client.Mutate(ctx, "Thing/update", map[string]any{"id": "1"})
	<-received
	if !errors.Is(err, ErrDisconnected) || errors.Is(err, ErrNotSent) {
		t.Fatalf("a mutation lost with the socket returned %v, want ErrDisconnected", err)
	}
}

func TestWatchGivenUpWhileRedialingIsNotAddedAfterward(t *testing.T) {
	s := newCounterSync(t)
	release := holdRedials(s)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	first, err := client.Watch(ctx, "Thing/read", map[string]any{"id": "1"})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { first.Close() })

	s.dropAll()
	until(t, "the client to notice the drop", func() bool { return !client.Connected() })
	// The caller waits 100 ms for a watch and gives up. The command is queued behind the redial.
	short, stop := context.WithTimeout(ctx, 100*time.Millisecond)
	defer stop()
	if _, err := client.Watch(short, "Thing/read", map[string]any{"id": "2"}); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Watch returned %v, want the deadline", err)
	}

	release()
	until(t, "the client to be back", client.Connected)
	nextValue(t, ctx, first)
	// The session has settled when a write reaches the surviving watch.
	if _, err := client.Mutate(ctx, "Thing/update", map[string]any{"id": "1"}); err != nil {
		t.Fatal(err)
	}
	nextValue(t, ctx, first)
	if got := s.liveQueries(); got != 1 {
		t.Fatalf("the server holds %d live queries, want 1: the abandoned watch was added after the redial", got)
	}
}

func TestCommandWithAnEndedContextIsNotSent(t *testing.T) {
	s := newCounterSync(t)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })

	// A mutation whose caller has already gone, queued as it would be behind a redial: the session
	// reaches it with the socket up, and still must not send it.
	gone, stop := context.WithCancel(ctx)
	stop()
	resp := make(chan mutationResult, 1)
	client.cmds <- command{ctx: gone, kind: kindMutate, path: "Thing/update", args: map[string]any{}, resp: resp}
	select {
	case result := <-resp:
		if !errors.Is(result.err, context.Canceled) {
			t.Fatalf("the dropped mutation answered %v, want the cancellation", result.err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("the mutation was never answered")
	}
	// A later call goes through the same loop, so the first is not still on its way.
	if _, err := client.Mutate(ctx, "Thing/update", map[string]any{"id": "1"}); err != nil {
		t.Fatal(err)
	}
	if _, mutations := s.counts(); mutations != 1 {
		t.Fatalf("the server received %d mutations, want only the second", mutations)
	}
}

// A watch cancelled while its Add is still being sent used to race: Close read the watch's id while
// the session goroutine wrote it. Run it under -race. It also checks that no query is left behind
// on the server, whichever side of the Add the cancellation fell.
func TestWatchCancelledRightAfterItStartsDoesNotRace(t *testing.T) {
	s := newCounterSync(t)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	client, err := Dial(ctx, s.URL)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })

	var wg sync.WaitGroup
	for i := 0; i < 300; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			call, stop := context.WithCancel(ctx)
			go func() {
				time.Sleep(time.Duration(i%7) * 100 * time.Microsecond)
				stop()
			}()
			watch, err := client.Watch(call, "Thing/read", map[string]any{"id": "1"})
			if err != nil {
				return
			}
			watch.Close()
		}(i)
	}
	wg.Wait()
	until(t, "every query to be removed", func() bool { return s.liveQueries() == 0 })
}
