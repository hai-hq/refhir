# Open refhir.sqlite, download and install the R4 packages the first time, and serve FHIR plus /sync.
run:
    REFHIR_PACKAGE_DOWNLOAD=true cargo run -p refhir -- --sqlite refhir.sqlite

# Ephemeral local server (no SQLite file).
memory:
    REFHIR_PACKAGE_DOWNLOAD=true cargo run -p refhir -- --memory --trace-engine

# Load mock FHIR JSON from fixtures/mock-fhir/library (see fixtures/mock-fhir/README.md).
seed url="http://127.0.0.1:8080" dir="../../fixtures/mock-fhir/library":
    cd clients/go && go run ./cmd/seed -url {{url}} -dir {{dir}}

# Formatting, lints (a warning fails) and the Rust tests: what CI runs first.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Format the Rust code.
fmt:
    cargo fmt --all

# Check that links and anchors in the Markdown files resolve.
docs-check:
    python3 scripts/check-doc-links.py
