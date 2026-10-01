# Open haifhir.sqlite, install the bundled R4 core package, and serve FHIR plus /sync.
run:
    cargo run -p haifhir -- --sqlite haifhir.sqlite
