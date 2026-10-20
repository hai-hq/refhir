use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use refhir_database::{ScannedEntry, ScannedRow, ScannedText, Transaction};
use refhir_types::{exact_key, string_interval, token_interval, IndexId, Interval, Timestamp};
use refhir_validate::{TermAnswer, Terminology};
use serde_json::{json, Value};

use crate::catalog::{Catalog, ResourceModel};
use crate::error::FhirError;
use crate::extract::{Probe, ReferenceId};
use crate::ops::decorate;
use crate::resolve::TxHost;

const MODIFIERS: &[&str] = &[
    "exact",
    "contains",
    "missing",
    "text",
    "not",
    "above",
    "below",
    "in",
    "not-in",
    "of-type",
    "identifier",
];

const WIDE: &[&str] = &[
    "contains",
    "missing",
    "not",
    "text",
    "above",
    "below",
    "in",
    "not-in",
    "of-type",
    "identifier",
];

/// A search is limited to this compartment (`Patient/p1`), whatever else the caller asks for. Only
/// the server sets it, to keep a patient-scoped token to its own patient: `_compartment` is the
/// caller's to choose, so a limit carried there could be replaced by the caller. The two are
/// combined, so this can only narrow what a search returns.
pub const PATIENT_COMPARTMENT: &str = "_patientCompartment";

/// The argument the server sets on a search to say it is lenient (`"lenient"`), from the `Prefer:
/// handling=lenient` header. It is not a search parameter, and not the client's to set: see
/// `CONTROL_ARGS`.
pub const HANDLING: &str = "_handling";

/// The argument the server sets on a search `GET` to say how many matches a page has when the
/// request names no `_count`. It is not a search parameter, and not the client's to set: see
/// `CONTROL_ARGS`. The search then runs as if the request had said `_count`, so the Bundle's links
/// show the page that applied, and nothing else sees a `_count` the client did not send.
pub const DEFAULT_COUNT: &str = "_defaultCount";

/// The compartment parameters are not part of a search's identity: a paging link or a cursor does
/// not carry them, because each request gets its limit from the server again.
fn is_compartment_param(key: &str) -> bool {
    key == "_compartment" || key == PATIENT_COMPARTMENT
}

/// Joins two occurrences of a search parameter into one value, for the places that gather a query
/// into a map with a single value per name.
///
/// FHIR reads a repeated parameter as AND (`date=ge2020&date=lt2021`), and a comma inside one value
/// as OR, so a comma cannot join the occurrences. They are joined with `AND`, which `clauses_from`
/// splits again into one clause each. The parameters that take a list rather than a condition
/// (`_include`, `_revinclude` and their `:iterate` forms, `_sort`, `_elements`) are still joined
/// with a comma, which is how they are written.
pub fn join_repeated(name: &str, existing: &str, value: &str) -> String {
    let separator = if is_list_control(name) { ',' } else { AND };
    format!("{existing}{separator}{value}")
}

/// Puts `value` into `params` under `name`; a second occurrence of `name` is joined to the first
/// as `join_repeated` says. Everything that gathers a search query into a map goes through this, so
/// a repeated parameter means the same wherever the query came from.
pub fn add_param(params: &mut BTreeMap<String, String>, name: String, value: String) {
    let value = match params.get(&name) {
        Some(existing) => join_repeated(&name, existing, &value),
        None => value,
    };
    params.insert(name, value);
}

/// Between the occurrences of a repeated search parameter in a value built by `join_repeated`.
/// Control characters do not appear in a search value, so no value is cut in two by it.
pub const AND: char = '\u{1f}';

/// Whether `name` is a control that takes one value (`_count`, `_summary`, `_total`, ...), so it
/// cannot be given more than once: two values are not an AND of anything, only two answers.
pub(crate) fn is_single_valued_control(name: &str) -> bool {
    is_control(name) && !is_list_control(name)
}

fn is_list_control(name: &str) -> bool {
    matches!(
        name,
        "_include"
            | "_include:iterate"
            | "_revinclude"
            | "_revinclude:iterate"
            | "_sort"
            | "_elements"
    )
}

/// Most alternatives one search value may list. Each is an index range to read, and every row
/// found is checked against all of them, so a list as long as a query string allows is refused.
const MAX_ALTERNATIVES: usize = 1000;

/// The alternatives of one search value: a comma between values of the same parameter is OR.
///
/// `\,` is a literal comma. A backslash pair is kept as written, but never hides the comma after
/// it, and every other escape (`\$`, `\|`) is left for the code that splits on that character.
/// Empty alternatives (a doubled or trailing comma) and repeats are dropped, and more than
/// `MAX_ALTERNATIVES` is an error. A value with no comma is its own only alternative, empty or not.
fn or_values(text: &str) -> Result<Vec<String>, FhirError> {
    if !text.contains(',') {
        return Ok(vec![text.to_string()]);
    }
    fn keep(
        values: &mut Vec<String>,
        seen: &mut BTreeSet<String>,
        value: String,
    ) -> Result<(), FhirError> {
        if value.is_empty() || !seen.insert(value.clone()) {
            return Ok(());
        }
        if values.len() == MAX_ALTERNATIVES {
            return Err(FhirError::Invalid(format!(
                "a search value lists more than {MAX_ALTERNATIVES} alternatives"
            )));
        }
        values.push(value);
        Ok(())
    }
    let mut values = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some(',') => current.push(','),
                Some(next) => {
                    current.push('\\');
                    current.push(next);
                }
                None => current.push('\\'),
            },
            ',' => keep(&mut values, &mut seen, std::mem::take(&mut current))?,
            _ => current.push(ch),
        }
    }
    keep(&mut values, &mut seen, current)?;
    if values.is_empty() {
        values.push(String::new());
    }
    Ok(values)
}

struct Controls {
    count: Option<usize>,
    offset: usize,
    sort: Vec<(String, bool)>,
    summary: Option<String>,
    elements: Vec<String>,
    total: String,
    include: Vec<IncludeSpec>,
    revinclude: Vec<IncludeSpec>,
    /// Every one of these must contain a match: the caller's own `_compartment`, and the limit the
    /// server set on the caller (`_patientCompartment`).
    compartments: Vec<Within>,
}

/// A compartment a search is limited to.
struct Within {
    type_name: String,
    id: String,
    /// Set by the server (`_patientCompartment`), not asked for by the caller. A resource that
    /// `_include` or `_revinclude` adds has to respect it as well as a match.
    limit: bool,
}

/// Most documents a page may have for the index-only path to read them one query each.
const MAX_POINT_READS: usize = 100;

impl Controls {
    /// Whether the response states how many resources match.
    fn needs_total(&self) -> bool {
        self.total != "none" || self.summary.as_deref() == Some("count")
    }

    /// Whether the page is small enough to fetch document by document. The index-only path reads
    /// each page document with a query of its own, which beats decoding a whole range when the
    /// page is a few rows of it, and loses to one joined scan when the page is most of the range
    /// (a request with no `_count` pages every match). A page with no documents is free.
    fn page_is_cheap_to_fetch(&self) -> bool {
        !self.needs_documents() || self.count.is_some_and(|count| count <= MAX_POINT_READS)
    }

    /// Whether the resources of the page are used at all: `_summary=count` shows none, and
    /// follows no `_include` or `_revinclude` out of them.
    fn needs_documents(&self) -> bool {
        self.summary.as_deref() != Some("count")
    }
}

struct IncludeSpec {
    /// Resource type named before the parameter, when the token has one.
    source: Option<String>,
    param: String,
    target: Option<String>,
    /// `:iterate`: the rule also applies to the resources the rules have added, over and over until
    /// it adds none. A plain rule applies to the matches only.
    iterate: bool,
}

enum Clause {
    /// The ids a resource may have: a comma between them is OR.
    Id(Vec<String>),
    Filter(String),
    Param {
        name: String,
        modifier: Option<String>,
        /// The alternatives of the value: a resource matches when any of them does (when none of
        /// them does, for the `:not` and `:not-in` modifiers).
        values: Vec<String>,
    },
    Chain {
        param: String,
        target_type: Option<String>,
        target_param: String,
        modifier: Option<String>,
        value: String,
    },
    Has {
        type_name: String,
        ref_param: String,
        search_param: String,
        modifier: Option<String>,
        value: String,
    },
}

/// A page of matches (each with its id), how many matches there are, and the work it took.
type Page = (Vec<(String, Value)>, usize, Work);

/// What a row must satisfy to be a match: the clauses, which of them the index range it was found
/// through has already settled, and the ids that the chain and `_has` clauses resolved to.
struct Checks<'a> {
    clauses: &'a [Clause],
    proven: Option<usize>,
    chain_ids: &'a [BTreeSet<String>],
    has_ids: &'a [BTreeSet<String>],
}

/// What a search did, so a test can see that the work follows the page, not the matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Work {
    /// Rows the index range returned.
    pub rows: usize,
    /// Documents decoded from JSON text.
    pub decoded: usize,
    /// Documents read from storage one by one, for the page.
    pub fetched: usize,
    /// Rows the search parameters were evaluated against.
    pub checked: usize,
    /// Sort keys worked out: one for each match and sort term, however the matches compare.
    pub keyed: usize,
    /// Most matches held at once while the page was picked out of them.
    pub retained: usize,
}

/// What a search does with a parameter whose value it cannot read (a date that is not one, a
/// number that is not a number). FHIR lets a server refuse such a search or ignore the parameter,
/// and lets the client say which with `Prefer: handling=strict | lenient`.
///
/// Every search is strict unless the one that serves a client's `GET` asks otherwise, so a chain,
/// a conditional update or delete, a transaction entry and a live query are never lenient: setting
/// a parameter aside widens a result, and a conditional delete must not widen into a different
/// resource than the one the client described.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Handling {
    /// The search fails with `400`, naming the parameter and what was wrong with its value.
    Strict,
    /// The parameter is left out of the search, which runs on the rest. The bundle says so.
    Lenient,
}

/// A parameter a lenient search set aside, and why.
struct Ignored {
    /// The name as the client wrote it, modifier and chain included.
    key: String,
    /// The occurrence that was set aside: a parameter given more than once is a clause each, and
    /// only the one that cannot be read goes.
    occurrence: String,
    message: String,
}

pub fn search(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
) -> Result<Value, FhirError> {
    search_handling(tx, catalog, model, params, Handling::Strict)
}

