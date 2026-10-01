package refhir

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"sort"
	"strings"
	"time"

	"github.com/coder/websocket"
)

var (
	errClosed       = errors.New("refhir: client closed")
	errDisconnected = errors.New("refhir: disconnected")
)

// OutcomeError is a failed mutation. Outcome is the OperationOutcome JSON.
type OutcomeError struct {
	Outcome json.RawMessage
}

func (e *OutcomeError) Error() string {
	return "refhir: " + string(e.Outcome)
}

// Option configures Dial.
type Option func(*dialOptions)

type dialOptions struct {
	token string
}

// Bearer sets the Authorization header on the /sync upgrade and on HTTP calls.
func Bearer(token string) Option {
	return func(o *dialOptions) {
		o.token = token
	}
}

// Client is one /sync session. Watch and Mutate share that socket.
type Client struct {
	httpBase string
	wsURL    string
	token    string
	http     *http.Client

	root   context.Context
	cancel context.CancelFunc

	cmds    chan command
	events  chan sessionEvent
	stopped chan struct{}

	// Session state is touched only by the actor goroutine.
	conn      *websocket.Conn
	gen       int
	version   uint64
	nextQuery uint64
	nextReq   uint64
	subs      map[uint64]*subscription
	queue     []modification
	flight    *flight
	pending   map[uint64]chan mutationResult
	closing   bool
}

type subscription struct {
	id    uint64
	path  string
	args  json.RawMessage
	watch *Watch
	sent  bool
}

type flight struct {
	base uint64
	want map[uint64]string
}

type mutationResult struct {
	value json.RawMessage
	err   error
}

type command struct {
	kind  string
	path  string
	args  map[string]any
	id    uint64
	watch *Watch
	resp  chan mutationResult
	errc  chan error
}

const (
	kindAdd    = "add"
	kindRemove = "remove"
	kindMutate = "mutate"
	kindSever  = "sever"
)

type sessionEvent struct {
	gen  int
	data []byte
	down bool
}

// Dial connects to a ReFHIR server. baseURL is the HTTP origin, such as http://127.0.0.1:8080.
func Dial(ctx context.Context, baseURL string, opts ...Option) (*Client, error) {
	var options dialOptions
	for _, opt := range opts {
		opt(&options)
	}
	httpBase, wsURL, err := splitBase(baseURL)
	if err != nil {
		return nil, err
	}
	conn, err := dialSocket(ctx, wsURL, options.token)
	if err != nil {
		return nil, err
	}
	root, cancel := context.WithCancel(context.Background())
	client := &Client{
		httpBase:  httpBase,
		wsURL:     wsURL,
		token:     options.token,
		http:      &http.Client{},
		root:      root,
		cancel:    cancel,
		cmds:      make(chan command, 64),
		events:    make(chan sessionEvent, 64),
		stopped:   make(chan struct{}),
		conn:      conn,
		gen:       1,
		nextQuery: 1,
		nextReq:   1,
		subs:      make(map[uint64]*subscription),
		pending:   make(map[uint64]chan mutationResult),
	}
	go client.read(conn, 1)
	go client.loop()
	return client, nil
}

// Close shuts down the session.
func (c *Client) Close() error {
	c.cancel()
	<-c.stopped
	return nil
}

// Mutate runs a write operation and waits for MutationResponse.
// A failed mutation returns *OutcomeError. Mutations are not retried after a disconnect.
func (c *Client) Mutate(ctx context.Context, path string, args map[string]any) (json.RawMessage, error) {
	resp := make(chan mutationResult, 1)
	select {
	case c.cmds <- command{kind: kindMutate, path: path, args: args, resp: resp}:
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-c.root.Done():
		return nil, errClosed
	}
	select {
	case result := <-resp:
		return result.value, result.err
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-c.root.Done():
		return nil, errClosed
	}
}

