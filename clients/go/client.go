package refhir

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"sort"
	"strconv"
	"strings"
	"sync/atomic"
	"time"

	"github.com/coder/websocket"
)

var (
	errClosed = errors.New("refhir: client closed")

	// ErrDisconnected is what a mutation fails with when the socket went down while it was in
	// flight. It may have been applied: the answer was lost, not the write. Check before sending it
	// again, since a create assigns a new id each time.
	ErrDisconnected = errors.New("refhir: disconnected")

	// ErrNotSent is what a mutation fails with when the client had no socket to send it on, as
	// while it dials again after the connection dropped. Nothing was written, so it is safe to try
	// again once Connected reports true.
	ErrNotSent = errors.New("refhir: disconnected: the mutation was not sent")
)

// UpgradeError is a /sync upgrade the server refused: it answered the WebSocket handshake with
// an HTTP error. Dial returns it, and so does a session that could not dial again. A 401 or 403
// is final: the session ends rather than dial a server that has said the token does not stand.
type UpgradeError struct {
	// Status is the HTTP status of the refusal.
	Status int
	// RetryAfter is how long the server's Retry-After header asked the client to wait, or zero.
	RetryAfter time.Duration
}

func (e *UpgradeError) Error() string {
	return fmt.Sprintf("refhir: the server refused the /sync upgrade: HTTP %d", e.Status)
}

// maxRetryAfter caps how long a Retry-After can hold a redial back.
const maxRetryAfter = time.Minute

// retryAfter reads a Retry-After header: a number of seconds, or an HTTP date.
func retryAfter(value string) time.Duration {
	value = strings.TrimSpace(value)
	if value == "" {
		return 0
	}
	var wait time.Duration
	if seconds, err := strconv.Atoi(value); err == nil {
		wait = time.Duration(seconds) * time.Second
	} else if when, err := http.ParseTime(value); err == nil {
		wait = time.Until(when)
	}
	if wait < 0 {
		return 0
	}
	if wait > maxRetryAfter {
		return maxRetryAfter
	}
	return wait
}

// QueryError is a live query the server stopped answering: its first run failed, or a later run
// did and the server dropped it. Watch returns it from Next (and from the call that added the
// query, when that failed). Once a query has failed it is over; add it again to watch again.
//
// Message is plain text whose wording is not a contract. Code is: it holds the FHIR IssueType the
// server gave, or is empty for a server that sends none.
//
//	var failed *refhir.QueryError
//	if errors.As(err, &failed) && failed.Code == refhir.CodeDeleted {
//		// the resource being watched was deleted
//	}
type QueryError struct {
	// Path is the operation that was being watched, such as Patient/read.
	Path string
	// Code is the FHIR IssueType: CodeDeleted, CodeNotFound, CodeForbidden, CodeThrottled,
	// CodeTooCostly, CodeInvalid, CodeTransient or CodeException, or another code a newer server
	// uses.
	Code string
	// Message is the server's explanation, for a person to read.
	Message string
}

// The codes of a QueryError that the server uses.
const (
	// CodeDeleted: the watched resource was deleted, which is the usual reason a query ends.
	CodeDeleted = "deleted"
	// CodeNotFound: a resource the query names, or a resource type, does not exist.
	CodeNotFound = "not-found"
	// CodeForbidden: the token's scopes (or its patient) do not allow the query.
	CodeForbidden = "forbidden"
	// CodeThrottled: a rate limit or quota refused it; trying again later may work.
	CodeThrottled = "throttled"
	// CodeTooCostly: a cap refused it, such as the number of live queries a connection may hold.
	CodeTooCostly = "too-costly"
	// CodeInvalid: the operation or its arguments cannot be run as a live query.
	CodeInvalid = "invalid"
	// CodeTransient: the server could not answer now and may be able to later.
	CodeTransient = "transient"
	// CodeException: the server failed.
	CodeException = "exception"
)

func (e *QueryError) Error() string {
	return fmt.Sprintf("refhir: %s: %s", e.Path, e.Message)
}

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
	tokens         TokenSource
	readYourWrites bool
}