/// `search`, for the request a client made, with the handling it asked for.
pub(crate) fn search_handling(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
    handling: Handling,
) -> Result<Value, FhirError> {
    search_counted(tx, catalog, model, params, handling).map(|(bundle, _)| bundle)
}

pub(crate) fn search_counted(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
    handling: Handling,
) -> Result<(Value, Work), FhirError> {
    catalog.deny_response_read(&model.name)?;
    if let Some(query) = params.get("_query") {
        return Err(FhirError::Invalid(format!("unknown query {query}")));
    }
    let (clauses, ignored) = clauses_from(catalog, model, params, handling)?;
    // The links, and the cursor in them, describe the search that ran, so a parameter that was set
    // aside is in neither, and a `_cursor` is checked against the same parameters it was made from.
    let ran = without_ignored(params, &ignored);
    let controls = controls_from(&ran)?;
    let chain_ids = chain_sets(tx, catalog, model, &clauses)?;
    let has_ids = has_sets(tx, catalog, model, &clauses)?;
    let scan = choose_index(model, &clauses);
    // Three costs used to grow with the number of matches, whatever `_count` asked for: reading and
    // decoding every document in the index range, checking the parameter against each one again,
    // and building and sorting the whole result before cutting it to the page. When the range
    // already decides every clause, only the ids are read and only the page's documents are
    // fetched; otherwise rows are decoded and checked one at a time (the clause the range decided
    // is not checked again), only the page is kept, and the scan stops once nothing more is needed.
    let (page, total, work) = if scan.decides_all(&clauses)
        && controls.compartments.is_empty()
        && controls.sort.is_empty()
        && controls.page_is_cheap_to_fetch()
    {
        page_from_index(tx, &scan, &controls)?
    } else {
        page_by_checking(
            tx,
            catalog,
            model,
            &scan,
            &Checks {
                clauses: &clauses,
                proven: scan.proven,
                chain_ids: &chain_ids,
                has_ids: &has_ids,
            },
            &controls,
        )?
    };
    let mut bundle = assemble(tx, catalog, model, &ran, &controls, page, total)?;
    // `_summary=count` has no entries to put the outcome in; its `self` link shows what ran.
    if controls.summary.as_deref() != Some("count") {
        report_ignored(&mut bundle, &ignored);
    }
    Ok((bundle, work))
}

fn without_ignored<'a>(
    params: &'a BTreeMap<String, String>,
    ignored: &[Ignored],
) -> Cow<'a, BTreeMap<String, String>> {
    if ignored.is_empty() {
        return Cow::Borrowed(params);
    }
    let mut ran = params.clone();
    for item in ignored {
        let Some(value) = ran.get(&item.key) else {
            continue;
        };
        // Only that occurrence goes: the others of the same parameter are clauses that were read.
        let mut kept: Vec<&str> = value.split(AND).collect();
        if let Some(at) = kept.iter().position(|text| *text == item.occurrence) {
            kept.remove(at);
        }
        if kept.is_empty() {
            ran.remove(&item.key);
        } else {
            let joined = kept.join(&AND.to_string());
            ran.insert(item.key.clone(), joined);
        }
    }
    Cow::Owned(ran)
}

/// Add to a search bundle the `OperationOutcome` that says which parameters were set aside. FHIR
/// gives an entry with mode `outcome` for this, and it is not a match, so it is never counted.
fn report_ignored(bundle: &mut Value, ignored: &[Ignored]) {
    if ignored.is_empty() {
        return;
    }
    let issues: Vec<Value> = ignored
        .iter()
        .map(|item| {
            json!({
                "severity": "warning",
                "code": "processing",
                "diagnostics": format!(
                    "{}. The parameter was ignored because the request asked for handling=lenient",
                    item.message
                ),
                "expression": [item.key]
            })
        })
        .collect();
    if let Some(entries) = bundle.get_mut("entry").and_then(Value::as_array_mut) {
        entries.push(json!({
            "resource": {"resourceType": "OperationOutcome", "issue": issues},
            "search": {"mode": "outcome"}
        }));
    }
}

/// The page of matches and how many there are, when the index range already decides every clause:
/// the range lists exactly the matching documents, in the order they are returned.
fn page_from_index(
    tx: &mut Transaction,
    scan: &Scan,
    controls: &Controls,
) -> Result<Page, FhirError> {
    let offset = controls.offset;
    // An exact total needs every row of the range counted. Without one, and with a page size, the
    // first rows past the page are enough (one more shows there is a next page), so the read stops
    // there instead of listing the whole range.
    let (rows, total) = match controls.count {
        Some(count) if !controls.needs_total() => {
            let (rows, more) = scan.first(tx, offset.saturating_add(count))?;
            let total = rows.len() + usize::from(more);
            (rows, total)
        }
        _ => {
            let rows = scan.entries(tx)?;
            let total = rows.len();
            (rows, total)
        }
    };
    // `total` is what the read listed: every row, or the page and the one that shows there is more.
    let mut work = Work {
        rows: total,
        ..Work::default()
    };
    let start = offset.min(rows.len());
    let end = offset
        .saturating_add(controls.count.unwrap_or(usize::MAX))
        .min(rows.len());
    let documents = controls.needs_documents();
    let mut page = Vec::with_capacity(end - start);
    for row in &rows[start..end] {
        let id = row.id.id_str().unwrap_or("").to_string();
        // `_summary=count` shows no resource, so there is nothing to fetch.
        let (json, ts) = if documents {
            work.fetched += 1;
            work.decoded += 1;
            tx.entry_json(row)?
        } else {
            (Value::Null, None)
        };
        page.push((id.clone(), decorate(json, &id, ts)));
    }
    Ok((page, total, work))
}

/// The page of matches and how many there are, checking the rows of the index range one by one.
///
/// Without `_sort` only the page is kept. Without a wanted total the scan ends one match after the
/// page, which is all `links` needs to know whether there is a next page.
///
/// With `_sort` every match has to be seen, since any of them may sort first, but not kept: a
/// match is given its sort key once, and only the `offset + count` that sort first are held (see
/// `Ranked`), so a small page costs the same memory and sorting work however many resources match.
fn page_by_checking(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    scan: &Scan,
    checks: &Checks<'_>,
    controls: &Controls,
) -> Result<Page, FhirError> {
    let rows = scan.text(tx)?;
    let mut work = Work {
        rows: rows.len(),
        ..Work::default()
    };
    let host = TxHost::new(tx, catalog);
    let sorted = !controls.sort.is_empty();
    let limit = controls.count.unwrap_or(usize::MAX);
    let stop_at = if sorted || controls.needs_total() {
        usize::MAX
    } else {
        controls.offset.saturating_add(limit).saturating_add(1)
    };
    // The matches that sort before the page, the page, and nothing after it.
    let wanted = controls.offset.saturating_add(limit);
    let mut kept = Vec::new();
    let mut ranked = Vec::new();
    let mut total = 0usize;
    for row in &rows {
        let json = row.json()?;
        work.decoded += 1;
        work.checked += 1;
        if !row_matches(catalog, model, &json, checks, controls, &host) {
            continue;
        }
        if sorted {
            if wanted > 0 {
                work.keyed += controls.sort.len();
                ranked.push(Ranked {
                    keys: sort_keys(catalog, model, &json, &controls.sort),
                    id: row.id.id_str().unwrap_or("").to_string(),
                    json,
                    ts: row.ts,
                });
                work.retained = work.retained.max(ranked.len());
                trim_ranked(&mut ranked, wanted, &controls.sort);
            }
        } else if total >= controls.offset && kept.len() < limit {
            let id = row.id.id_str().unwrap_or("").to_string();
            kept.push((id.clone(), decorate(json, &id, row.ts)));
        }
        total += 1;
        if total >= stop_at {
            break;
        }
    }
    if sorted {
        ranked.sort_by(|left, right| left.compare(right, &controls.sort));
        let page = ranked
            .into_iter()
            .skip(controls.offset)
            .take(limit)
            .map(|item| (item.id.clone(), decorate(item.json, &item.id, item.ts)))
            .collect();
        return Ok((page, total, work));
    }
    Ok((kept, total, work))
}

/// The search bundle for a page of matches.
fn assemble(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
    controls: &Controls,
    page: Vec<(String, Value)>,
    total: usize,
) -> Result<Value, FhirError> {
    let mut entries = Vec::new();
    for (id, resource) in &page {
        entries.push(json!({
            "resource": project_resource(resource, controls),
            "search": {"mode": "match"}
        }));
        let _ = id;
    }
    // `_summary=count` shows no entry, so it follows no `_include` or `_revinclude` either.
    if controls.summary.as_deref() != Some("count") {
        entries.extend(include_entries(tx, catalog, model, &page, controls)?);
    }
    let mut bundle = json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "entry": entries,
        "link": links(model, params, controls, page_len(&entries), total)
    });
    if controls.summary.as_deref() != Some("count") && controls.total != "none" {
        bundle["total"] = json!(total);
    } else if controls.summary.as_deref() == Some("count") {
        bundle["total"] = json!(total);
        bundle["entry"] = json!([]);
    }
    if controls.total == "none" && controls.summary.as_deref() != Some("count") {
        bundle.as_object_mut().expect("object").remove("total");
    }
    Ok(bundle)
}

/// The search as it was before the page was built lazily: every row decoded, every parameter
/// checked, every match copied and sorted, then cut to the page. Kept so tests can check that the
/// current search returns the same bundle and records the same reads.
#[cfg(test)]
pub(crate) fn search_reference(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
) -> Result<Value, FhirError> {
    catalog.deny_response_read(&model.name)?;
    if let Some(query) = params.get("_query") {
        return Err(FhirError::Invalid(format!("unknown query {query}")));
    }
    let (clauses, _) = clauses_from(catalog, model, params, Handling::Strict)?;
    let controls = controls_from(params)?;
    let chain_ids = chain_sets(tx, catalog, model, &clauses)?;
    let has_ids = has_sets(tx, catalog, model, &clauses)?;
    let scan = choose_index(model, &clauses);
    let mut rows = Vec::new();
    for interval in &scan.intervals {
        rows.extend(tx.index_range(&scan.index, interval.clone())?);
    }
    if scan.intervals.len() > 1 {
        rows.sort_by(|left, right| left.key.cmp(&right.key));
        let mut seen = BTreeSet::new();
        rows.retain(|row| seen.insert(row.id.id_str().unwrap_or("").to_string()));
    }
    let host = TxHost::new(tx, catalog);
    let mut matched = Vec::new();
    for row in rows {
        let checks = Checks {
            clauses: &clauses,
            proven: None,
            chain_ids: &chain_ids,
            has_ids: &has_ids,
        };
        if !row_matches(catalog, model, &row.json, &checks, &controls, &host) {
            continue;
        }
        let id = row.id.id_str().unwrap_or("").to_string();
        matched.push((
            id,
            decorate(row.json, row.id.id_str().unwrap_or(""), row.ts),
        ));
    }
    sort_matches(catalog, model, &mut matched, &controls.sort);
    let total = matched.len();
    let page: Vec<_> = matched
        .into_iter()
        .skip(controls.offset)
        .take(controls.count.unwrap_or(usize::MAX))
        .collect();
    assemble(tx, catalog, model, params, &controls, page, total)
}