// Watch subscribes to a read-only operation and waits for the first result.
func (c *Client) Watch(ctx context.Context, path string, args map[string]any) (*Watch, error) {
	watch := newWatch(c)
	select {
	case c.cmds <- command{kind: kindAdd, path: path, args: args, watch: watch}:
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-c.root.Done():
		return nil, errClosed
	}
	if _, err := watch.waitAfter(ctx, 0); err != nil {
		watch.Close()
		return nil, err
	}
	return watch, nil
}

// sever drops the socket so the session dials again. Tests use it.
func (c *Client) sever() {
	select {
	case c.cmds <- command{kind: kindSever}:
	case <-c.root.Done():
	}
}

func (c *Client) loop() {
	defer c.finish()
	for {
		select {
		case <-c.root.Done():
			return
		case ev := <-c.events:
			if ev.gen != c.gen {
				continue
			}
			if ev.down {
				c.onDown()
				if c.root.Err() != nil {
					return
				}
				continue
			}
			c.onMessage(ev.data)
		case cmd := <-c.cmds:
			c.onCommand(cmd)
		}
	}
}

func (c *Client) finish() {
	c.closing = true
	if c.conn != nil {
		c.conn.Close(websocket.StatusNormalClosure, "")
		c.conn = nil
	}
	c.failMutations(errClosed)
	for _, sub := range c.subs {
		sub.watch.fail(errClosed)
	}
	close(c.stopped)
}

func (c *Client) onCommand(cmd command) {
	switch cmd.kind {
	case kindAdd:
		c.addWatch(cmd)
	case kindRemove:
		c.removeWatch(cmd)
	case kindMutate:
		c.mutate(cmd)
	case kindSever:
		c.dropSocket()
	}
}

func (c *Client) addWatch(cmd command) {
	raw, err := marshalArgs(cmd.args)
	if err != nil {
		cmd.watch.fail(err)
		return
	}
	id := c.nextQuery
	c.nextQuery++
	cmd.watch.id = id
	sub := &subscription{
		id:    id,
		path:  cmd.path,
		args:  raw,
		watch: cmd.watch,
	}
	c.subs[id] = sub
	c.queue = append(c.queue, modification{
		Type:    "Add",
		QueryID: id,
		UdfPath: cmd.path,
		Args:    raw,
	})
	c.flush()
}

func (c *Client) removeWatch(cmd command) {
	id := cmd.id
	if id == 0 && cmd.watch != nil {
		for sid, sub := range c.subs {
			if sub.watch == cmd.watch {
				id = sid
				break
			}
		}
	}
	errc := cmd.errc
	sub, ok := c.subs[id]
	if !ok {
		reply(errc, nil)
		return
	}
	delete(c.subs, id)
	if !sub.sent {
		c.queue = dropQuery(c.queue, id)
		reply(errc, nil)
		return
	}
	c.queue = append(c.queue, modification{Type: "Remove", QueryID: id})
	c.flush()
	reply(errc, nil)
}

func (c *Client) mutate(cmd command) {
	raw, err := marshalArgs(cmd.args)
	if err != nil {
		cmd.resp <- mutationResult{err: err}
		return
	}
	if c.conn == nil {
		cmd.resp <- mutationResult{err: errDisconnected}
		return
	}
	id := c.nextReq
	c.nextReq++
	c.pending[id] = cmd.resp
	msg := mutationMessage{
		Type:      "Mutation",
		RequestID: id,
		UdfPath:   cmd.path,
		Args:      raw,
	}
	if err := c.writeJSON(msg); err != nil {
		delete(c.pending, id)
		cmd.resp <- mutationResult{err: err}
		c.dropSocket()
	}
}