// TokenSource gives the bearer token for the next /sync upgrade or REST call. The client asks it
// each time it needs one, so it may return a token it has renewed. It is called from the session's
// own goroutine and from the callers of Do, and must be safe for that. A source that returns an
// empty token sends no Authorization header.
type TokenSource func(ctx context.Context) (string, error)

// Bearer sets the Authorization header on the /sync upgrade and on HTTP calls, always to the same
// token. A session that outlives it ends when the server stops accepting it: use WithTokenSource
// for a token that is renewed.
func Bearer(token string) Option {
	return WithTokenSource(func(context.Context) (string, error) { return token, nil })
}

// WithTokenSource sets where the bearer token comes from, for the /sync upgrade on the first dial
// and on every redial, and for HTTP calls (Do and Metadata). Use it with tokens that expire, such
// as a gateway's five-minute OAuth client-credentials tokens: the server drops the socket when a
// token expires, the client dials again, and the source supplies a fresh one.
//
// When the server refuses an upgrade with HTTP 401, the client asks the source once more and dials
// again before it treats the refusal as final, so a source that was handing out a token on its way
// out gets a second chance. A second 401, or a source that returns the very token just refused,
// ends the session; so does 403. An error from the source is not final: a redial tries again after
// the usual backoff, and Dial returns it.
func WithTokenSource(source TokenSource) Option {
	return func(o *dialOptions) {
		o.tokens = source
	}
}

// WithReadYourWrites controls read-your-writes for REST calls; the default is on.
//
// When on, the client remembers the highest commit timestamp the server has
// reported (X-Refhir-Commit-Ts on REST responses, ts on /sync mutation
// responses) and sends it as X-Refhir-Min-Ts on every later REST request, so a
// replica only answers once it has caught up to this client's own writes.
// When off, nothing is sent; LastCommitTs still tracks timestamps.
func WithReadYourWrites(enabled bool) Option {
	return func(o *dialOptions) {
		o.readYourWrites = enabled
	}
}

// Client is one /sync session. Watch and Mutate share that socket.
type Client struct {
	httpBase string
	wsURL    string
	tokens   TokenSource
	http     *http.Client

	// lastTs is the highest commit timestamp seen. Do reads and writes it from
	// any goroutine; the actor goroutine writes it on mutation responses.
	lastTs         atomic.Uint64
	readYourWrites bool

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
	// held is what was queued on cmds while the session dialled again, to be run once it is back.
	held []command

	// down is true from the moment the socket drops until the session has dialled again. It is read
	// by Mutate and Connected from any goroutine.
	down atomic.Bool

	// fatal is why the session ended when the server ended it (an upgrade it refused for good),
	// rather than Close. It is written before the session is cancelled, so any goroutine that
	// sees the cancellation sees it.
	fatal error
}

type subscription struct {
	id    uint64
	path  string
	args  json.RawMessage
	watch *Watch
	sent  bool
}

// flight is the change to the query set that is waiting for its answer. The answer is the
// Transition that holds one item for each modification in it, whichever version it is at.
type flight struct {
	want map[uint64]string
}

type mutationResult struct {
	value json.RawMessage
	err   error
}

type command struct {
	// ctx is the caller's. A command whose context ended before the session reached it is dropped
	// unsent: its caller has given up and cannot know it was run.
	ctx   context.Context
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
	options := dialOptions{readYourWrites: true}
	for _, opt := range opts {
		opt(&options)
	}
	httpBase, wsURL, err := splitBase(baseURL)
	if err != nil {
		return nil, err
	}
	client := &Client{
		httpBase:  httpBase,
		wsURL:     wsURL,
		tokens:    options.tokens,
		http:      &http.Client{},
		cmds:      make(chan command, 64),
		events:    make(chan sessionEvent, 64),
		stopped:   make(chan struct{}),
		gen:       1,
		nextQuery: 1,
		nextReq:   1,
		subs:      make(map[uint64]*subscription),
		pending:   make(map[uint64]chan mutationResult),

		readYourWrites: options.readYourWrites,
	}
	conn, err := client.dialSocket(ctx)
	if err != nil {
		return nil, err
	}
	client.conn = conn
	root, cancel := context.WithCancel(context.Background())
	client.root, client.cancel = root, cancel
	go client.read(conn, 1)
	go client.loop()
	return client, nil
}

