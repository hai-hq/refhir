# Mock FHIR fixtures

JSON files used by the seed command (`clients/go/cmd/seed`). Each file is either:

- one FHIR resource with `resourceType` and `id`, or
- a FHIR **Bundle** (typical Synthea output). The loader takes `entry[].resource` and skips nested bundles.

Files can live in subfolders (`Patient/alice.json`, `Organization/org.json`, …). Duplicate `resourceType/id` values are deduped (last file wins).

## Seed the server

```bash
cargo run -p refhir -- --memory

cd clients/go
go run ./cmd/seed -url http://127.0.0.1:8080

# explicit directory (path relative to clients/go)
go run ./cmd/seed -dir ../../fixtures/mock-fhir/library -url http://127.0.0.1:8080
```

Flags: `-dir`, `-url`, `-token`, `-batch` (resources per transaction, default 40), `-timeout`.

Use a **fresh** database when re-seeding the same ids. Synthea re-runs produce the same logical patients only if you fix the random seed.

## Checked-in library

`library/` holds a small hand-written set (multiple patients, related persons, org, services, practitioner, encounter, observation). Good for CI and quick local demos without installing Synthea.

## Synthea (~100 patients)

[Synthea](https://github.com/synthetichealth/synthea) exports one FHIR R4 **Bundle per patient** under `output/fhir/`. Point the seed command at that folder; you do not need to split bundles into per-type files.

```bash
# Install Synthea (Java), then from a working directory:
java -jar synthea-with-dependencies.jar -p 100 Massachusetts

# Seed ReFHIR from generated bundles
cd clients/go && go run ./cmd/seed \
  -dir /path/to/output/fhir \
  -url http://127.0.0.1:8080 \
  -batch 40
```

Keep large generated trees out of git:

```bash
mkdir -p fixtures/mock-fhir/generated
# copy or symlink Synthea output here, then:
go run ./cmd/seed -dir ../../fixtures/mock-fhir/generated/fhir
```

`fixtures/mock-fhir/generated/` is gitignored.

### Custom organization name

Synthea picks hospitals from `providers/hospitals.csv` (and related provider CSVs). To use **your** facility names, edit or replace those CSVs before generating, or post-process exports with **Flexporter**.

Example Flexporter mapping (`flexporter-rename-org.yaml` in this folder) to set `Organization.name` for hospital-type orgs:

```yaml
---
name: Rename hospitals
actions:
  - name: Hospital name
    set_values:
      - applicability: Organization.where(type.coding.code = 'hospital')
        fields:
          - location: Organization.name
            value: "Riverside Community Health"
```

Wire the mapping in `synthea.properties` (see [Synthea Flexporter wiki](https://github.com/synthetichealth/synthea/wiki/Flexporter)) so it runs on export, or run Flexporter on existing bundles.

### Custom organization id

Synthea assigns **UUIDs** to resource ids. There is no single `synthea.properties` flag for a fixed `Organization/id`. Options:

1. **Flexporter** `set_values` on `Organization.id` (and update references if you need a stable id across patients).
2. **Edit JSON** after generation (fragile at scale).
3. **Seed only your org** from `library/Organization/*.json` first, then generate patients that reference that org (requires custom modules or post-processing; not built into stock Synthea).

For ReFHIR demos, the practical pattern is: keep one org file in `library/`, seed it, then seed Synthea patient bundles that reference orgs by id only if you have aligned those ids in Flexporter.

### What ReFHIR accepts

Stock Synthea bundles include many R4 types. ReFHIR validates against its installed packages and reference rules. Some resources may fail validation or reference checks if dependencies are missing or narrative invariants fail on standalone POST. If a batch fails, the seed command stops and prints the `OperationOutcome`. Reduce `-batch` or filter bundles (Flexporter `keep_resources`) to the types you care about.

Start with the small `library/` set, then scale up with Synthea and tune mappings.
