package main

import (
	"encoding/json"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// uploadRank orders resource types so references usually resolve within a batch.
var uploadRank = map[string]int{
	"Organization":         10,
	"Location":             20,
	"HealthcareService":    30,
	"Practitioner":         40,
	"Patient":              50,
	"RelatedPerson":        60,
	"PractitionerRole":     70,
	"Encounter":            80,
	"Condition":            90,
	"Observation":          100,
	"Procedure":            110,
	"MedicationRequest":    120,
	"DiagnosticReport":     130,
	"Immunization":         140,
	"AllergyIntolerance":   150,
	"CarePlan":             160,
	"Claim":                170,
	"DocumentReference":    180,
	"ExplanationOfBenefit": 190,
}

func loadResources(dir string) ([]map[string]any, error) {
	info, err := os.Stat(dir)
	if err != nil {
		return nil, err
	}
	if !info.IsDir() {
		return nil, fmt.Errorf("%s is not a directory", dir)
	}

	var files []string
	err = filepath.WalkDir(dir, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		if strings.EqualFold(filepath.Ext(path), ".json") {
			files = append(files, path)
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	sort.Strings(files)

	byKey := make(map[string]map[string]any)
	for _, path := range files {
		data, err := os.ReadFile(path)
		if err != nil {
			return nil, err
		}
		parsed, err := parseJSONFile(data)
		if err != nil {
			return nil, fmt.Errorf("%s: invalid JSON: %w", path, err)
		}
		for _, resource := range parsed {
			key, err := resourceKey(resource)
			if err != nil {
				return nil, fmt.Errorf("%s: %w", path, err)
			}
			byKey[key] = resource
		}
	}

	keys := make([]string, 0, len(byKey))
	for key := range byKey {
		keys = append(keys, key)
	}
	sort.Slice(keys, func(i, j int) bool {
		ri := rank(keys[i])
		rj := rank(keys[j])
		if ri != rj {
			return ri < rj
		}
		return keys[i] < keys[j]
	})

	out := make([]map[string]any, 0, len(keys))
	for _, key := range keys {
		out = append(out, byKey[key])
	}
	return out, nil
}

func parseJSONFile(data []byte) ([]map[string]any, error) {
	var raw any
	if err := json.Unmarshal(data, &raw); err != nil {
		return nil, err
	}
	return flattenAny(raw), nil
}

func flattenAny(raw any) []map[string]any {
	switch value := raw.(type) {
	case map[string]any:
		if value["resourceType"] == "Bundle" {
			return flattenBundle(value)
		}
		if _, ok := value["resourceType"].(string); ok {
			return []map[string]any{value}
		}
	case []any:
		var out []map[string]any
		for _, item := range value {
			out = append(out, flattenAny(item)...)
		}
		return out
	}
	return nil
}

func flattenBundle(bundle map[string]any) []map[string]any {
	entries, ok := bundle["entry"].([]any)
	if !ok {
		return nil
	}
	var out []map[string]any
	for _, entry := range entries {
		item, ok := entry.(map[string]any)
		if !ok {
			continue
		}
		resource, ok := item["resource"].(map[string]any)
		if !ok {
			continue
		}
		if rt, _ := resource["resourceType"].(string); rt != "" && rt != "Bundle" {
			out = append(out, resource)
		}
	}
	return out
}

func resourceKey(resource map[string]any) (string, error) {
	rt, _ := resource["resourceType"].(string)
	if rt == "" {
		return "", fmt.Errorf("missing resourceType")
	}
	id, _ := resource["id"].(string)
	if id == "" {
		return "", fmt.Errorf("%s missing id", rt)
	}
	return rt + "/" + id, nil
}

func rank(key string) int {
	rt, _, _ := strings.Cut(key, "/")
	if r, ok := uploadRank[rt]; ok {
		return r
	}
	return 500
}

func transactionBundle(resources []map[string]any) map[string]any {
	entries := make([]map[string]any, 0, len(resources))
	for _, resource := range resources {
		rt, _ := resource["resourceType"].(string)
		entries = append(entries, map[string]any{
			"resource": resource,
			"request": map[string]string{
				"method": "POST",
				"url":    rt,
			},
		})
	}
	return map[string]any{
		"resourceType": "Bundle",
		"type":         "transaction",
		"entry":        entries,
	}
}

func chunkResources(resources []map[string]any, size int) [][]map[string]any {
	if size <= 0 {
		size = 50
	}
	var batches [][]map[string]any
	for i := 0; i < len(resources); i += size {
		end := i + size
		if end > len(resources) {
			end = len(resources)
		}
		batches = append(batches, resources[i:end])
	}
	return batches
}
