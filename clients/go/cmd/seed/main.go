// Seed a running ReFHIR server from JSON files (single resources or FHIR Bundles).
//
//	cargo run -p refhir -- --memory
//	go run ./cmd/seed -dir ../../../fixtures/mock-fhir/library
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"time"

	"github.com/hai-hq/haifhir/clients/go"
)

func main() {
	base := flag.String("url", "http://127.0.0.1:8080", "ReFHIR HTTP origin")
	dir := flag.String("dir", defaultDataDir(), "directory of .json files (recursive)")
	token := flag.String("token", "", "optional bearer token (SMART)")
	batch := flag.Int("batch", 40, "resources per transaction bundle")
	timeout := flag.Duration("timeout", 10*time.Minute, "overall timeout")
	flag.Parse()

	ctx, cancel := context.WithTimeout(context.Background(), *timeout)
	defer cancel()

	abs, err := filepath.Abs(*dir)
	if err != nil {
		log.Fatal(err)
	}
	resources, err := loadResources(abs)
	if err != nil {
		log.Fatalf("load %s: %v", abs, err)
	}
	if len(resources) == 0 {
		log.Fatalf("no FHIR resources found under %s", abs)
	}

	var opts []refhir.Option
	if *token != "" {
		opts = append(opts, refhir.Bearer(*token))
	}
	client, err := refhir.Dial(ctx, *base, opts...)
	if err != nil {
		log.Fatalf("dial: %v", err)
	}
	defer client.Close()

	if _, err := client.Metadata(ctx); err != nil {
		log.Fatalf("server not reachable at %s: %v", *base, err)
	}

	var created int
	for i, batchResources := range chunkResources(resources, *batch) {
		raw, err := json.Marshal(transactionBundle(batchResources))
		if err != nil {
			log.Fatal(err)
		}
		result, err := client.Transaction(ctx, raw)
		if err != nil {
			var outcome *refhir.OutcomeError
			if errors.As(err, &outcome) {
				log.Fatalf("batch %d failed (duplicate ids or validation): %s", i+1, outcome.Outcome)
			}
			log.Fatalf("batch %d: %v", i+1, err)
		}
		created += len(summarize(result))
	}

	fmt.Printf("Loaded %d resources from %s (%d transaction batches).\n", len(resources), abs, len(chunkResources(resources, *batch)))
	fmt.Printf("Server reported %d created entries.\n", created)
}

func defaultDataDir() string {
	candidates := []string{
		"fixtures/mock-fhir/library",
		filepath.Join("..", "..", "fixtures", "mock-fhir", "library"),
		filepath.Join("..", "..", "..", "..", "fixtures", "mock-fhir", "library"),
	}
	for _, dir := range candidates {
		if info, err := os.Stat(dir); err == nil && info.IsDir() {
			return dir
		}
	}
	return "fixtures/mock-fhir/library"
}

func summarize(response json.RawMessage) []string {
	var bundle struct {
		Entry []struct {
			Resource struct {
				ResourceType string `json:"resourceType"`
				ID           string `json:"id"`
			} `json:"resource"`
			Response struct {
				Status string `json:"status"`
			} `json:"response"`
		} `json:"entry"`
	}
	if err := json.Unmarshal(response, &bundle); err != nil {
		return []string{string(response)}
	}
	var lines []string
	for _, entry := range bundle.Entry {
		if entry.Resource.ID == "" {
			continue
		}
		lines = append(lines, fmt.Sprintf("%s/%s (%s)",
			entry.Resource.ResourceType, entry.Resource.ID, entry.Response.Status))
	}
	return lines
}