fn page_len(entries: &[Value]) -> usize {
    entries
        .iter()
        .filter(|entry| {
            entry
                .pointer("/search/mode")
                .and_then(|value| value.as_str())
                == Some("match")
        })
        .count()
}

fn controls_from(params: &BTreeMap<String, String>) -> Result<Controls, FhirError> {
    let count = params
        .get("_count")
        .map(|text| parse_usize(text, "_count"))
        .transpose()?;
    let offset = page_offset(params)?;
    let sort = params
        .get("_sort")
        .map(|text| {
            text.split(',')
                .filter(|part| !part.is_empty())
                .map(|part| {
                    if let Some(name) = part.strip_prefix('-') {
                        (name.to_string(), true)
                    } else {
                        (part.trim_start_matches('+').to_string(), false)
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let elements = params
        .get("_elements")
        .map(|text| {
            text.split(',')
                .filter(|part| !part.is_empty())
                .map(|part| part.to_string())
                .collect()
        })
        .unwrap_or_default();
    let mut compartments = Vec::new();
    // `_compartment` is the caller's to choose. A value that is not `Type/id` names nothing.
    if let Some((type_name, id)) = params
        .get("_compartment")
        .and_then(|text| text.split_once('/'))
    {
        compartments.push(Within {
            type_name: type_name.to_string(),
            id: id.to_string(),
            limit: false,
        });
    }
    // The server's limit has to hold or the search must not run: ignoring a malformed one would
    // search without it.
    if let Some(text) = params.get(PATIENT_COMPARTMENT) {
        let (type_name, id) = text
            .split_once('/')
            .ok_or_else(|| FhirError::Invalid(format!("{PATIENT_COMPARTMENT} must be Type/id")))?;
        compartments.push(Within {
            type_name: type_name.to_string(),
            id: id.to_string(),
            limit: true,
        });
    }
    Ok(Controls {
        count,
        offset,
        sort,
        summary: params.get("_summary").cloned(),
        elements,
        total: params
            .get("_total")
            .cloned()
            .unwrap_or_else(|| "accurate".into()),
        include: include_specs(params, "_include")?,
        revinclude: include_specs(params, "_revinclude")?,
        compartments,
    })
}

fn parse_usize(text: &str, name: &str) -> Result<usize, FhirError> {
    text.parse()
        .map_err(|_| FhirError::Invalid(format!("{name} must be an integer")))
}

/// The rules of `_include` or `_revinclude` (`name`): those given under the plain name, and those
/// under `name:iterate`.
///
/// A reverse include cannot take `*`: finding what points at a resource through any parameter
/// means reading the index of every reference parameter of every resource type, for each match.
fn include_specs(
    params: &BTreeMap<String, String>,
    name: &str,
) -> Result<Vec<IncludeSpec>, FhirError> {
    let mut specs = Vec::new();
    for (key, iterate) in [(name.to_string(), false), (format!("{name}:iterate"), true)] {
        let Some(value) = params.get(&key) else {
            continue;
        };
        for part in value.split(',').filter(|part| !part.is_empty()) {
            let Some(spec) = parse_include(part, iterate) else {
                continue;
            };
            if name == "_revinclude" && spec.param == "*" {
                return Err(FhirError::Invalid(format!(
                    "{key}={part} is not supported: name the parameter that points at the match, \
                     for example Observation:subject"
                )));
            }
            specs.push(spec);
        }
    }
    Ok(specs)
}

fn parse_include(text: &str, iterate: bool) -> Option<IncludeSpec> {
    let text = text.trim();
    // A rule written `Type:param:iterate` has always been accepted, though it used to be treated
    // as a plain one. It means what the `:iterate` form of the parameter means.
    let (text, marked) = match text.strip_suffix(":iterate") {
        Some(rest) => (rest, true),
        None => (text, false),
    };
    let iterate = iterate || marked;
    if text == "*" {
        return Some(IncludeSpec {
            source: None,
            param: "*".into(),
            target: None,
            iterate,
        });
    }
    let mut parts = text.split(':');
    let first = parts.next()?.to_string();
    if first.is_empty() {
        return None;
    }
    let Some(param) = parts.next() else {
        return Some(IncludeSpec {
            source: None,
            param: first,
            target: None,
            iterate,
        });
    };
    if param.is_empty() {
        return None;
    }
    let target = parts
        .next()
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string());
    Some(IncludeSpec {
        source: Some(first),
        param: param.to_string(),
        target,
        iterate,
    })
}

/// Most characters of a value an error message repeats.
const ECHO_LIMIT: usize = 64;

/// The clauses of a search, and the parameters a lenient search set aside.
///
/// A value is read here, before any row is, so whether it is well formed never depends on what
/// is stored: a search of an empty table, and a live query before anything is written, refuse a
/// value exactly as a full table does.
fn clauses_from(
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
    handling: Handling,
) -> Result<(Vec<Clause>, Vec<Ignored>), FhirError> {
    let mut clauses = Vec::new();
    let mut ignored = Vec::new();
    // Whether the clause stays. A value that cannot be read ends a strict search; a lenient one
    // runs without the clause and says so.
    let mut keeps = |verdict: Result<(), (String, String)>, key: &str, occurrence: &str| {
        let Err((value, reason)) = verdict else {
            return Ok(true);
        };
        // A long value is not echoed whole: the message goes into the response, a live query's
        // error and the logs.
        let mut shown: String = value.chars().take(ECHO_LIMIT).collect();
        if shown.len() < value.len() {
            shown.push('…');
        }
        let message = format!(
            "invalid value {shown:?} for search parameter {key} on {}: {reason}",
            model.name
        );
        match handling {
            Handling::Strict => Err(FhirError::Invalid(message)),
            Handling::Lenient => {
                ignored.push(Ignored {
                    key: key.to_string(),
                    occurrence: occurrence.to_string(),
                    message,
                });
                Ok(false)
            }
        }
    };
    for (name, value) in params {
        if is_control(name) || name == "_query" {
            continue;
        }
        // A parameter given more than once is AND: each occurrence is a clause of its own.
        for occurrence in value.split(AND) {
            let clause = clause_from(model, name, occurrence)?;
            if keeps(check_clause(catalog, model, &clause), name, occurrence)? {
                clauses.push(clause);
            }
        }
    }
    Ok((clauses, ignored))
}

/// The first value of the clause that its parameter cannot read, and why. A comma between values
/// is OR, so each alternative is read on its own.
fn check_clause(
    catalog: &Catalog,
    model: &ResourceModel,
    clause: &Clause,
) -> Result<(), (String, String)> {
    match clause {
        Clause::Id(_) => Ok(()),
        // The syntax of a `_filter` was refused, and the parameters it names checked, with the clause.
        Clause::Filter(value) => {
            let Ok(filter) = parse_filter(value) else {
                return Ok(());
            };
            let mut comparisons = Vec::new();
            filter_comparisons(&filter, &mut comparisons);
            for (param, op, text) in comparisons {
                let Some(found) = model.ready_param(param) else {
                    continue;
                };
                let modifier = (op == "pr").then_some("missing");
                found
                    .extractor
                    .check_value(modifier, text)
                    .map_err(|reason| (text.to_string(), format!("in {param}: {reason}")))?;
            }
            Ok(())
        }
        Clause::Param {
            name,
            modifier,
            values,
        } => {
            let Some(param) = model.ready_param(name) else {
                return Ok(());
            };
            for value in values {
                param
                    .extractor
                    .check_value(modifier.as_deref(), value)
                    .map_err(|reason| (value.clone(), reason))?;
            }
            Ok(())
        }
        Clause::Chain {
            param,
            target_type,
            target_param,
            modifier,
            value,
        } => {
            let types = target_types(catalog, model, param, target_type.as_deref());
            reads_on(
                catalog,
                types.iter().map(String::as_str),
                target_param,
                modifier.as_deref(),
                value,
            )
        }
        Clause::Has {
            type_name,
            search_param,
            modifier,
            value,
            ..
        } => reads_on(
            catalog,
            [type_name.as_str()],
            search_param,
            modifier.as_deref(),
            value,
        ),
    }
}

/// Whether the parameter `code` of every one of `types` can read each alternative of `value`. A
/// type with no such parameter says nothing: the search over it refuses the name, as it does for
/// any parameter.
fn reads_on<'a>(
    catalog: &Catalog,
    types: impl IntoIterator<Item = &'a str>,
    code: &str,
    modifier: Option<&str>,
    value: &str,
) -> Result<(), (String, String)> {
    // The search over the type splits the value the same way, and refuses a list that is too long.
    let alternatives = if modifier == Some("missing") {
        vec![value.to_string()]
    } else {
        or_values(value).unwrap_or_else(|_| vec![value.to_string()])
    };
    for type_name in types {
        let Some(param) = catalog
            .get(type_name)
            .ok()
            .and_then(|model| model.ready_param(code))
        else {
            continue;
        };
        for alternative in &alternatives {
            param
                .extractor
                .check_value(modifier, alternative)
                .map_err(|reason| (alternative.clone(), reason))?;
        }
    }
    Ok(())
}

fn clause_from(model: &ResourceModel, name: &str, value: &str) -> Result<Clause, FhirError> {
    if name == "_id" {
        return Ok(Clause::Id(or_values(value)?));
    }
    if name == "_filter" {
        let filter = parse_filter(value).map_err(FhirError::Invalid)?;
        let mut comparisons = Vec::new();
        filter_comparisons(&filter, &mut comparisons);
        // A comparison with a parameter that does not exist used to match nothing.
        for (param, _, _) in comparisons {
            if model.ready_param(param).is_none() {
                return Err(FhirError::Invalid(format!(
                    "unsupported search parameter {param} on {}",
                    model.name
                )));
            }
        }
        check_filter_ids(model, &filter)?;
        return Ok(Clause::Filter(value.to_string()));
    }
    if let Some(clause) = parse_has(name, value) {
        return Ok(clause);
    }
    if let Some(clause) = parse_chain(name, value) {
        if let Clause::Chain { param, .. } = &clause {
            if model.ready_param(param).is_none() {
                return Err(FhirError::Invalid(format!(
                    "unsupported search parameter {param} on {}",
                    model.name
                )));
            }
        }
        return Ok(clause);
    }
    let (base, modifier) = split_modifier(name);
    let Some(param) = model.ready_param(&base) else {
        return Err(FhirError::Invalid(format!(
            "unsupported search parameter {name} on {}",
            model.name
        )));
    };
    // `:missing` takes `true` or `false`, which a comma cannot list.
    let values = if modifier.as_deref() == Some("missing") {
        vec![value.to_string()]
    } else {
        or_values(value)?
    };
    // Only a plain value or `:not` is an id to look for. A bare id is one range for each type it
    // can stand for, so the ranges a plain value reads are counted, not only its alternatives.
    if matches!(modifier.as_deref(), None | Some("not")) {
        let mut ranges = 0;
        for value in &values {
            match param.extractor.reference_id(value) {
                ReferenceId::Open(types) => return Err(open_reference(&base, value, types)),
                ReferenceId::Typed(tokens) => ranges += tokens.len(),
                ReferenceId::AsWritten => ranges += 1,
            }
        }
        if modifier.is_none() && ranges > MAX_ALTERNATIVES {
            return Err(FhirError::Invalid(format!(
                "reference parameter {base} would read {ranges} index ranges: an id counts once \
                 for every type the parameter can point at, and a search reads at most \
                 {MAX_ALTERNATIVES}"
            )));
        }
    }
    Ok(Clause::Param {
        name: base,
        modifier,
        values,
    })
}

/// Why a bare id cannot be searched for through a reference parameter, and what to send instead.
/// It is `Unsupported` (still a 400) so that a caller that runs searches on someone else's behalf,
/// as SDC population does, can tell it from a malformed query.
fn open_reference(name: &str, id: &str, types: usize) -> FhirError {
    let why = if types == 0 {
        "does not say which resource types it can point at".to_string()
    } else {
        format!("can point at any of {types} resource types")
    };
    FhirError::Unsupported(format!(
        "reference parameter {name} {why}, so the id {id} alone is not enough: use Type/{id}"
    ))
}

/// A `_filter` comparison reads an id as a plain parameter does, so it is refused the same way.
fn check_filter_ids(model: &ResourceModel, filter: &Filter) -> Result<(), FhirError> {
    match filter {
        Filter::And(parts) | Filter::Or(parts) => parts
            .iter()
            .try_for_each(|part| check_filter_ids(model, part)),
        Filter::Cmp { name, op, value } => {
            if let Some(param) = model
                .ready_param(name)
                .filter(|_| matches!(op.as_str(), "eq" | "ne"))
            {
                if let ReferenceId::Open(types) = param.extractor.reference_id(value) {
                    return Err(open_reference(name, value, types));
                }
            }
            Ok(())
        }
    }
}

fn is_control(name: &str) -> bool {
    matches!(
        name,
        "_format"
            | "_pretty"
            | "_count"
            | "_sort"
            | "_summary"
            | "_elements"
            | "_total"
            | "_offset"
            | "_cursor"
            | "_include"
            | "_include:iterate"
            | "_revinclude"
            | "_revinclude:iterate"
            | "_compartment"
            | PATIENT_COMPARTMENT
    )
}

fn parse_has(name: &str, value: &str) -> Option<Clause> {
    let rest = name.strip_prefix("_has:")?;
    let mut parts = rest.split(':');
    let type_name = parts.next()?.to_string();
    let ref_param = parts.next()?.to_string();
    let search_param = parts.next()?.to_string();
    let modifier = parts.next().map(|item| item.to_string());
    if type_name.is_empty() || ref_param.is_empty() || search_param.is_empty() {
        return None;
    }
    Some(Clause::Has {
        type_name,
        ref_param,
        search_param,
        modifier,
        value: value.to_string(),
    })
}

fn parse_chain(name: &str, value: &str) -> Option<Clause> {
    let (head, tail) = name.split_once('.')?;
    let (target_param, modifier) = split_modifier(tail);
    let (param, target_type) = if let Some((param, type_name)) = head.split_once(':') {
        if MODIFIERS.contains(&type_name) {
            return None;
        }
        (param.to_string(), Some(type_name.to_string()))
    } else {
        (head.to_string(), None)
    };
    Some(Clause::Chain {
        param,
        target_type,
        target_param,
        modifier,
        value: value.to_string(),
    })
}

fn split_modifier(name: &str) -> (String, Option<String>) {
    if let Some((base, modifier)) = name.rsplit_once(':') {
        if MODIFIERS.contains(&modifier) {
            return (base.to_string(), Some(modifier.to_string()));
        }
    }
    (name.to_string(), None)
}

/// Where `search` reads, and what that read already settles.
struct Scan {
    index: IndexId,
    /// The ranges of `index` that hold every match: one for a single value, one per alternative
    /// when a comma lists several (they may overlap, and one document may be in several).
    intervals: Vec<Interval>,
    /// The clause whose match every row of `intervals` already proves, if there is one.
    proven: Option<usize>,
}

impl Scan {
    /// Whether the ranges decide every clause, so no row needs checking.
    fn decides_all(&self, clauses: &[Clause]) -> bool {
        (0..clauses.len()).all(|position| self.proven == Some(position))
    }

    /// The rows of the ranges, without their documents, in key order and each document once.
    fn entries(&self, tx: &mut Transaction) -> Result<Vec<ScannedEntry>, FhirError> {
        let mut lists = Vec::with_capacity(self.intervals.len());
        for interval in &self.intervals {
            lists.push(tx.index_range_entries(&self.index, interval.clone())?);
        }
        Ok(in_key_order(lists))
    }

    /// The first `wanted` rows `entries` lists, and whether the ranges hold more. Each range is read
    /// no further than `wanted` rows, which is enough: a row among the first `wanted` of the merged
    /// list is among the first `wanted` of its own range, and one more row anywhere shows there is
    /// more.
    fn first(
        &self,
        tx: &mut Transaction,
        wanted: usize,
    ) -> Result<(Vec<ScannedEntry>, bool), FhirError> {
        let mut lists = Vec::with_capacity(self.intervals.len());
        let mut more = false;
        for interval in &self.intervals {
            let (rows, extra) = tx.index_range_first(&self.index, interval.clone(), wanted)?;
            more |= extra;
            lists.push(rows);
        }
        let mut rows = in_key_order(lists);
        more |= rows.len() > wanted;
        rows.truncate(wanted);
        Ok((rows, more))
    }

    /// The rows of the ranges, each with its document as JSON text, in key order and each
    /// document once.
    fn text(&self, tx: &mut Transaction) -> Result<Vec<ScannedText>, FhirError> {
        let mut lists = Vec::with_capacity(self.intervals.len());
        for interval in &self.intervals {
            lists.push(tx.index_range_text(&self.index, interval.clone())?);
        }
        Ok(in_key_order(lists))
    }
}

/// The rows of several ranges of one index as one list. Each range is in key order and holds a
/// document once, so one range is returned as it is; several are merged in key order, and a
/// document found by more than one value is kept where its first key puts it.
fn in_key_order<P>(mut lists: Vec<Vec<ScannedRow<P>>>) -> Vec<ScannedRow<P>> {
    if lists.len() == 1 {
        return lists.pop().expect("one list");
    }
    let mut rows: Vec<ScannedRow<P>> = lists.into_iter().flatten().collect();
    rows.sort_by(|left, right| left.key.cmp(&right.key));
    let mut seen = BTreeSet::new();
    rows.retain(|row| seen.insert(row.id.id_str().unwrap_or("").to_string()));
    rows
}

/// The index ranges a probe narrows to (one, except for a bare id, which is one for each type it
/// can stand for), and how wide they are likely to be (lower is narrower).
fn probe_ranges(probe: Probe<'_>) -> Option<(u8, Vec<Interval>)> {
    Some(match probe {
        Probe::Token(text) => (0, vec![token_interval(text)]),
        Probe::Tokens(tokens) => (0, tokens.iter().map(|text| token_interval(text)).collect()),
        Probe::Prefix(text) => (1, vec![string_interval(text)]),
        Probe::Dates(intervals) => (2, intervals),
        Probe::Range(interval) => (2, vec![interval]),
        Probe::Scan => return None,
    })
}

fn choose_index(model: &ResourceModel, clauses: &[Clause]) -> Scan {
    for clause in clauses {
        if let Clause::Id(ids) = clause {
            return Scan {
                index: model.by_id(),
                intervals: ids
                    .iter()
                    .map(|id| Interval::point(exact_key(id)))
                    .collect(),
                proven: None,
            };
        }
    }
    struct Candidate {
        rank: u8,
        name: String,
        intervals: Vec<Interval>,
        position: usize,
        proven: bool,
    }
    let mut chosen: Option<Candidate> = None;
    for (position, clause) in clauses.iter().enumerate() {
        let Clause::Param {
            name,
            modifier,
            values,
        } = clause
        else {
            continue;
        };
        if modifier.as_deref().is_some_and(|item| WIDE.contains(&item)) {
            continue;
        }
        let Some(param) = model.ready_param(name) else {
            continue;
        };
        // Every alternative needs a range, or the clause cannot narrow the read at all.
        let Some(ranges) = values
            .iter()
            .map(|value| probe_ranges(param.extractor.probe(value)))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let rank = ranges.iter().map(|(rank, _)| *rank).max().unwrap_or(0);
        let intervals: Vec<Interval> = ranges
            .into_iter()
            .flat_map(|(_, intervals)| intervals)
            .collect();
        // Ties go to the clause with fewer ranges to read.
        let replace = chosen
            .as_ref()
            .map(|prev| (rank, intervals.len()) < (prev.rank, prev.intervals.len()))
            .unwrap_or(true);
        if replace {
            // A modifier changes what a match is, so only a plain parameter can be proven.
            let proven = modifier.is_none()
                && values
                    .iter()
                    .all(|value| param.extractor.index_proves(value));
            chosen = Some(Candidate {
                rank,
                name: name.clone(),
                intervals,
                position,
                proven,
            });
        }
    }
    if let Some(Candidate {
        name,
        intervals,
        position,
        proven,
        ..
    }) = chosen
    {
        return Scan {
            index: model.search_index(&name),
            intervals,
            proven: proven.then_some(position),
        };
    }
    Scan {
        index: model.by_id(),
        intervals: vec![Interval::all()],
        proven: None,
    }
}

fn chain_sets(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    clauses: &[Clause],
) -> Result<Vec<BTreeSet<String>>, FhirError> {
    let mut sets = Vec::new();
    for clause in clauses {
        let Clause::Chain {
            param,
            target_type,
            target_param,
            modifier,
            value,
        } = clause
        else {
            sets.push(BTreeSet::new());
            continue;
        };
        let types = target_types(catalog, model, param, target_type.as_deref());
        let mut ids = BTreeSet::new();
        for type_name in types {
            let Ok(target) = catalog.get(&type_name) else {
                continue;
            };
            let mut params = BTreeMap::new();
            let key = match modifier {
                Some(modifier) => format!("{target_param}:{modifier}"),
                None => target_param.clone(),
            };
            params.insert(key, value.clone());
            let bundle = search(tx, catalog, target, &params)?;
            for entry in bundle
                .get("entry")
                .and_then(|value| value.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(id) = entry.pointer("/resource/id").and_then(|item| item.as_str()) {
                    ids.insert(format!("{type_name}/{id}"));
                    ids.insert(id.to_string());
                }
            }
        }
        sets.push(ids);
    }
    Ok(sets)
}

fn has_sets(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    clauses: &[Clause],
) -> Result<Vec<BTreeSet<String>>, FhirError> {
    let mut sets = Vec::new();
    for clause in clauses {
        let Clause::Has {
            type_name,
            ref_param,
            search_param,
            modifier,
            value,
        } = clause
        else {
            sets.push(BTreeSet::new());
            continue;
        };
        let Ok(other) = catalog.get(type_name) else {
            sets.push(BTreeSet::new());
            continue;
        };
        let mut params = BTreeMap::new();
        let key = match modifier {
            Some(modifier) => format!("{search_param}:{modifier}"),
            None => search_param.clone(),
        };
        params.insert(key, value.clone());
        let bundle = search(tx, catalog, other, &params)?;
        let mut ids = BTreeSet::new();
        let Some(param) = other.ready_param(ref_param) else {
            sets.push(ids);
            continue;
        };
        for entry in bundle
            .get("entry")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
        {
            let Some(resource) = entry.get("resource") else {
                continue;
            };
            let host = TxHost::new(tx, catalog);
            for reference in param
                .extractor
                .indexed(resource, catalog.path_model(), &host)
            {
                let Some((type_name, id)) = reference.rsplit_once('/') else {
                    continue;
                };
                if type_name == model.name || type_name.ends_with(&format!("/{}", model.name)) {
                    ids.insert(id.to_string());
                }
            }
        }
        sets.push(ids);
    }
    Ok(sets)
}

fn target_types(
    catalog: &Catalog,
    model: &ResourceModel,
    param: &str,
    explicit: Option<&str>,
) -> Vec<String> {
    if let Some(type_name) = explicit {
        return vec![type_name.to_string()];
    }
    if let Some(element) = model.references.iter().find(|item| item.path == param) {
        if !element.targets.is_empty() {
            return element.targets.clone();
        }
    }
    catalog
        .resource_models()
        .map(|item| item.name.clone())
        .collect()
}

/// Whether `resource` satisfies every clause, except the one at `proven` (if any), which the index
/// range the resource was found through has already settled.
fn row_matches(
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    checks: &Checks<'_>,
    controls: &Controls,
    host: &TxHost<'_>,
) -> bool {
    let Checks {
        clauses,
        proven,
        chain_ids,
        has_ids,
    } = *checks;
    for within in &controls.compartments {
        if !in_compartment(
            catalog,
            model,
            resource,
            &within.type_name,
            &within.id,
            host,
        ) {
            return false;
        }
    }
    clauses.iter().enumerate().all(|(index, clause)| {
        proven == Some(index)
            || match clause {
                Clause::Id(ids) => resource
                    .get("id")
                    .and_then(|item| item.as_str())
                    .is_some_and(|id| ids.iter().any(|wanted| wanted == id)),
                Clause::Filter(value) => eval_filter(value, catalog, model, resource, host),
                Clause::Param {
                    name,
                    modifier,
                    values,
                } => param_matches(
                    catalog,
                    model,
                    resource,
                    name,
                    modifier.as_deref(),
                    values,
                    host,
                ),
                Clause::Chain { param, .. } => {
                    let Some(search) = model.ready_param(param) else {
                        return false;
                    };
                    let ids = &chain_ids[index];
                    search
                        .extractor
                        .indexed(resource, catalog.path_model(), host)
                        .iter()
                        .any(|reference| {
                            ids.contains(reference)
                                || reference
                                    .rsplit_once('/')
                                    .is_some_and(|(_, id)| ids.contains(id))
                        })
                }
                Clause::Has { .. } => resource
                    .get("id")
                    .and_then(|item| item.as_str())
                    .is_some_and(|id| has_ids[index].contains(id)),
            }
    })
}

/// Whether `resource` matches the parameter: any of the alternatives of `values` (a comma is OR)
/// has to match. With `:not` and `:not-in` it is the other way round, so a comma excludes every
/// value it lists: `gender:not=male,female` finds what is neither. The resource is read once, not
/// once per alternative.
fn param_matches(
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    name: &str,
    modifier: Option<&str>,
    values: &[String],
    host: &TxHost<'_>,
) -> bool {
    let Some(param) = model.ready_param(name) else {
        return false;
    };
    match modifier {
        None => param
            .extractor
            .matches_any(resource, values, catalog.path_model(), host),
        Some(modifier @ ("above" | "below" | "in" | "not-in")) => {
            let tokens = param
                .extractor
                .indexed(resource, catalog.path_model(), host);
            if modifier == "not-in" {
                values
                    .iter()
                    .all(|value| hierarchy_match(catalog, &tokens, modifier, value))
            } else {
                values
                    .iter()
                    .any(|value| hierarchy_match(catalog, &tokens, modifier, value))
            }
        }
        Some(modifier) => param.extractor.matches_modifier_any(
            resource,
            modifier,
            values,
            catalog.path_model(),
            host,
        ),
    }
}

fn hierarchy_match(catalog: &Catalog, tokens: &[String], modifier: &str, expected: &str) -> bool {
    let terms = catalog.terms();
    match modifier {
        "in" => tokens.iter().any(|token| in_set(terms, token, expected)),
        "not-in" => tokens.iter().all(|token| !in_set(terms, token, expected)),
        "above" => tokens.iter().any(|token| {
            let (system, code) = token_parts(token);
            let (query_system, query_code) = token_parts(expected);
            let system = system.or(query_system);
            match (system, code, query_code) {
                (Some(system), Some(code), Some(query)) => {
                    terms.subsumes(system, None, code, query)
                }
                _ => false,
            }
        }),
        "below" => tokens.iter().any(|token| {
            let (system, code) = token_parts(token);
            let (query_system, query_code) = token_parts(expected);
            let system = system.or(query_system);
            match (system, code, query_code) {
                (Some(system), Some(code), Some(query)) => {
                    terms.subsumes(system, None, query, code)
                }
                _ => false,
            }
        }),
        _ => false,
    }
}

fn in_set(terms: &refhir_validate::MemoryTerminology, token: &str, value_set: &str) -> bool {
    let (system, code) = token_parts(token);
    let Some(code) = code else {
        return false;
    };
    matches!(terms.contains(value_set, system, code), TermAnswer::Yes)
}

fn token_parts(token: &str) -> (Option<&str>, Option<&str>) {
    if let Some((system, code)) = token.split_once('|') {
        (Some(system), Some(code))
    } else if token.is_empty() {
        (None, None)
    } else {
        (None, Some(token))
    }
}

pub fn resource_in_compartment(
    catalog: &Catalog,
    tx: &mut Transaction,
    resource: &Value,
    compartment_type: &str,
    compartment_id: &str,
) -> bool {
    let Some(type_name) = resource
        .get("resourceType")
        .and_then(|value| value.as_str())
    else {
        return false;
    };
    let Ok(model) = catalog.get(type_name) else {
        return false;
    };
    let host = TxHost::new(tx, catalog);
    in_compartment(
        catalog,
        model,
        resource,
        compartment_type,
        compartment_id,
        &host,
    )
}

fn in_compartment(
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    type_name: &str,
    id: &str,
    host: &TxHost<'_>,
) -> bool {
    if model.name == *type_name {
        return resource.get("id").and_then(|item| item.as_str()) == Some(id);
    }
    let Some(compartment) = catalog.compartment(type_name) else {
        return false;
    };
    let Some(params) = compartment.params_for(&model.name) else {
        return false;
    };
    let token = format!("{type_name}/{id}");
    params.iter().any(|name| {
        model.ready_param(name).is_some_and(|param| {
            param
                .extractor
                .indexed(resource, catalog.path_model(), host)
                .iter()
                .any(|reference| reference == &token || reference.ends_with(&format!("/{token}")))
        })
    })
}

/// Sorts matches by comparing sort keys worked out for every comparison, as a search once did. The
/// reference search keeps it, so a test can hold the current one to the same order.
#[cfg(test)]
fn sort_matches(
    catalog: &Catalog,
    model: &ResourceModel,
    rows: &mut [(String, Value)],
    sort: &[(String, bool)],
) {
    if sort.is_empty() {
        return;
    }
    rows.sort_by(|left, right| {
        for (name, desc) in sort {
            let ordering = sort_key(catalog, model, &left.1, name)
                .cmp(&sort_key(catalog, model, &right.1, name));
            let ordering = if *desc { ordering.reverse() } else { ordering };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        left.0.cmp(&right.0)
    });
}

fn sort_key(catalog: &Catalog, model: &ResourceModel, resource: &Value, name: &str) -> String {
    let Some(param) = model.ready_param(name) else {
        return String::new();
    };
    let host = SortHost;
    param
        .extractor
        .indexed(resource, catalog.path_model(), &host)
        .into_iter()
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

/// The sort keys of a resource, one for each `_sort` term.
fn sort_keys(
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    sort: &[(String, bool)],
) -> Vec<String> {
    sort.iter()
        .map(|(name, _)| sort_key(catalog, model, resource, name))
        .collect()
}

/// A match held while a sorted page is picked out of the matches, with the keys it sorts by. The
/// keys are worked out once: evaluating the parameter's FHIRPath expression is the expensive part,
/// and a comparison sort that did it for every comparison did it about twenty times for each match.
struct Ranked {
    keys: Vec<String>,
    id: String,
    json: Value,
    ts: Option<Timestamp>,
}

impl Ranked {
    /// The order the `_sort` terms give, each ascending or descending, and the id for a tie.
    fn compare(&self, other: &Self, sort: &[(String, bool)]) -> std::cmp::Ordering {
        for ((_, desc), (left, right)) in sort.iter().zip(self.keys.iter().zip(&other.keys)) {
            let ordering = left.cmp(right);
            let ordering = if *desc { ordering.reverse() } else { ordering };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        self.id.cmp(&other.id)
    }
}

/// Drop what cannot be on the page: all but the `wanted` that sort first, once twice that many are
/// held. Pruning that late makes the work for a match a constant, since the part of `ranked` that
/// is kept is found without sorting it, and every match that is dropped was beaten by `wanted`.
fn trim_ranked(ranked: &mut Vec<Ranked>, wanted: usize, sort: &[(String, bool)]) {
    // Not worth a pass for a page of a few: sorting a few dozen costs nothing.
    let limit = wanted.saturating_mul(2).max(64);
    if ranked.len() < limit || wanted >= ranked.len() {
        return;
    }
    ranked.select_nth_unstable_by(wanted, |left, right| left.compare(right, sort));
    ranked.truncate(wanted);
}

struct SortHost;

impl refhir_fhirpath::Host for SortHost {}

fn project_resource(resource: &Value, controls: &Controls) -> Value {
    match controls.summary.as_deref() {
        Some("count") => json!({}),
        Some("text") => {
            let mut copy = json!({
                "resourceType": resource.get("resourceType"),
                "id": resource.get("id"),
                "meta": resource.get("meta"),
            });
            if let Some(text) = resource.get("text") {
                copy["text"] = text.clone();
            }
            copy
        }
        Some("data") => {
            let mut copy = resource.clone();
            if let Some(object) = copy.as_object_mut() {
                object.remove("text");
            }
            copy
        }
        Some("true") => {
            let mut copy = resource.clone();
            if let Some(object) = copy.as_object_mut() {
                object.remove("text");
                object.remove("extension");
                object.remove("modifierExtension");
            }
            copy
        }
        _ if !controls.elements.is_empty() => {
            let Some(object) = resource.as_object() else {
                return resource.clone();
            };
            let mut copy = serde_json::Map::new();
            for name in ["resourceType", "id", "meta"] {
                if let Some(value) = object.get(name) {
                    copy.insert(name.to_string(), value.clone());
                }
            }
            for name in &controls.elements {
                if let Some(value) = object.get(name) {
                    copy.insert(name.clone(), value.clone());
                }
            }
            Value::Object(copy)
        }
        _ => resource.clone(),
    }
}

/// Most resources the `:iterate` rules may add to one search. A round of an iterating rule can add
/// more than the round before, so a rule that follows every reference (`*`), or finds everything
/// that points at a resource, can reach much of a database. A search that would pass this is
/// refused, not answered with part of what it asked for.
const MAX_ITERATED: usize = 1000;

#[cfg(test)]
thread_local! {
    static ITERATED_CAP: std::cell::Cell<usize> =
        const { std::cell::Cell::new(MAX_ITERATED) };
}

/// Lowers the cap for the rest of the test (or until it is dropped), so a test reaches it without
/// creating a thousand resources.
#[cfg(test)]
pub(crate) struct LoweredCap;

#[cfg(test)]
impl LoweredCap {
    pub(crate) fn to(cap: usize) -> Self {
        ITERATED_CAP.with(|current| current.set(cap));
        Self
    }
}

#[cfg(test)]
impl Drop for LoweredCap {
    fn drop(&mut self) {
        ITERATED_CAP.with(|current| current.set(MAX_ITERATED));
    }
}

fn max_iterated() -> usize {
    #[cfg(test)]
    {
        ITERATED_CAP.with(|cap| cap.get())
    }
    #[cfg(not(test))]
    {
        MAX_ITERATED
    }
}

fn too_many_iterated() -> FhirError {
    FhirError::Invalid(format!(
        "the :iterate rules would reach more than {} resources: name the parameters to follow \
         instead of *, or search for fewer resources",
        max_iterated()
    ))
}

/// The resources the rules of one pass add. `room` is how many it may hold, and a pass that would
/// hold more is refused where it reads them, not after it has read them all. Only the `:iterate`
/// rules have one: they follow references out of resources the search did not match.
struct Added {
    found: Vec<Value>,
    room: Option<usize>,
}

impl Added {
    fn new(room: Option<usize>) -> Self {
        Self {
            found: Vec::new(),
            room,
        }
    }

    fn push(&mut self, resource: Value) -> Result<(), FhirError> {
        self.found.push(resource);
        match self.room {
            Some(room) if self.found.len() > room => Err(too_many_iterated()),
            _ => Ok(()),
        }
    }
}

/// What this server's search does that a client may not expect, and what it does not do: the
/// `documentation` of the CapabilityStatement's `rest`. R4 has no element that advertises
/// `:iterate`, or the limits of a comma or an escape, so a client reads it here.
pub(crate) fn capability_notes() -> String {
    format!(
        "Search: a comma between the values of one parameter is OR (`status=free,busy`), \
         and `\\,` is a literal comma; a value lists at most {MAX_ALTERNATIVES} alternatives. \
         `:not` and `:not-in` with several values leave out every one of them, and `:missing` \
         takes one value. A parameter given more than once is AND \
         (`date=ge2026-01-01&date=lt2026-02-01`) in a query string, a Bundle entry and \
         Subscription criteria, each occurrence judged on its own so one resource can satisfy \
         both through different elements of a repeating one; a live query gives the occurrences \
         as an array of strings, and a GraphQL argument has one value per name. A search GET \
         that names no `_count` returns one page with a `next` link, of a size the server sets. \
         `_include` and `_revinclude` take `Type:param` or `Type:param:TargetType`, once per \
         occurrence or comma-separated; `_include:iterate` and `_revinclude:iterate` apply a rule \
         to the matches and to every resource the rules have added, until none is added, and a \
         search that would add more than {MAX_ITERATED} resources that way is refused with a 400. \
         A value that cannot be read as its parameter's type (a date that is not one) is a 400, and \
         `Prefer: handling=lenient` on a search GET leaves that parameter out instead and says so \
         in the Bundle. \
         Not supported: `_revinclude=*` and `_revinclude=Type:*` (a 400), and the escapes `\\$` \
         and `\\|` in a search value (they are not interpreted)."
    )
}

/// The resources `_include` and `_revinclude` add to a page, as bundle entries.
///
/// The plain rules are applied to the matches. The `:iterate` rules are applied to the matches and
/// to everything the plain rules added, then to what those added, until a round adds nothing. A
/// resource is added once, and never when it is a match already, so references that lead back to
/// where they started end the search.
fn include_entries(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    page: &[(String, Value)],
    controls: &Controls,
) -> Result<Vec<Value>, FhirError> {
    let mut seen: BTreeSet<String> = page
        .iter()
        .map(|(id, _)| format!("{}/{}", model.name, id))
        .collect();
    let matches: Vec<&Value> = page.iter().map(|(_, resource)| resource).collect();
    let mut plain = Added::new(None);
    apply_rules(
        tx, catalog, controls, &mut seen, &matches, false, &mut plain,
    )?;
    let mut added = plain.found;
    if controls
        .include
        .iter()
        .chain(&controls.revinclude)
        .any(|spec| spec.iterate)
    {
        let mut fresh = Added::new(Some(max_iterated()));
        let sources: Vec<&Value> = matches.iter().copied().chain(added.iter()).collect();
        apply_rules(tx, catalog, controls, &mut seen, &sources, true, &mut fresh)?;
        let mut iterated = 0;
        while !fresh.found.is_empty() {
            iterated += fresh.found.len();
            let from = added.len();
            added.append(&mut fresh.found);
            fresh.room = Some(max_iterated() - iterated);
            let sources: Vec<&Value> = added[from..].iter().collect();
            apply_rules(tx, catalog, controls, &mut seen, &sources, true, &mut fresh)?;
        }
    }
    Ok(added
        .into_iter()
        .map(|resource| json!({"resource": resource, "search": {"mode": "include"}}))
        .collect())
}

/// Applies the plain rules (`iterating` false) or the `:iterate` rules to each of `sources`, and
/// puts the resources they add into `into`: what the `_include` rules add first, then what the
/// `_revinclude` rules add.
fn apply_rules(
    tx: &mut Transaction,
    catalog: &Catalog,
    controls: &Controls,
    seen: &mut BTreeSet<String>,
    sources: &[&Value],
    iterating: bool,
    into: &mut Added,
) -> Result<(), FhirError> {
    for resource in sources {
        for spec in controls
            .include
            .iter()
            .filter(|spec| spec.iterate == iterating)
        {
            push_forward(tx, catalog, controls, resource, spec, into, seen)?;
        }
    }
    for resource in sources {
        for spec in controls
            .revinclude
            .iter()
            .filter(|spec| spec.iterate == iterating)
        {
            push_reverse(tx, catalog, controls, resource, spec, into, seen)?;
        }
    }
    Ok(())
}

/// Whether a resource a rule would add is outside the compartment the server limited the caller
/// to. The limit holds for the types that belong to that compartment; a type that does not belong
/// to it (a Practitioner, for a patient's compartment) has never been kept out of an include.
fn outside_limit(
    tx: &mut Transaction,
    catalog: &Catalog,
    controls: &Controls,
    model: &ResourceModel,
    resource: &Value,
) -> bool {
    let Some(Within { type_name, id, .. }) =
        controls.compartments.iter().find(|within| within.limit)
    else {
        return false;
    };
    let belongs = model.name == *type_name
        || catalog
            .compartment(type_name)
            .is_some_and(|compartment| compartment.params_for(&model.name).is_some());
    if !belongs {
        return false;
    }
    let host = TxHost::new(tx, catalog);
    !in_compartment(catalog, model, resource, type_name, id, &host)
}

/// `Type/id` of a resource, as the references that point at it are written.
fn identity(resource: &Value) -> Option<(&str, &str)> {
    Some((
        resource.get("resourceType")?.as_str()?,
        resource.get("id")?.as_str()?,
    ))
}

/// Follows the reference parameter of `spec` out of `resource` (`_include`), adding what it points
/// at.
fn push_forward(
    tx: &mut Transaction,
    catalog: &Catalog,
    controls: &Controls,
    resource: &Value,
    spec: &IncludeSpec,
    into: &mut Added,
    seen: &mut BTreeSet<String>,
) -> Result<(), FhirError> {
    let Some((source_type, _)) = identity(resource) else {
        return Ok(());
    };
    if spec
        .source
        .as_deref()
        .is_some_and(|source| source != "*" && source != source_type)
    {
        return Ok(());
    }
    let Ok(source) = catalog.get(source_type) else {
        return Ok(());
    };
    let params: Vec<&crate::catalog::SearchParam> = if spec.param == "*" {
        source
            .params
            .iter()
            .filter(|param| {
                param.status == refhir_persistence::ParamStatus::Ready
                    && param.kind == crate::extract::SearchKind::Reference
            })
            .collect()
    } else {
        source.ready_param(&spec.param).into_iter().collect()
    };
    let mut references = Vec::new();
    {
        let host = TxHost::new(tx, catalog);
        for param in &params {
            references.extend(
                param
                    .extractor
                    .indexed(resource, catalog.path_model(), &host),
            );
        }
    }
    for reference in references {
        let Some((type_name, id)) = reference.rsplit_once('/') else {
            continue;
        };
        let type_name = type_name.rsplit('/').next().unwrap_or(type_name);
        if spec
            .target
            .as_deref()
            .is_some_and(|target| target != type_name)
        {
            continue;
        }
        if catalog.deny_response_read(type_name).is_err() {
            continue;
        }
        if !seen.insert(format!("{type_name}/{id}")) {
            continue;
        }
        let Ok(target) = catalog.get(type_name) else {
            continue;
        };
        if let Some(stored) = tx.get(&target.tablet(), id, &target.by_id())? {
            let included = decorate(stored.json, id, stored.ts);
            if !outside_limit(tx, catalog, controls, target, &included) {
                into.push(included)?;
            }
        }
    }
    Ok(())
}

/// The documents that have a key for `token` in `index`, as their id, JSON and revision timestamp.
/// With a `room`, the range is read no further than one row past it and a range that holds more is
/// refused, so a resource that millions point at costs a few reads, not all of them.
fn referencing(
    tx: &mut Transaction,
    index: &IndexId,
    token: &str,
    room: Option<usize>,
) -> Result<Vec<(String, Value, Option<Timestamp>)>, FhirError> {
    let interval = token_interval(token);
    let Some(room) = room else {
        return Ok(tx
            .index_range(index, interval)?
            .into_iter()
            .map(|row| (row.id.id_str().unwrap_or("").to_string(), row.json, row.ts))
            .collect());
    };
    let (rows, _) = tx.index_range_first(index, interval, room.saturating_add(1))?;
    if rows.len() > room {
        return Err(too_many_iterated());
    }
    let mut documents = Vec::with_capacity(rows.len());
    for row in &rows {
        let (json, ts) = tx.entry_json(row)?;
        documents.push((row.id.id_str().unwrap_or("").to_string(), json, ts));
    }
    Ok(documents)
}

/// Finds what points at `resource` through the reference parameter of `spec` (`_revinclude`), and
/// adds it. A target type in the rule (`Provenance:target:Observation`) restricts it to references
/// to resources of that type.
fn push_reverse(
    tx: &mut Transaction,
    catalog: &Catalog,
    controls: &Controls,
    resource: &Value,
    spec: &IncludeSpec,
    into: &mut Added,
    seen: &mut BTreeSet<String>,
) -> Result<(), FhirError> {
    let Some((type_name, id)) = identity(resource) else {
        return Ok(());
    };
    if spec
        .target
        .as_deref()
        .is_some_and(|target| target != type_name)
    {
        return Ok(());
    }
    let token = format!("{type_name}/{id}");
    for other in catalog.resource_models() {
        if spec
            .source
            .as_deref()
            .is_some_and(|source| source != "*" && source != other.name)
        {
            continue;
        }
        if catalog.deny_response_read(&other.name).is_err() {
            continue;
        }
        let Some(param) = other.ready_param(&spec.param) else {
            continue;
        };
        let documents = referencing(tx, &other.search_index(&spec.param), &token, into.room)?;
        for (other_id, json, ts) in documents {
            if !param
                .extractor
                .indexed(&json, catalog.path_model(), &TxHost::new(tx, catalog))
                .iter()
                .any(|reference| reference == &token)
            {
                continue;
            }
            if !seen.insert(format!("{}/{}", other.name, other_id)) {
                continue;
            }
            let included = decorate(json, &other_id, ts);
            if !outside_limit(tx, catalog, controls, other, &included) {
                into.push(included)?;
            }
        }
    }
    Ok(())
}

fn page_offset(params: &BTreeMap<String, String>) -> Result<usize, FhirError> {
    if let Some(token) = params.get("_cursor") {
        let Some((offset, canonical)) = decode_cursor(token) else {
            return Err(FhirError::Invalid("invalid _cursor".into()));
        };
        if canonical != cursor_canonical(params) {
            return Err(FhirError::Invalid(
                "cursor does not match this search".into(),
            ));
        }
        return Ok(offset);
    }
    params
        .get("_offset")
        .map(|text| parse_usize(text, "_offset"))
        .transpose()
        .map(|offset| offset.unwrap_or(0))
}

fn cursor_canonical(params: &BTreeMap<String, String>) -> String {
    let mut canonical = String::new();
    for (key, value) in params {
        if key == "_cursor" || key == "_offset" || is_compartment_param(key) {
            continue;
        }
        canonical.push_str(key);
        canonical.push('=');
        canonical.push_str(value);
        canonical.push('\n');
    }
    canonical
}

fn encode_cursor(offset: usize, params: &BTreeMap<String, String>) -> String {
    base64url(format!("{offset}\n{}", cursor_canonical(params)).as_bytes())
}

fn decode_cursor(token: &str) -> Option<(usize, String)> {
    let bytes = base64url_decode(token)?;
    let text = String::from_utf8(bytes).ok()?;
    let (offset, canonical) = text.split_once('\n')?;
    Some((offset.parse().ok()?, canonical.to_string()))
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 63) as usize] as char);
        }
    }
    out
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let n0 = value(*chunk.first()?)?;
        let n1 = value(*chunk.get(1)?)?;
        let n2 = chunk.get(2).copied().and_then(value);
        let n3 = chunk.get(3).copied().and_then(value);
        out.push((n0 << 2) | (n1 >> 4));
        if let Some(n2) = n2 {
            out.push((n1 << 4) | (n2 >> 2));
            if let Some(n3) = n3 {
                out.push((n2 << 6) | n3);
            }
        }
    }
    Some(out)
}

