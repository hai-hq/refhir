// Package refhir is the Go client for ReFHIR.
//
// ReFHIR exposes standard FHIR REST and a WebSocket at /sync. This client uses
// /sync as the primary path: subscribe to a read or search, receive updates when
// committed data changes, and run writes on the same session.
//
// Start with Dial, then Search or Read for live data and Create, Update, Delete,
// or Transaction for writes. Resource bodies are encoding/json.RawMessage; the
// server validates against its installed FHIR packages.
//
// REST calls made with Do and Metadata echo the highest commit timestamp seen
// (X-Refhir-Commit-Ts, or ts on /sync mutations) as X-Refhir-Min-Ts so a replica
// never serves a read older than this client's own writes. See LastCommitTs and
// WithReadYourWrites.
//
// See README.md in this directory for examples, operation arguments, and
// reconnect behavior.
package refhir