func (c *Client) flush() {
	if c.flight != nil || c.conn == nil || len(c.queue) == 0 {
		return
	}
	mods := c.queue
	c.queue = nil
	want := make(map[uint64]string, len(mods))
	for i := range mods {
		want[mods[i].QueryID] = mods[i].Type
		if mods[i].Type == "Add" {
			if sub := c.subs[mods[i].QueryID]; sub != nil {
				sub.sent = true
			}
		}
	}
	base := c.version
	msg := modifyQuerySet{
		Type:          "ModifyQuerySet",
		BaseVersion:   base,
		NewVersion:    base + 1,
		Modifications: mods,
	}
	if err := c.writeJSON(msg); err != nil {
		c.dropSocket()
		return
	}
	c.flight = &flight{base: base, want: want}
}

func (c *Client) onMessage(data []byte) {
	var env struct {
		Type string `json:"type"`
	}
	if err := json.Unmarshal(data, &env); err != nil {
		c.dropSocket()
		return
	}
	switch env.Type {
	case "Transition":
		var msg transitionMessage
		if err := json.Unmarshal(data, &msg); err != nil {
			c.dropSocket()
			return
		}
		c.onTransition(msg)
	case "MutationResponse":
		var msg mutationResponse
		if err := json.Unmarshal(data, &msg); err != nil {
			c.dropSocket()
			return
		}
		c.onMutation(msg)
	case "FatalError":
		c.dropSocket()
	default:
		c.dropSocket()
	}
}

func (c *Client) onTransition(msg transitionMessage) {
	if msg.StartVersion != c.version || msg.EndVersion <= msg.StartVersion {
		c.dropSocket()
		return
	}
	start := msg.StartVersion
	c.version = msg.EndVersion
	for _, mod := range msg.Modifications {
		c.apply(mod)
	}
	if c.flight != nil && start == c.flight.base && c.flight.met(msg.Modifications) {
		c.flight = nil
		c.flush()
	}
}

func (c *Client) apply(mod stateModification) {
	switch mod.Type {
	case "QueryUpdated":
		sub := c.subs[mod.QueryID]
		if sub == nil {
			return
		}
		sub.watch.deliver(mod.Value)
	case "QueryFailed":
		sub := c.subs[mod.QueryID]
		if sub == nil {
			return
		}
		delete(c.subs, mod.QueryID)
		sub.watch.fail(fmt.Errorf("refhir: %s: %s", sub.path, mod.Error))
	case "QueryRemoved":
	}
}

func (f *flight) met(mods []stateModification) bool {
	seen := make(map[uint64]string, len(mods))
	for _, mod := range mods {
		switch mod.Type {
		case "QueryUpdated", "QueryFailed":
			seen[mod.QueryID] = "Add"
		case "QueryRemoved":
			seen[mod.QueryID] = "Remove"
		}
	}
	for id, kind := range f.want {
		if seen[id] != kind {
			return false
		}
	}
	return true
}

func (c *Client) onMutation(msg mutationResponse) {
	ch := c.pending[msg.RequestID]
	delete(c.pending, msg.RequestID)
	if ch == nil {
		return
	}
	if msg.Success {
		ch <- mutationResult{value: cloneRaw(msg.Result)}
		return
	}
	ch <- mutationResult{err: &OutcomeError{Outcome: cloneRaw(msg.Result)}}
}

func (c *Client) onDown() {
	if c.closing {
		return
	}
	c.gen++
	c.failMutations(errDisconnected)
	c.flight = nil
	c.queue = nil
	if c.conn != nil {
		c.conn.CloseNow()
		c.conn = nil
	}
	conn, err := c.dialUntilCancelled()
	if err != nil {
		return
	}
	c.conn = conn
	c.version = 0
	go c.read(conn, c.gen)
	c.resubscribe()
}

func (c *Client) resubscribe() {
	if len(c.subs) == 0 {
		return
	}
	ids := make([]uint64, 0, len(c.subs))
	for id := range c.subs {
		ids = append(ids, id)
	}
	sort.Slice(ids, func(i, j int) bool { return ids[i] < ids[j] })
	mods := make([]modification, 0, len(ids))
	for _, id := range ids {
		sub := c.subs[id]
		sub.sent = true
		mods = append(mods, modification{
			Type:    "Add",
			QueryID: id,
			UdfPath: sub.path,
			Args:    sub.args,
		})
	}
	c.queue = mods
	c.flush()
}