fn links(
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
    controls: &Controls,
    page_len: usize,
    total: usize,
) -> Value {
    let mut self_params = params.clone();
    self_params.remove("_offset");
    self_params.remove("_cursor");
    let self_url = query_url(&model.name, &self_params, controls.offset);
    let mut links = vec![json!({"relation": "self", "url": self_url})];
    if let Some(count) = controls.count {
        let filled = count > 0 && page_len == count;
        let more = controls.offset.saturating_add(page_len) < total;
        if filled && more {
            links.push(json!({
                "relation": "next",
                "url": query_url(&model.name, params, controls.offset + count)
            }));
        }
    }
    json!(links)
}

fn query_url(type_name: &str, params: &BTreeMap<String, String>, offset: usize) -> String {
    let mut parts = Vec::new();
    for (key, value) in params {
        if key == "_offset" || key == "_cursor" || is_compartment_param(key) {
            continue;
        }
        // A parameter given more than once is written as it was sent.
        for occurrence in value.split(AND) {
            parts.push(format!("{key}={occurrence}"));
        }
    }
    if offset > 0 {
        parts.push(format!("_cursor={}", encode_cursor(offset, params)));
    }
    if parts.is_empty() {
        type_name.to_string()
    } else {
        format!("{type_name}?{}", parts.join("&"))
    }
}

