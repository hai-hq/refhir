# Patient summary

`Patient/$summary` returns an [International Patient Summary](https://hl7.org/fhir/uv/ips/) (IPS) of
one patient: a FHIR document Bundle with the patient, the parts of their record that matter to
someone meeting them for the first time, and a readable table of each. Give it to a clinician, to
an app, or put it behind a [health link](health-links.md).

The summary is built when you ask for it, from what the server holds at that moment. It is not
stored.

## Ask for one

By the patient's id, with `GET` or `POST`:

```bash
curl -H "Authorization: Bearer $TOKEN" 'http://localhost:8080/Patient/p1/$summary'
```

Or, from a client that is not tied to one patient, by identifier (`system|value`), as a query
parameter or as an `identifier` string in a `Parameters` body:

```bash
curl -H "Authorization: Bearer $TOKEN" \
  'http://localhost:8080/Patient/$summary?identifier=http://hospital.example|mrn-1'
```

No patient with that identifier is a `404`, more than one is a `412`, and a request that names
neither an id nor an identifier is a `400`. A `profile` parameter is accepted; the summary is
always the IPS one.

## What is in it

A section holds the patient's resources of the kinds it takes, chosen by these rules:

| Section | LOINC code | Holds | Chosen |
|---|---|---|---|
| Allergies and Intolerances | `48765-2` | `AllergyIntolerance` | Not inactive, resolved, refuted or entered in error |
| Problem List | `11450-4` | `Condition` | Not inactive, resolved, refuted or entered in error |
| Medication Summary | `10160-0` | `MedicationStatement`, `MedicationRequest` | Statements that are active, intended or on hold; requests that are active or on hold |
| Immunizations | `11369-6` | `Immunization` | Completed |
| Results | `30954-2` | `Observation`, `DiagnosticReport` | Laboratory results (an observation in the `laboratory` category, a report in `LAB`) that are final, amended or corrected (a report may also be appended) |
| History of Procedures | `47519-4` | `Procedure` | Not entered in error and not "not done" |

The first three sections are required by the IPS profile. When one has nothing to show, it is still
there and says so: its text reads that no information is available, and its `emptyReason` is
`unavailable`. The other three are left out when they are empty.

Each section carries a table in its `text`, so a person can read the summary without opening a
single resource. A medication that points at a `Medication` resource is shown by that resource's
code.

Along with the patient's own resources the summary holds what they point at, so that every
reference in it resolves inside it: the practitioners, organizations, locations, medications,
devices and specimens they name, and the observations that a diagnostic report lists as its
results. It never holds another patient. A link from this patient to another (`Patient.link`) is
left out of the patient, and anything else that belongs to someone else stays out.

## The document

The Bundle is a `document`. It has an `identifier` and a `timestamp`, the Composition is its first
entry, and every entry has a `urn:uuid` `fullUrl` that the references inside it point at. The
Bundle names the IPS profile in `meta.profile`, and so does the Composition.

## Who can ask

The caller needs to be able to read `Patient`. The summary then holds only the kinds of resource
their scopes let them read, so a token with `patient/Patient.rs` and `patient/Condition.rs` gets
the patient and their problems, and the other sections say that nothing is available. Under SMART a
token that is tied to one patient asks by the patient's id; asking for another patient, or by
identifier, is a `403`. See [SMART on FHIR](smart.md).

`$everything` and `$document` follow the same rule: they hold only what the caller's scopes let
them read.

## Check it against the IPS profiles

The summary is built whether or not the IPS package is installed. Install it to check summaries
against the profiles:

```toml
[conformance]
package_download = true
packages = ["hl7.fhir.uv.ips@2.0.1"]
```

The IPS package depends on other packages, such as the IPA package, and installing it fetches them,
so downloads have to be on while it installs. See
[Package source](../deploy/production.md#package-source) and
[Configuration](configuration.md#configuration-file).

Then post a summary to `[base]/Bundle/$validate`. It names its own profile, so it is checked
against the IPS one. A code from a code system the server holds only in part or not at all (SNOMED
CT is the usual one) cannot be checked against a value set. That is reported as a warning, and it
does not make the summary invalid; see [Terminology](fhir-api.md#terminology).

## What it covers

The summary has the six sections above. The other IPS sections (medical devices, advance
directives, alerts, functional status, past problems, pregnancy history, patient story, plan of
care, social history and vital signs) are not included.
