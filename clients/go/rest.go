package refhir

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
)

const (
	// headerCommitTs carries the commit (or read snapshot) timestamp on REST responses.
	headerCommitTs = "X-Refhir-Commit-Ts"
	// headerMinTs asks a replica to catch up to a timestamp before answering a read.
	headerMinTs = "X-Refhir-Min-Ts"
)

// HTTPError is a non-2xx REST response. Body is the raw response.
type HTTPError struct {
	Status int
	Body   []byte
}

func (e *HTTPError) Error() string {
	return fmt.Sprintf("refhir: HTTP %d: %s", e.Status, e.Body)
}

// Metadata fetches GET /metadata.
func (c *Client) Metadata(ctx context.Context) ([]byte, error) {
	return c.Do(ctx, http.MethodGet, "/metadata", nil)
}

// LastCommitTs is the highest commit timestamp this client has seen, from
// X-Refhir-Commit-Ts on REST responses and ts on /sync mutation responses.
// It is 0 until the server reports one. With read-your-writes on (the default,
// see WithReadYourWrites) the value is sent as X-Refhir-Min-Ts on every REST
// request. Safe to call from any goroutine.
func (c *Client) LastCommitTs() uint64 {
	return c.lastTs.Load()
}

// observeTs raises lastTs to ts. It never lowers it.
func (c *Client) observeTs(ts uint64) {
	for {
		cur := c.lastTs.Load()
		if ts <= cur || c.lastTs.CompareAndSwap(cur, ts) {
			return
		}
	}
}

// parseTS reads a timestamp from a /sync ts field, which the server sends as a
// decimal string (a bare number and null are tolerated).
func parseTS(raw []byte) (uint64, bool) {
	text := strings.Trim(strings.TrimSpace(string(raw)), `"`)
	if text == "" || text == "null" {
		return 0, false
	}
	ts, err := strconv.ParseUint(text, 10, 64)
	if err != nil {
		return 0, false
	}
	return ts, true
}

// Do sends one REST call to the server origin. CRUD goes through Mutate and Watch.
//
// With read-your-writes on, the request carries X-Refhir-Min-Ts and the response's
// X-Refhir-Commit-Ts feeds LastCommitTs.
func (c *Client) Do(ctx context.Context, method, path string, body []byte) ([]byte, error) {
	if !strings.HasPrefix(path, "/") {
		path = "/" + path
	}
	var reader io.Reader
	if body != nil {
		reader = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.httpBase+path, reader)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "application/fhir+json")
	if body != nil {
		req.Header.Set("Content-Type", "application/fhir+json")
	}
	token, err := c.bearer(ctx)
	if err != nil {
		return nil, &tokenError{err}
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	if c.readYourWrites {
		if ts := c.lastTs.Load(); ts > 0 {
			req.Header.Set(headerMinTs, strconv.FormatUint(ts, 10))
		}
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if ts, ok := parseTS([]byte(resp.Header.Get(headerCommitTs))); ok {
		c.observeTs(ts)
	}
	data, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return data, &HTTPError{Status: resp.StatusCode, Body: data}
	}
	return data, nil
}
