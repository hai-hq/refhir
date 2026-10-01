package refhir

import (
	"context"
	"encoding/json"
)

// Search subscribes to {resourceType}/search. Parameter values are strings, matching the server.
func (c *Client) Search(ctx context.Context, resourceType string, params map[string]string) (*Watch, error) {
	args := make(map[string]any, len(params))
	for key, value := range params {
		args[key] = value
	}
	return c.Watch(ctx, resourceType+"/search", args)
}

// Read subscribes to {resourceType}/read.
func (c *Client) Read(ctx context.Context, resourceType, id string) (*Watch, error) {
	return c.Watch(ctx, resourceType+"/read", map[string]any{"id": id})
}

// Query subscribes, returns the first result, and removes the subscription.
func (c *Client) Query(ctx context.Context, path string, args map[string]any) (json.RawMessage, error) {
	watch, err := c.Watch(ctx, path, args)
	if err != nil {
		return nil, err
	}
	defer watch.Close()
	return watch.Latest(), nil
}

// Create calls {resourceType}/create.
func (c *Client) Create(ctx context.Context, resourceType string, resource json.RawMessage) (json.RawMessage, error) {
	return c.Mutate(ctx, resourceType+"/create", map[string]any{"resource": resource})
}

// Update calls {resourceType}/update. An empty ifMatch omits the field.
func (c *Client) Update(ctx context.Context, resourceType, id string, resource json.RawMessage, ifMatch string) (json.RawMessage, error) {
	args := map[string]any{
		"id":       id,
		"resource": resource,
	}
	if ifMatch != "" {
		args["ifMatch"] = ifMatch
	}
	return c.Mutate(ctx, resourceType+"/update", args)
}

// Delete calls {resourceType}/delete.
func (c *Client) Delete(ctx context.Context, resourceType, id string) (json.RawMessage, error) {
	return c.Mutate(ctx, resourceType+"/delete", map[string]any{"id": id})
}

// Transaction calls Bundle/transaction.
func (c *Client) Transaction(ctx context.Context, bundle json.RawMessage) (json.RawMessage, error) {
	return c.Mutate(ctx, "Bundle/transaction", map[string]any{"resource": bundle})
}
