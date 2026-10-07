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
// See README.md in this directory for examples, operation arguments, and
// reconnect behavior.
package refhir
