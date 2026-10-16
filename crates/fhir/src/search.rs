use std::collections::{BTreeMap, BTreeSet};

use refhir_database::Transaction;
use refhir_types::{
    exact_key, string_at_least, string_at_most, string_interval, token_interval, IndexId, Interval,
};
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

/// The compartment parameters are not part of a search's identity: a paging link or a cursor does
/// not carry them, because each request gets its limit from the server again.
fn is_compartment_param(key: &str) -> bool {
    key == "_compartment" || key == PATIENT_COMPARTMENT
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
    compartments: Vec<(String, String)>,
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

    /// Whether the resources of the page are used at all: `_summary=count` shows none, unless
    /// `_include` or `_revinclude` follow references out of them.
    fn needs_documents(&self) -> bool {
        self.summary.as_deref() != Some("count")
            || !self.include.is_empty()
            || !self.revinclude.is_empty()
    }
}

struct IncludeSpec {
    /// Resource type named before the parameter, when the token has one.
    source: Option<String>,
    param: String,
    target: Option<String>,
}

enum Clause {
    Id(String),
    Filter(String),
    Param {
        name: String,
        modifier: Option<String>,
        value: String,
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
}

pub fn search(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
) -> Result<Value, FhirError> {
    search_counted(tx, catalog, model, params).map(|(bundle, _)| bundle)
}

pub(crate) fn search_counted(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
) -> Result<(Value, Work), FhirError> {
    catalog.deny_response_read(&model.name)?;
    if let Some(query) = params.get("_query") {
        return Err(FhirError::Invalid(format!("unknown query {query}")));
    }
    let controls = controls_from(params)?;
    let clauses = clauses_from(model, params)?;
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
    let bundle = assemble(tx, catalog, model, params, &controls, page, total)?;
    Ok((bundle, work))
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
            let (rows, more) =
                tx.index_ranges_first(&scan.index, &scan.intervals, offset.saturating_add(count))?;
            let total = rows.len() + usize::from(more);
            (rows, total)
        }
        _ => {
            let rows = tx.index_ranges_entries(&scan.index, &scan.intervals)?;
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
fn page_by_checking(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    scan: &Scan,
    checks: &Checks<'_>,
    controls: &Controls,
) -> Result<Page, FhirError> {
    let rows = tx.index_ranges_text(&scan.index, &scan.intervals)?;
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
    let mut kept = Vec::new();
    let mut total = 0usize;
    for row in &rows {
        let json = row.json()?;
        work.decoded += 1;
        work.checked += 1;
        if !row_matches(catalog, model, &json, checks, controls, &host) {
            continue;
        }
        if sorted || (total >= controls.offset && kept.len() < limit) {
            let id = row.id.id_str().unwrap_or("").to_string();
            kept.push((id.clone(), decorate(json, &id, row.ts)));
        }
        total += 1;
        if total >= stop_at {
            break;
        }
    }
    if sorted {
        sort_matches(catalog, model, &mut kept, &controls.sort);
        let page = kept.into_iter().skip(controls.offset).take(limit).collect();
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
    let included = include_entries(tx, catalog, model, &page, controls)?;
    entries.extend(included);
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
    let controls = controls_from(params)?;
    let clauses = clauses_from(model, params)?;
    let chain_ids = chain_sets(tx, catalog, model, &clauses)?;
    let has_ids = has_sets(tx, catalog, model, &clauses)?;
    let scan = choose_index(model, &clauses);
    let rows = tx.index_ranges_text(&scan.index, &scan.intervals)?;
    let host = TxHost::new(tx, catalog);
    let mut matched = Vec::new();
    for row in rows {
        let json = row.json()?;
        let checks = Checks {
            clauses: &clauses,
            proven: None,
            chain_ids: &chain_ids,
            has_ids: &has_ids,
        };
        if !row_matches(catalog, model, &json, &checks, &controls, &host) {
            continue;
        }
        let id = row.id.id_str().unwrap_or("").to_string();
        matched.push((id, decorate(json, row.id.id_str().unwrap_or(""), row.ts)));
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
        compartments.push((type_name.to_string(), id.to_string()));
    }
    // The server's limit has to hold or the search must not run: ignoring a malformed one would
    // search without it.
    if let Some(text) = params.get(PATIENT_COMPARTMENT) {
        let (type_name, id) = text
            .split_once('/')
            .ok_or_else(|| FhirError::Invalid(format!("{PATIENT_COMPARTMENT} must be Type/id")))?;
        compartments.push((type_name.to_string(), id.to_string()));
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
        include: split_includes(params.get("_include")),
        revinclude: split_includes(params.get("_revinclude")),
        compartments,
    })
}

fn parse_usize(text: &str, name: &str) -> Result<usize, FhirError> {
    text.parse()
        .map_err(|_| FhirError::Invalid(format!("{name} must be an integer")))
}

fn split_includes(value: Option<&String>) -> Vec<IncludeSpec> {
    let Some(value) = value else {
        return Vec::new();
    };
    value
        .split(',')
        .filter(|part| !part.is_empty())
        .filter_map(parse_include)
        .collect()
}

fn parse_include(text: &str) -> Option<IncludeSpec> {
    let text = text.trim().trim_end_matches(":iterate");
    if text == "*" {
        return Some(IncludeSpec {
            source: None,
            param: "*".into(),
            target: None,
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
    })
}

fn clauses_from(
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
) -> Result<Vec<Clause>, FhirError> {
    let mut clauses = Vec::new();
    for (name, value) in params {
        if is_control(name) {
            continue;
        }
        if name == "_id" {
            clauses.push(Clause::Id(value.clone()));
            continue;
        }
        if name == "_query" {
            continue;
        }
        if name == "_filter" {
            let filter = parse_filter(value).map_err(FhirError::Invalid)?;
            check_filter_ids(model, &filter)?;
            clauses.push(Clause::Filter(value.clone()));
            continue;
        }
        if let Some(clause) = parse_has(name, value) {
            clauses.push(clause);
            continue;
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
            clauses.push(clause);
            continue;
        }
        let (base, modifier) = split_modifier(name);
        let Some(param) = model.ready_param(&base) else {
            return Err(FhirError::Invalid(format!(
                "unsupported search parameter {name} on {}",
                model.name
            )));
        };
        // Only a plain value or `:not` is an id to look for; `:missing` takes true or false.
        if matches!(modifier.as_deref(), None | Some("not")) {
            if let ReferenceId::Open(types) = param.extractor.reference_id(value) {
                return Err(open_reference(&base, value, types));
            }
        }
        clauses.push(Clause::Param {
            name: base,
            modifier,
            value: value.clone(),
        });
    }
    Ok(clauses)
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
            | "_revinclude"
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
    /// The key ranges read, one for almost every search. A bare id for a reference that can point
    /// at several types is one range for each type.
    intervals: Vec<Interval>,
    /// The clause whose match every row of `intervals` already proves, if there is one.
    proven: Option<usize>,
}

impl Scan {
    /// Whether the range decides every clause, so no row needs checking.
    fn decides_all(&self, clauses: &[Clause]) -> bool {
        (0..clauses.len()).all(|position| self.proven == Some(position))
    }
}

fn choose_index(model: &ResourceModel, clauses: &[Clause]) -> Scan {
    for clause in clauses {
        if let Clause::Id(id) = clause {
            return Scan {
                index: model.by_id(),
                intervals: vec![Interval::point(exact_key(id))],
                proven: None,
            };
        }
    }
    let mut chosen: Option<(u8, String, Vec<Interval>, usize, bool)> = None;
    for (position, clause) in clauses.iter().enumerate() {
        let Clause::Param {
            name,
            modifier,
            value,
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
        let probe = param.extractor.probe(value);
        let (rank, intervals) = match probe {
            Probe::Token(text) => (0, vec![token_interval(text)]),
            Probe::Tokens(tokens) => (0, tokens.iter().map(|text| token_interval(text)).collect()),
            Probe::Prefix(text) => (1, vec![string_interval(text)]),
            Probe::AtLeast(text) => (2, vec![string_at_least(text)]),
            Probe::AtMost(text) => (2, vec![string_at_most(text)]),
            Probe::Range(interval) => (2, vec![interval]),
            Probe::Scan => continue,
        };
        let replace = chosen
            .as_ref()
            .map(|(prev, _, _, _, _)| rank < *prev)
            .unwrap_or(true);
        if replace {
            // A modifier changes what a match is, so only a plain parameter can be proven.
            let proven = modifier.is_none() && param.extractor.index_proves(value);
            chosen = Some((rank, name.clone(), intervals, position, proven));
        }
    }
    if let Some((_, name, intervals, position, proven)) = chosen {
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
    for (type_name, id) in &controls.compartments {
        if !in_compartment(catalog, model, resource, type_name, id, host) {
            return false;
        }
    }
    clauses.iter().enumerate().all(|(index, clause)| {
        proven == Some(index)
            || match clause {
                Clause::Id(id) => resource.get("id").and_then(|item| item.as_str()) == Some(id),
                Clause::Filter(value) => eval_filter(value, catalog, model, resource, host),
                Clause::Param {
                    name,
                    modifier,
                    value,
                } => param_matches(
                    catalog,
                    model,
                    resource,
                    name,
                    modifier.as_deref(),
                    value,
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

fn param_matches(
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    name: &str,
    modifier: Option<&str>,
    value: &str,
    host: &TxHost<'_>,
) -> bool {
    let Some(param) = model.ready_param(name) else {
        return false;
    };
    match modifier {
        None => param
            .extractor
            .matches(resource, value, catalog.path_model(), host),
        Some("above" | "below" | "in" | "not-in") => hierarchy_match(
            catalog,
            &param
                .extractor
                .indexed(resource, catalog.path_model(), host),
            modifier.unwrap_or(""),
            value,
        ),
        Some(modifier) => {
            param
                .extractor
                .matches_modifier(resource, modifier, value, catalog.path_model(), host)
        }
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
                .any(|reference| reference == &token || reference.ends_with(&format!("/{id}")))
        })
    })
}

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

fn include_entries(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    page: &[(String, Value)],
    controls: &Controls,
) -> Result<Vec<Value>, FhirError> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for (_, resource) in page {
        for spec in &controls.include {
            push_forward(tx, catalog, model, resource, spec, &mut entries, &mut seen)?;
        }
    }
    for (id, _) in page {
        for spec in &controls.revinclude {
            push_reverse(tx, catalog, model, id, spec, &mut entries, &mut seen)?;
        }
    }
    Ok(entries)
}

fn push_forward(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    spec: &IncludeSpec,
    entries: &mut Vec<Value>,
    seen: &mut BTreeSet<String>,
) -> Result<(), FhirError> {
    if spec
        .source
        .as_deref()
        .is_some_and(|source| source != "*" && source != model.name)
    {
        return Ok(());
    }
    let params: Vec<&crate::catalog::SearchParam> = if spec.param == "*" {
        model
            .params
            .iter()
            .filter(|param| {
                param.status == refhir_persistence::ParamStatus::Ready
                    && param.kind == crate::extract::SearchKind::Reference
            })
            .collect()
    } else {
        model.ready_param(&spec.param).into_iter().collect()
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
            entries.push(json!({
                "resource": decorate(stored.json, id, stored.ts),
                "search": {"mode": "include"}
            }));
        }
    }
    Ok(())
}

fn push_reverse(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    id: &str,
    spec: &IncludeSpec,
    entries: &mut Vec<Value>,
    seen: &mut BTreeSet<String>,
) -> Result<(), FhirError> {
    let token = format!("{}/{}", model.name, id);
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
        let rows = tx.index_range(&other.search_index(&spec.param), token_interval(&token))?;
        for row in rows {
            if !param
                .extractor
                .indexed(&row.json, catalog.path_model(), &TxHost::new(tx, catalog))
                .iter()
                .any(|reference| reference == &token)
            {
                continue;
            }
            let other_id = row.id.id_str().unwrap_or("").to_string();
            if !seen.insert(format!("{}/{}", other.name, other_id)) {
                continue;
            }
            entries.push(json!({
                "resource": decorate(row.json, &other_id, row.ts),
                "search": {"mode": "include"}
            }));
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
        parts.push(format!("{key}={value}"));
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