#[derive(Debug)]
enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Cmp {
        name: String,
        op: String,
        value: String,
    },
}

fn parse_filter(input: &str) -> Result<Filter, String> {
    let tokens = filter_tokens(input)?;
    let mut parser = FilterParser { tokens, pos: 0 };
    let filter = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err("invalid _filter".into());
    }
    Ok(filter)
}

struct FilterParser {
    tokens: Vec<String>,
    pos: usize,
}

impl FilterParser {
    fn parse_or(&mut self) -> Result<Filter, String> {
        let mut parts = vec![self.parse_and()?];
        while self.eat("or") {
            parts.push(self.parse_and()?);
        }
        if parts.len() == 1 {
            Ok(parts.pop().expect("part"))
        } else {
            Ok(Filter::Or(parts))
        }
    }

    fn parse_and(&mut self) -> Result<Filter, String> {
        let mut parts = vec![self.parse_primary()?];
        while self.eat("and") {
            parts.push(self.parse_primary()?);
        }
        if parts.len() == 1 {
            Ok(parts.pop().expect("part"))
        } else {
            Ok(Filter::And(parts))
        }
    }

    fn parse_primary(&mut self) -> Result<Filter, String> {
        if self.eat("(") {
            let inner = self.parse_or()?;
            if !self.eat(")") {
                return Err("invalid _filter".into());
            }
            return Ok(inner);
        }
        let name = self.next().ok_or_else(|| "invalid _filter".to_string())?;
        let op = self.next().ok_or_else(|| "invalid _filter".to_string())?;
        if !is_filter_op(&op) {
            return Err("invalid _filter".into());
        }
        let value = self.next().ok_or_else(|| "invalid _filter".to_string())?;
        Ok(Filter::Cmp { name, op, value })
    }

