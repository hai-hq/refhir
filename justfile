# Open refhir.sqlite, install the bundled R4 core package, and serve FHIR plus /sync.
run:
    cargo run -p refhir -- --sqlite refhir.sqlite