// closedErr is the error for a call on a session that has ended: the refusal that ended it, or
// errClosed when it was closed.
func (c *Client) closedErr() error {
	if c.fatal != nil {
		return c.fatal
	}
	return errClosed
}

// Close shuts down the session.
func (c *Client) Close() error {
	c.cancel()
	<-c.stopped
	return nil
}

// Connected reports whether the session has a socket. It is false while the client dials again
// after the connection dropped, and Mutate fails with ErrNotSent until it is true.
func (c *Client) Connected() bool {
	return !c.down.Load() && c.root.Err() == nil
}

// Mutate runs a write operation and waits for MutationResponse.
// A failed mutation returns *OutcomeError. Mutations are not retried after a disconnect.
//
// Two errors say how far a mutation got when the connection was not there. ErrNotSent: nothing was
// written, because the client was dialling again; try again once Connected is true.
// ErrDisconnected: it was sent and the socket dropped before the answer, so it may have been
// applied. A call whose ctx has ended is not sent either, and returns the ctx's error; that holds
// for a call that was already queued when the client began to dial again.
func (c *Client) Mutate(ctx context.Context, path string, args map[string]any) (json.RawMessage, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if c.root.Err() != nil {
		return nil, c.closedErr()
	}
	if c.down.Load() {
		return nil, ErrNotSent
	}
	resp := make(chan mutationResult, 1)
	select {
	case c.cmds <- command{ctx: ctx, kind: kindMutate, path: path, args: args, resp: resp}:
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-c.root.Done():
		return nil, c.closedErr()
	}
	select {
	case result := <-resp:
		return result.value, result.err
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-c.root.Done():
		return nil, c.closedErr()
	}
}