    fn eat(&mut self, word: &str) -> bool {
        if self.tokens.get(self.pos).map(String::as_str) == Some(word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<String> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }
}

/// Every comparison in a `_filter` expression, as (parameter, operator, value).
fn filter_comparisons<'a>(filter: &'a Filter, found: &mut Vec<(&'a str, &'a str, &'a str)>) {
    match filter {
        Filter::And(parts) | Filter::Or(parts) => {
            for part in parts {
                filter_comparisons(part, found);
            }
        }
        Filter::Cmp { name, op, value } => found.push((name, op, value)),
    }
}

fn is_filter_op(op: &str) -> bool {
    matches!(
        op,
        "eq" | "ne" | "gt" | "lt" | "ge" | "le" | "sa" | "eb" | "ap" | "co" | "sw" | "ew" | "pr"
    )
}

fn filter_tokens(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '(' || ch == ')' {
            tokens.push(ch.to_string());
            continue;
        }
        if ch == '"' {
            let mut text = String::new();
            let mut closed = false;
            for item in chars.by_ref() {
                if item == '"' {
                    closed = true;
                    break;
                }
                text.push(item);
            }
            if !closed {
                return Err("invalid _filter".into());
            }
            tokens.push(text);
            continue;
        }
        let mut text = String::from(ch);
        while let Some(item) = chars.peek() {
            if item.is_whitespace() || *item == '(' || *item == ')' {
                break;
            }
            text.push(chars.next().expect("peeked"));
        }
        tokens.push(text);
    }
    Ok(tokens)
}