func (c *Client) dialUntilCancelled() (*websocket.Conn, error) {
	backoff := 50 * time.Millisecond
	for {
		if c.root.Err() != nil {
			return nil, errClosed
		}
		ctx, cancel := context.WithTimeout(c.root, 10*time.Second)
		conn, err := dialSocket(ctx, c.wsURL, c.token)
		cancel()
		if err == nil {
			return conn, nil
		}
		timer := time.NewTimer(backoff)
		select {
		case <-c.root.Done():
			timer.Stop()
			return nil, errClosed
		case <-timer.C:
		}
		if backoff < time.Second {
			backoff *= 2
		}
	}
}

func (c *Client) dropSocket() {
	if c.conn == nil {
		return
	}
	conn := c.conn
	c.conn = nil
	conn.CloseNow()
}

func (c *Client) failMutations(err error) {
	for id, ch := range c.pending {
		ch <- mutationResult{err: err}
		delete(c.pending, id)
	}
}

func (c *Client) writeJSON(value any) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(c.root, 30*time.Second)
	defer cancel()
	// The actor goroutine is the only writer, so frames stay in order.
	return c.conn.Write(ctx, websocket.MessageText, data)
}

func (c *Client) read(conn *websocket.Conn, gen int) {
	for {
		_, data, err := conn.Read(c.root)
		if err != nil {
			c.sendEvent(sessionEvent{gen: gen, down: true})
			return
		}
		buf := make([]byte, len(data))
		copy(buf, data)
		if !c.sendEvent(sessionEvent{gen: gen, data: buf}) {
			return
		}
	}
}

func (c *Client) sendEvent(ev sessionEvent) bool {
	select {
	case c.events <- ev:
		return true
	case <-c.root.Done():
		return false
	}
}

func dialSocket(ctx context.Context, wsURL, token string) (*websocket.Conn, error) {
	header := make(http.Header)
	if token != "" {
		header.Set("Authorization", "Bearer "+token)
	}
	conn, _, err := websocket.Dial(ctx, wsURL, &websocket.DialOptions{HTTPHeader: header})
	if err != nil {
		return nil, err
	}
	conn.SetReadLimit(16 << 20)
	return conn, nil
}

func splitBase(baseURL string) (httpBase, wsURL string, err error) {
	parsed, err := url.Parse(baseURL)
	if err != nil {
		return "", "", err
	}
	switch parsed.Scheme {
	case "http", "https", "ws", "wss":
	default:
		return "", "", fmt.Errorf("refhir: unsupported scheme %q", parsed.Scheme)
	}
	ws := *parsed
	switch ws.Scheme {
	case "http":
		ws.Scheme = "ws"
	case "https":
		ws.Scheme = "wss"
	}
	ws.Path = strings.TrimRight(ws.Path, "/")
	if !strings.HasSuffix(ws.Path, "/sync") {
		if ws.Path == "" {
			ws.Path = "/sync"
		} else {
			ws.Path += "/sync"
		}
	}
	ws.RawQuery = ""
	ws.Fragment = ""
	httpURL := ws
	switch httpURL.Scheme {
	case "ws":
		httpURL.Scheme = "http"
	case "wss":
		httpURL.Scheme = "https"
	}
	httpURL.Path = strings.TrimSuffix(ws.Path, "/sync")
	return strings.TrimRight(httpURL.String(), "/"), ws.String(), nil
}

func dropQuery(mods []modification, id uint64) []modification {
	out := mods[:0]
	for _, mod := range mods {
		if mod.QueryID == id && mod.Type == "Add" {
			continue
		}
		out = append(out, mod)
	}
	return out
}

func reply(errc chan error, err error) {
	if errc != nil {
		errc <- err
	}
}
