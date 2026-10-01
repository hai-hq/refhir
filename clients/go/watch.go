package refhir

import (
	"context"
	"encoding/json"
	"errors"
	"sync"
)

// Watch is a live read or search. Latest is the last committed result.
// Next returns a result newer than the one Watch or the previous Next already observed.
type Watch struct {
	client *Client
	id     uint64

	mu     sync.Mutex
	latest json.RawMessage
	seq    uint64
	seen   uint64
	err    error
	closed bool
	notify chan struct{}
}

func newWatch(client *Client) *Watch {
	return &Watch{
		client: client,
		notify: make(chan struct{}),
	}
}

// Latest returns a copy of the last committed JSON, or nil before the first result.
func (w *Watch) Latest() json.RawMessage {
	w.mu.Lock()
	defer w.mu.Unlock()
	return cloneRaw(w.latest)
}

// Next waits for a committed result that Watch or a previous Next has not returned.
func (w *Watch) Next(ctx context.Context) (json.RawMessage, error) {
	w.mu.Lock()
	after := w.seen
	w.mu.Unlock()
	return w.waitAfter(ctx, after)
}

// Close drops the subscription. The server is sent a Remove if it already has this query.
func (w *Watch) Close() error {
	w.mu.Lock()
	if w.closed {
		w.mu.Unlock()
		return nil
	}
	w.closed = true
	if w.err == nil {
		w.err = errWatchClosed
	}
	w.wakeLocked()
	id := w.id
	w.mu.Unlock()

	if w.client == nil || id == 0 {
		return nil
	}
	errc := make(chan error, 1)
	select {
	case w.client.cmds <- command{kind: kindRemove, id: id, watch: w, errc: errc}:
	case <-w.client.root.Done():
		return errClosed
	}
	select {
	case err := <-errc:
		return err
	case <-w.client.root.Done():
		return errClosed
	}
}

func (w *Watch) deliver(value json.RawMessage) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.closed {
		return
	}
	w.latest = cloneRaw(value)
	w.seq++
	w.wakeLocked()
}

func (w *Watch) fail(err error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.closed {
		return
	}
	if w.err == nil {
		w.err = err
	}
	w.wakeLocked()
}

func (w *Watch) wakeLocked() {
	close(w.notify)
	w.notify = make(chan struct{})
}

func (w *Watch) waitAfter(ctx context.Context, after uint64) (json.RawMessage, error) {
	for {
		w.mu.Lock()
		if w.seq > after {
			value := cloneRaw(w.latest)
			w.seen = w.seq
			w.mu.Unlock()
			return value, nil
		}
		if w.err != nil {
			err := w.err
			w.mu.Unlock()
			return nil, err
		}
		notify := w.notify
		w.mu.Unlock()
		select {
		case <-notify:
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-w.client.root.Done():
			return nil, errClosed
		}
	}
}

func cloneRaw(value json.RawMessage) json.RawMessage {
	if value == nil {
		return nil
	}
	return append(json.RawMessage(nil), value...)
}

// errWatchClosed is returned from Next after Close.
var errWatchClosed = errors.New("refhir: watch closed")