// Watch subscribes to a read-only operation and waits for the first result.
func (c *Client) Watch(ctx context.Context, path string, args map[string]any) (*Watch, error) {
	watch := newWatch(c)
	select {
	case c.cmds <- command{ctx: ctx, kind: kindAdd, path: path, args: args, watch: watch}:
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-c.root.Done():
		return nil, c.closedErr()
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
	err := c.closedErr()
	c.failMutations(err)
	for _, sub := range c.subs {
		sub.watch.fail(err)
	}
	close(c.stopped)
}

func (c *Client) onCommand(cmd command) {
	if cmd.ctx != nil && cmd.ctx.Err() != nil && (cmd.kind == kindAdd || cmd.kind == kindMutate) {
		// Its caller gave up while the command waited, and cannot know it was run. A write that
		// went out now could be applied unseen.
		if cmd.kind == kindMutate {
			cmd.resp <- mutationResult{err: cmd.ctx.Err()}
		}
		return
	}
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
	// Close runs on the caller's goroutine while this one is here, so the id is handed over under
	// the watch's lock. If Close has already run there is nobody to tell about the query, and
	// registering it would leave it live on the server with no one to remove it.
	if !cmd.watch.assign(id) {
		return
	}
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
		cmd.resp <- mutationResult{err: ErrNotSent}
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
	c.flight = &flight{want: want}
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
	c.version = msg.EndVersion
	for _, mod := range msg.Modifications {
		c.apply(mod)
	}
	// A push can land between the change being sent and its answer, so the answer is not at the
	// version the change was based on. It is found by what it holds: one item for each
	// modification. The server answers the change before it pushes anything for what it added.
	if c.flight != nil && c.flight.met(msg.Modifications) {
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
		sub.watch.fail(&QueryError{Path: sub.path, Code: mod.Code, Message: mod.Error})
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
		// Record the timestamp before waking the caller so LastCommitTs and the
		// next REST request already reflect this write.
		if ts, ok := parseTS(msg.TS); ok {
			c.observeTs(ts)
		}
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
	c.down.Store(true)
	c.failMutations(ErrDisconnected)
	c.flight = nil
	c.queue = nil
	if c.conn != nil {
		c.conn.CloseNow()
		c.conn = nil
	}
	// A call that was queued before the socket dropped has not been sent either: say so now,
	// instead of sending it whenever the connection returns.
	c.refuseQueuedMutations()
	conn, err := c.dialUntilCancelled()
	if err != nil {
		if !errors.Is(err, errClosed) {
			// The server refused for good. The session ends, and says why.
			c.fatal = err
			c.cancel()
		}
		return
	}
	c.conn = conn
	c.version = 0
	go c.read(conn, c.gen)
	c.resubscribe()
	c.refuseQueuedMutations()
	c.down.Store(false)
	held := c.held
	c.held = nil
	for _, cmd := range held {
		c.onCommand(cmd)
	}
}

// refuseQueuedMutations answers the Mutate calls waiting on cmds with ErrNotSent, and keeps the
// other commands, in order, to run when the session is back. The session goroutine does not read
// cmds while it dials, so these are calls that got past Mutate's own check just as the socket
// dropped.
func (c *Client) refuseQueuedMutations() {
	for {
		select {
		case cmd := <-c.cmds:
			if cmd.kind == kindMutate {
				cmd.resp <- mutationResult{err: ErrNotSent}
				continue
			}
			c.held = append(c.held, cmd)
		default:
			return
		}
	}
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

// tokenError is a failure of the token source rather than of the dial. It is never final: the
// source may work next time.
type tokenError struct{ err error }

func (e *tokenError) Error() string { return "refhir: the token source failed: " + e.err.Error() }
func (e *tokenError) Unwrap() error { return e.err }

// bearer is the token to send now, or empty for none.
func (c *Client) bearer(ctx context.Context) (string, error) {
	if c.tokens == nil {
		return "", nil
	}
	return c.tokens(ctx)
}

// dialSocket opens the /sync socket with the token the source gives. On a 401 it asks the source
// once more and dials again, unless the source gives back the very token that was refused, which
// would only be refused again.
func (c *Client) dialSocket(ctx context.Context) (*websocket.Conn, error) {
	token, err := c.bearer(ctx)
	if err != nil {
		return nil, &tokenError{err}
	}
	conn, err := dialSocket(ctx, c.wsURL, token)
	var refused *UpgradeError
	if err == nil || !errors.As(err, &refused) || refused.Status != http.StatusUnauthorized {
		return conn, err
	}
	fresh, serr := c.bearer(ctx)
	if serr != nil {
		return nil, &tokenError{serr}
	}
	if fresh == token {
		return nil, err
	}
	return dialSocket(ctx, c.wsURL, fresh)
}

func (c *Client) dialUntilCancelled() (*websocket.Conn, error) {
	backoff := 50 * time.Millisecond
	for {
		if c.root.Err() != nil {
			return nil, errClosed
		}
		ctx, cancel := context.WithTimeout(c.root, 10*time.Second)
		conn, err := c.dialSocket(ctx)
		cancel()
		if err == nil {
			return conn, nil
		}
		wait := backoff
		var refused *UpgradeError
		if errors.As(err, &refused) {
			// A token the server does not accept, even one the source has just renewed, will not be
			// accepted by asking again.
			if refused.Status == http.StatusUnauthorized || refused.Status == http.StatusForbidden {
				return nil, err
			}
			wait = max(wait, refused.RetryAfter)
		}
		timer := time.NewTimer(wait)
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
	conn, resp, err := websocket.Dial(ctx, wsURL, &websocket.DialOptions{HTTPHeader: header})
	if err != nil {
		if resp != nil {
			return nil, &UpgradeError{
				Status:     resp.StatusCode,
				RetryAfter: retryAfter(resp.Header.Get("Retry-After")),
			}
		}
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
