package main

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestLoadLibrary(t *testing.T) {
	dir := libraryDir(t)
	resources, err := loadResources(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(resources) < 10 {
		t.Fatalf("expected at least 10 resources, got %d", len(resources))
	}
	keys := make(map[string]bool)
	for _, resource := range resources {
		key, err := resourceKey(resource)
		if err != nil {
			t.Fatal(err)
		}
		if keys[key] {
			t.Fatalf("duplicate %s after sort", key)
		}
		keys[key] = true
	}
	if !keys["Organization/demo-org"] || !keys["Patient/demo-patient-3"] {
		t.Fatalf("missing expected demo resources: %v", keys)
	}
}

func TestInvalidJSONIncludesPath(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "bad.json")
	if err := os.WriteFile(path, []byte("{"), 0o644); err != nil {
		t.Fatal(err)
	}
	_, err := loadResources(dir)
	if err == nil || !strings.Contains(err.Error(), "bad.json") {
		t.Fatalf("expected bad.json in error, got %v", err)
	}
}

func TestFlattenBundle(t *testing.T) {
	bundle := map[string]any{
		"resourceType": "Bundle",
		"entry": []any{
			map[string]any{
				"resource": map[string]any{
					"resourceType": "Patient",
					"id":           "p1",
				},
			},
		},
	}
	out := flattenBundle(bundle)
	if len(out) != 1 || out[0]["id"] != "p1" {
		t.Fatalf("flatten: %v", out)
	}
}

func libraryDir(t *testing.T) string {
	_, file, _, _ := runtime.Caller(0)
	dir := filepath.Join(filepath.Dir(file), "..", "..", "..", "..", "fixtures", "mock-fhir", "library")
	return dir
}
