# Open refhir.sqlite, install the bundled R4 core package, and serve FHIR plus /sync.
run:
    cargo run -p refhir -- --sqlite refhir.sqlite

# Ephemeral local server (no SQLite file).
memory:
    cargo run -p refhir -- --memory --trace-engine

# Load mock FHIR JSON from fixtures/mock-fhir/library (see fixtures/mock-fhir/README.md).
seed url="http://127.0.0.1:8080" dir="../../fixtures/mock-fhir/library":
    cd clients/go && go run ./cmd/seed -url {{url}} -dir {{dir}}
