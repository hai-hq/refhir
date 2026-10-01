package refhir

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"strings"
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

// Do sends one REST call to the server origin. CRUD goes through Mutate and Watch.
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
	if c.token != "" {
		req.Header.Set("Authorization", "Bearer "+c.token)
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return data, &HTTPError{Status: resp.StatusCode, Body: data}
	}
	return data, nil
}