fn eval_filter(
    input: &str,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    host: &TxHost<'_>,
) -> bool {
    let Ok(filter) = parse_filter(input) else {
        return false;
    };
    eval_filter_expr(&filter, catalog, model, resource, host)
}

fn eval_filter_expr(
    filter: &Filter,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    host: &TxHost<'_>,
) -> bool {
    match filter {
        Filter::And(parts) => parts
            .iter()
            .all(|part| eval_filter_expr(part, catalog, model, resource, host)),
        Filter::Or(parts) => parts
            .iter()
            .any(|part| eval_filter_expr(part, catalog, model, resource, host)),
        Filter::Cmp { name, op, value } => model
            .ready_param(name)
            .map(|param| {
                param
                    .extractor
                    .matches_op(resource, op, value, catalog.path_model(), host)
            })
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keeping only the first `wanted` matches while they arrive, in the batches `trim_ranked`
    /// chooses, leaves the same page as sorting them all and cutting it. Keys repeat on purpose,
    /// so the order of the ones that tie (their ids) is what is checked.
    #[test]
    fn trimming_the_ranked_matches_keeps_what_sorting_them_all_keeps() {
        // A small generator: the tests of this crate have no random source of their own.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let terms: [&[(String, bool)]; 3] = [
            &[],
            &[(String::new(), false)],
            &[(String::new(), true), (String::new(), false)],
        ];
        for sort in terms {
            for (count, offset) in [(0usize, 0usize), (1, 0), (3, 0), (3, 40), (10, 5), (200, 0)] {
                let wanted = count + offset;
                let make = |n: usize, next: &mut dyn FnMut(u64) -> u64| Ranked {
                    keys: sort.iter().map(|_| format!("k{}", next(7))).collect(),
                    id: format!("id{n:05}"),
                    json: json!({"n": n}),
                    ts: None,
                };
                let all: Vec<Ranked> = (0..1500).map(|n| make(n, &mut next)).collect();
                let mut want: Vec<&Ranked> = all.iter().collect();
                want.sort_by(|left, right| left.compare(right, sort));
                let want: Vec<&str> = want
                    .iter()
                    .skip(offset)
                    .take(count)
                    .map(|item| item.id.as_str())
                    .collect();
                let mut held = Vec::new();
                let mut most = 0;
                for item in &all {
                    held.push(Ranked {
                        keys: item.keys.clone(),
                        id: item.id.clone(),
                        json: item.json.clone(),
                        ts: None,
                    });
                    most = most.max(held.len());
                    if wanted > 0 {
                        trim_ranked(&mut held, wanted, sort);
                    } else {
                        held.clear();
                    }
                }
                held.sort_by(|left, right| left.compare(right, sort));
                let got: Vec<&str> = held
                    .iter()
                    .skip(offset)
                    .take(count)
                    .map(|item| item.id.as_str())
                    .collect();
                assert_eq!(got, want, "{} terms, {count} from {offset}", sort.len());
                assert!(
                    most <= (2 * wanted).max(64),
                    "held {most} for a page of {wanted}"
                );
            }
        }
    }

    /// With no page size every match is kept, as the page is all of them.
    #[test]
    fn a_page_of_everything_trims_nothing() {
        let sort = [(String::new(), false)];
        let mut held: Vec<Ranked> = (0..300)
            .map(|n| Ranked {
                keys: vec![format!("k{}", n % 3)],
                id: format!("id{n:03}"),
                json: Value::Null,
                ts: None,
            })
            .collect();
        trim_ranked(&mut held, usize::MAX, &sort);
        assert_eq!(held.len(), 300);
    }

    fn or(text: &str) -> Vec<String> {
        or_values(text).unwrap()
    }

    #[test]
    fn a_value_with_no_comma_is_its_own_alternative() {
        assert_eq!(or("free"), ["free"]);
        assert_eq!(or(""), [""]);
        assert_eq!(or(r"a\|b"), [r"a\|b"]);
        assert_eq!(or("a|b"), ["a|b"]);
        assert_eq!(or(r"trailing\"), [r"trailing\"]);
    }

    #[test]
    fn a_comma_separates_alternatives() {
        assert_eq!(or("free,busy"), ["free", "busy"]);
        assert_eq!(or("a,b,c"), ["a", "b", "c"]);
        // Values are not trimmed, repeats are dropped and so are empty alternatives.
        assert_eq!(or("a, b"), ["a", " b"]);
        assert_eq!(or("a,a,b,a"), ["a", "b"]);
        assert_eq!(or("a,,b,"), ["a", "b"]);
        assert_eq!(or(",a"), ["a"]);
        // Nothing left is still one (empty) value, as an empty value always was.
        assert_eq!(or(","), [""]);
        assert_eq!(or(",,"), [""]);
    }

    #[test]
    fn an_escaped_comma_is_a_comma_in_the_value() {
        assert_eq!(or(r"a\,b"), ["a,b"]);
        assert_eq!(or(r"a\,b,c"), ["a,b", "c"]);
        assert_eq!(or(r"a\,\,b"), ["a,,b"]);
        assert_eq!(or(r"\,"), [","]);
    }

    #[test]
    fn a_backslash_pair_never_hides_the_comma_after_it() {
        assert_eq!(or(r"a\\,b"), [r"a\\", "b"]);
        assert_eq!(or(r"a\\\,b"), [r"a\\,b"]);
        // Escapes for other characters are left for whoever splits on them.
        assert_eq!(or(r"a\$b,c\|d"), [r"a\$b", r"c\|d"]);
        assert_eq!(or(r"a,\"), ["a", r"\"]);
    }

    #[test]
    fn a_list_of_alternatives_has_a_limit_and_repeats_do_not_count_toward_it() {
        let many: Vec<String> = (0..=MAX_ALTERNATIVES).map(|n| format!("v{n}")).collect();
        assert!(or_values(&many.join(",")).is_err());
        let at_the_limit = or_values(&many[..MAX_ALTERNATIVES].join(",")).unwrap();
        assert_eq!(at_the_limit.len(), MAX_ALTERNATIVES);
        let repeated = vec!["a"; MAX_ALTERNATIVES * 3].join(",");
        assert_eq!(or(&repeated), ["a"]);
    }

    #[test]
    fn add_param_joins_a_second_occurrence_as_join_repeated_does() {
        let mut params = BTreeMap::new();
        for (name, value) in [
            ("date", "ge1"),
            ("date", "lt2"),
            ("_include", "a"),
            ("_include", "b"),
            ("status", "free"),
        ] {
            add_param(&mut params, name.to_string(), value.to_string());
        }
        assert_eq!(params["date"], "ge1\u{1f}lt2");
        assert_eq!(params["_include"], "a,b");
        assert_eq!(params["status"], "free");
    }

    #[test]
    fn repeated_occurrences_are_joined_as_and_but_lists_stay_lists() {
        assert_eq!(join_repeated("date", "ge1", "lt2"), "ge1\u{1f}lt2");
        assert_eq!(join_repeated("status", "a,b", "c"), "a,b\u{1f}c");
        for list in [
            "_include",
            "_include:iterate",
            "_revinclude",
            "_revinclude:iterate",
            "_sort",
            "_elements",
        ] {
            assert_eq!(join_repeated(list, "a", "b"), "a,b", "{list}");
        }
        // The pieces come back apart as clauses: one per occurrence, each its own OR list.
        let joined = join_repeated("status", "a,b", "c");
        let pieces: Vec<&str> = joined.split(AND).collect();
        assert_eq!(pieces, ["a,b", "c"]);
    }

    #[test]
    fn include_rules_know_whether_they_iterate() {
        let spec = |text: &str, iterate: bool| parse_include(text, iterate).expect(text);
        let plain = spec("Slot:schedule", false);
        assert!(!plain.iterate);
        assert_eq!(plain.source.as_deref(), Some("Slot"));
        assert_eq!(plain.param, "schedule");
        assert_eq!(plain.target, None);
        assert!(spec("Slot:schedule", true).iterate);
        let marked = spec("Slot:schedule:iterate", false);
        assert!(marked.iterate);
        assert_eq!(marked.target, None);
        let targeted = spec("Schedule:actor:Location", true);
        assert_eq!(targeted.target.as_deref(), Some("Location"));
        assert!(spec("*", true).iterate);
    }

    #[test]
    fn the_iterate_forms_are_rules_of_their_own() {
        let params: BTreeMap<String, String> = [
            ("_include", "Slot:schedule"),
            ("_include:iterate", "Schedule:actor,Schedule:actor:Location"),
            ("_revinclude:iterate", "Slot:schedule"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        let include = include_specs(&params, "_include").unwrap();
        let iterating: Vec<bool> = include.iter().map(|spec| spec.iterate).collect();
        assert_eq!(iterating, [false, true, true]);
        let revinclude = include_specs(&params, "_revinclude").unwrap();
        assert_eq!(revinclude.len(), 1);
        assert!(revinclude[0].iterate);
        // Neither is a search parameter.
        assert!(is_control("_include:iterate") && is_control("_revinclude:iterate"));
    }
}
