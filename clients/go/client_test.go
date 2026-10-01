package refhir

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"testing"
	"time"
)

func TestWireFieldNames(t *testing.T) {
	query, err := json.Marshal(modifyQuerySet{
		Type:        "ModifyQuerySet",
		BaseVersion: 0,
		NewVersion:  1,
		Modifications: []modification{{
			Type:    "Add",
			QueryID: 1,
			UdfPath: "Patient/search",
			Args:    json.RawMessage(`{"active":"true"}`),
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	assertKeys(t, query, "type", "baseVersion", "newVersion", "modifications")

	var mods []map[string]any
	var envelope struct {
		Modifications []map[string]any `json:"modifications"`
	}
	if err := json.Unmarshal(query, &envelope); err != nil {
		t.Fatal(err)
	}
	mods = envelope.Modifications
	if mods[0]["type"] != "Add" || mods[0]["udfPath"] != "Patient/search" {
		t.Fatalf("add modification: %s", query)
	}
	if _, ok := mods[0]["queryId"]; !ok {
		t.Fatal("queryId")
	}

	mutation, err := json.Marshal(mutationMessage{
		Type:      "Mutation",
		RequestID: 4,
		UdfPath:   "Patient/update",
		Args:      json.RawMessage(`{"id":"p1"}`),
	})
	if err != nil {
		t.Fatal(err)
	}
	assertKeys(t, mutation, "type", "requestId", "udfPath", "args")

	httpBase, wsURL, err := splitBase("http://127.0.0.1:8080")
	if err != nil {
		t.Fatal(err)
	}
	if httpBase != "http://127.0.0.1:8080" || wsURL != "ws://127.0.0.1:8080/sync" {
		t.Fatalf("base %s socket %s", httpBase, wsURL)
	}
}

func TestSearchFollowsMutation(t *testing.T) {
	base := startServer(t)
	ctx := context.Background()
	reader, err := Dial(ctx, base)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { reader.Close() })
	writer, err := Dial(ctx, base)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { writer.Close() })

	if _, err := reader.Metadata(ctx); err != nil {
		t.Fatal(err)
	}
	created, err := reader.Create(ctx, "Patient", patientJSON("p1", "Ann"))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(created, []byte(`"Ann"`)) {
		t.Fatalf("create: %s", created)
	}
	watch, err := reader.Search(ctx, "Patient", map[string]string{"active": "true"})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { watch.Close() })
	if family(t, watch.Latest()) != "Ann" {
		t.Fatalf("first result: %s", watch.Latest())
	}

	if _, err := writer.Update(ctx, "Patient", "p1", patientJSON("p1", "Bee"), ""); err != nil {
		t.Fatal(err)
	}
	nextCtx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	next, err := watch.Next(nextCtx)
	if err != nil {
		t.Fatal(err)
	}
	if family(t, next) != "Bee" {
		t.Fatalf("transition: %s", next)
	}

	got, err := reader.Query(ctx, "Patient/read", map[string]any{"id": "p1"})
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(got, []byte(`"Bee"`)) {
		t.Fatalf("query: %s", got)
	}
}

func TestMutationOutcome(t *testing.T) {
	base := startServer(t)
	ctx := context.Background()
	client, err := Dial(ctx, base)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })

	_, err = client.Update(ctx, "Patient", "missing", patientJSON("missing", "Ann"), "")
	var outcome *OutcomeError
	if !errors.As(err, &outcome) {
		t.Fatalf("expected OperationOutcome, got %v", err)
	}
	if !bytes.Contains(outcome.Outcome, []byte(`"OperationOutcome"`)) {
		t.Fatalf("outcome: %s", outcome.Outcome)
	}
}

func TestWatchResubscribesAfterDisconnect(t *testing.T) {
	base := startServer(t)
	ctx := context.Background()
	client, err := Dial(ctx, base)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	if _, err := client.Create(ctx, "Patient", patientJSON("p1", "Ann")); err != nil {
		t.Fatal(err)
	}
	watch, err := client.Search(ctx, "Patient", map[string]string{"active": "true"})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { watch.Close() })

	client.sever()
	nextCtx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	next, err := watch.Next(nextCtx)
	if err != nil {
		t.Fatal(err)
	}
	if family(t, next) != "Ann" {
		t.Fatalf("resubscribe: %s", next)
	}
}

