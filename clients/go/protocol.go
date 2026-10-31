package refhir

import "encoding/json"

// Wire messages for /sync. Field names match crates/server/src/sync.rs.

type modifyQuerySet struct {
	Type          string         `json:"type"`
	BaseVersion   uint64         `json:"baseVersion"`
	NewVersion    uint64         `json:"newVersion"`
	Modifications []modification `json:"modifications"`
}

type modification struct {
	Type    string          `json:"type"`
	QueryID uint64          `json:"queryId"`
	UdfPath string          `json:"udfPath,omitempty"`
	Args    json.RawMessage `json:"args,omitempty"`
}

type mutationMessage struct {
	Type      string          `json:"type"`
	RequestID uint64          `json:"requestId"`
	UdfPath   string          `json:"udfPath"`
	Args      json.RawMessage `json:"args"`
}

type transitionMessage struct {
	Type          string              `json:"type"`
	StartVersion  uint64              `json:"startVersion"`
	EndVersion    uint64              `json:"endVersion"`
	Modifications []stateModification `json:"modifications"`
}

type stateModification struct {
	Type    string          `json:"type"`
	QueryID uint64          `json:"queryId"`
	Value   json.RawMessage `json:"value"`
	Error   string          `json:"error"`
	Code    string          `json:"code"`
}

type mutationResponse struct {
	Type      string          `json:"type"`
	RequestID uint64          `json:"requestId"`
	Success   bool            `json:"success"`
	Result    json.RawMessage `json:"result"`
	TS        json.RawMessage `json:"ts"`
}

type fatalError struct {
	Type    string `json:"type"`
	Message string `json:"message"`
}

func marshalArgs(args map[string]any) (json.RawMessage, error) {
	if args == nil {
		return json.RawMessage(`{}`), nil
	}
	return json.Marshal(args)
}