func TestSmartBearer(t *testing.T) {
	base := startServer(t, "--smart")
	ctx := context.Background()
	_, err := Dial(ctx, base)
	if err == nil || !bytes.Contains([]byte(err.Error()), []byte("401")) {
		t.Fatalf("dial without token: %v", err)
	}

	token := issueToken(t, base)
	client, err := Dial(ctx, base, Bearer(token))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	body, err := client.Metadata(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(body, []byte(`"CapabilityStatement"`)) {
		t.Fatalf("metadata: %s", body)
	}
	if _, err := client.Create(ctx, "Patient", patientJSON("p1", "Ann")); err != nil {
		t.Fatal(err)
	}
}

func TestTransactionAndDelete(t *testing.T) {
	base := startServer(t)
	ctx := context.Background()
	client, err := Dial(ctx, base)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })

	bundle := json.RawMessage(`{
		"resourceType":"Bundle",
		"type":"transaction",
		"entry":[{
			"resource":{"resourceType":"Patient","id":"p9","active":true,"name":[{"family":"Cam"}]},
			"request":{"method":"POST","url":"Patient"}
		}]
	}`)
	if _, err := client.Transaction(ctx, bundle); err != nil {
		t.Fatal(err)
	}
	watch, err := client.Read(ctx, "Patient", "p9")
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(watch.Latest(), []byte(`"Cam"`)) {
		t.Fatalf("read: %s", watch.Latest())
	}
	watch.Close()
	if _, err := client.Delete(ctx, "Patient", "p9"); err != nil {
		t.Fatal(err)
	}
}

func patientJSON(id, family string) json.RawMessage {
	return json.RawMessage(fmt.Sprintf(
		`{"resourceType":"Patient","id":%q,"active":true,"name":[{"family":%q}]}`,
		id, family,
	))
}

func family(t *testing.T, body json.RawMessage) string {
	t.Helper()
	var bundle struct {
		Entry []struct {
			Resource struct {
				Name []struct {
					Family string `json:"family"`
				} `json:"name"`
			} `json:"resource"`
		} `json:"entry"`
	}
	if err := json.Unmarshal(body, &bundle); err != nil {
		t.Fatal(err)
	}
	if len(bundle.Entry) == 0 || len(bundle.Entry[0].Resource.Name) == 0 {
		t.Fatalf("bundle: %s", body)
	}
	return bundle.Entry[0].Resource.Name[0].Family
}

func issueToken(t *testing.T, base string) string {
	t.Helper()
	resp, err := http.Post(base+"/oauth/token", "application/x-www-form-urlencoded", bytes.NewBufferString("scope=user/*.read+user/*.write"))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("token %d: %s", resp.StatusCode, body)
	}
	var parsed struct {
		AccessToken string `json:"access_token"`
	}
	if err := json.Unmarshal(body, &parsed); err != nil {
		t.Fatal(err)
	}
	if parsed.AccessToken == "" {
		t.Fatalf("token body: %s", body)
	}
	return parsed.AccessToken
}

func refhirBin() string {
	_, file, _, _ := runtime.Caller(0)
	dir := filepath.Dir(file)
	bin := os.Getenv("REFHIR_BIN")
	if bin == "" {
		bin = filepath.Join(dir, "..", "..", "target", "debug", "refhir")
	} else if !filepath.IsAbs(bin) {
		if _, err := os.Stat(bin); err != nil {
			bin = filepath.Join(dir, bin)
		}
	}
	abs, err := filepath.Abs(bin)
	if err != nil {
		return bin
	}
	return abs
}

func startServer(t *testing.T, extra ...string) string {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := ln.Addr().(*net.TCPAddr).Port
	ln.Close()

	dir := t.TempDir()
	bin := refhirBin()
	args := []string{"--sqlite", filepath.Join(dir, "refhir.sqlite"), "--bind", fmt.Sprintf("127.0.0.1:%d", port)}
	args = append(args, extra...)
	cmd := exec.Command(bin, args...)
	cmd.Dir = dir
	var output bytes.Buffer
	cmd.Stdout = &output
	cmd.Stderr = &output
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		cmd.Process.Kill()
		cmd.Wait()
	})

	base := fmt.Sprintf("http://127.0.0.1:%d", port)
	deadline := time.Now().Add(60 * time.Second)
	for time.Now().Before(deadline) {
		resp, err := http.Get(base + "/metadata")
		if err == nil {
			io.Copy(io.Discard, resp.Body)
			resp.Body.Close()
			if resp.StatusCode == http.StatusOK {
				return base
			}
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("refhir did not start: %s", output.String())
	return ""
}

func assertKeys(t *testing.T, data []byte, keys ...string) {
	t.Helper()
	var obj map[string]json.RawMessage
	if err := json.Unmarshal(data, &obj); err != nil {
		t.Fatal(err)
	}
	for _, key := range keys {
		if _, ok := obj[key]; !ok {
			t.Fatalf("missing %s in %s", key, data)
		}
	}
}
