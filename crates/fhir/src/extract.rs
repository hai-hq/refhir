//! Compile a SearchParameter into index keys and a match function.
//!
//! Token, string, reference, date, uri, number, quantity, and composite
//! parameters from the R4 set all compile. `memberOf()` stays out of the
//! index expression. Number and quantity prefixes narrow to a sortable range.
//! The matcher still applies implicit precision.

use std::collections::BTreeMap;

use refhir_fhirpath::{Expr, Host, Model};
use refhir_types::{
    number_key, number_range, quantity_key, quantity_range, string_key, token_key, valid_id,
    IndexId, Interval,
};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKind {
    Token,
    String,
    Reference,
    Date,
    Uri,
    Number,
    Quantity,
    Composite,
    Special,
}

impl SearchKind {
    pub fn fhir_type(self) -> &'static str {
        match self {
            SearchKind::Token => "token",
            SearchKind::String => "string",
            SearchKind::Reference => "reference",
            SearchKind::Date => "date",
            SearchKind::Uri => "uri",
            SearchKind::Number => "number",
            SearchKind::Quantity => "quantity",
            SearchKind::Composite => "composite",
            SearchKind::Special => "special",
        }
    }
}

/// A bare id stands for at most this many resource types. A reference parameter that can point at
/// more (`Task?subject` points at any resource) would read an index range for each of them, and an
/// id alone says little about which was meant, so it has to be given as `Type/id`.
const MAX_BARE_ID_TARGETS: usize = 16;

/// What a query value for a reference parameter says about the resource it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReferenceId {
    /// A reference as it is stored (`Patient/123`, a URL), or a parameter that is not a reference:
    /// compared as written.
    AsWritten,
    /// A bare id (`123`): the references it stands for, `Type/123` for each type the parameter can
    /// point at.
    Typed(Vec<String>),
    /// A bare id the parameter cannot place: it declares no target types, or more than
    /// `MAX_BARE_ID_TARGETS`. Carries how many it declares.
    Open(usize),
}

/// How a query value can narrow the index. `Scan` leaves that to `matches`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe<'a> {
    Token(&'a str),
    /// Any of these tokens. Each one is a range of its own.
    Tokens(Vec<String>),
    Prefix(&'a str),
    /// Inclusive lower bound on a string or date key. Matching still applies the prefix.
    AtLeast(&'a str),
    /// Exclusive upper bound on a string or date key.
    AtMost(&'a str),
    /// Inclusive start, exclusive end, over number or quantity keys.
    Range(Interval),
    Scan,
}

#[derive(Clone, Debug)]
enum Source {
    Path(Expr),
    Composite { focus: Expr, parts: Vec<Part> },
    Narrative,
    Content,
    Age,
    MultipleBirth,
    Near,
    Filter,
    Query,
}

#[derive(Clone, Debug)]
struct Part {
    kind: SearchKind,
    expr: Expr,
}

#[derive(Clone, Debug)]
pub struct Extractor {
    kind: SearchKind,
    source: Source,
    /// The resource types a reference parameter can point at, as its `SearchParameter` lists them.
    targets: Vec<String>,
}

impl Extractor {
    pub fn kind(&self) -> SearchKind {
        self.kind
    }

    /// What `query` names, for a reference parameter read from a path.
    ///
    /// A reference is stored as `Type/id`, so `patient=Patient/123` compares whole tokens. FHIR also
    /// lets a search give just the id, and the parameter's target types supply the rest. The types
    /// are the `SearchParameter`'s own, wider than what an element allows (`patient` lists `Patient`
    /// and `Group`), which is safe because the parameter's expression decides what is indexed under
    /// it: `Observation.subject.where(resolve() is Patient)` never holds a `Group/123`.
    pub fn reference_id(&self, query: &str) -> ReferenceId {
        if let Some(types) = self.typed_targets(query) {
            return ReferenceId::Typed(
                types
                    .iter()
                    .map(|type_name| format!("{type_name}/{query}"))
                    .collect(),
            );
        }
        if self.is_bare_id(query) {
            return ReferenceId::Open(self.targets.len());
        }
        ReferenceId::AsWritten
    }

    /// Limits the target types of a reference parameter to those it can hold in a resource of type
    /// `base`.
    ///
    /// `SearchParameter.target` is one list for every base the parameter has. The shared `patient`
    /// parameter lists `Patient` and `Group` for all thirty-odd of them, although most read an
    /// element that can only hold a Patient, and some add `.where(resolve() is Patient)`. So for
    /// each branch of the expression that reads from `base`, a reference can only be to a listed
    /// type that the element allows (`element_targets`, which is given the element's path) and,
    /// if the branch resolves, to the type it resolves to. A bare id then stands for those types,
    /// and the usual search (`patient=123`) reads one index range, not one for each listed type.
    ///
    /// A branch this cannot read, a cast or a function, may hold any listed type, and an
    /// expression with no branch that reads from `base` is left as it is.
    pub fn narrow_targets<'a>(
        &mut self,
        base: &str,
        element_targets: impl Fn(&str) -> Option<&'a [String]>,
    ) {
        let Source::Path(expr) = &self.source else {
            return;
        };
        if self.kind != SearchKind::Reference || self.targets.is_empty() {
            return;
        }
        let mut held: Vec<String> = Vec::new();
        let mut read_from_base = false;
        for branch in expr.union_operands() {
            if branch.root_name() != Some(base) {
                continue;
            }
            read_from_base = true;
            let (path, resolved) = match branch.resolve_filter() {
                Some((focus, type_name)) => (focus, Some(type_name)),
                None => (branch, None),
            };
            // An element that can point at any resource restricts nothing.
            let element = path
                .member_path()
                .and_then(|(_, members)| element_targets(&members.join(".")))
                .filter(|types| !types.is_empty() && !types.iter().any(|item| item == "Resource"));
            for type_name in &self.targets {
                let allowed = element.is_none_or(|types| types.contains(type_name));
                let resolves = resolved.is_none_or(|resolved| resolved == type_name);
                if allowed && resolves && !held.contains(type_name) {
                    held.push(type_name.clone());
                }
            }
        }
        if read_from_base && !held.is_empty() {
            self.targets.retain(|type_name| held.contains(type_name));
        }
    }

    /// Whether `query` is an id given to a reference parameter read from a path.
    fn is_bare_id(&self, query: &str) -> bool {
        self.kind == SearchKind::Reference
            && matches!(self.source, Source::Path(_))
            && valid_id(query)
    }

    /// The types a bare id `query` stands for, when this parameter can place one.
    fn typed_targets(&self, query: &str) -> Option<&[String]> {
        (self.is_bare_id(query) && (1..=MAX_BARE_ID_TARGETS).contains(&self.targets.len()))
            .then_some(self.targets.as_slice())
    }

    pub fn probe<'a>(&self, query: &'a str) -> Probe<'a> {
        if let ReferenceId::Typed(tokens) = self.reference_id(query) {
            return Probe::Tokens(tokens);
        }
        match &self.source {
            Source::Composite { parts, .. } => {
                let first = query.split('$').next().unwrap_or(query);
                probe_kind(parts.first().map(|part| part.kind), first)
            }
            Source::Narrative
            | Source::Content
            | Source::Age
            | Source::Near
            | Source::Filter
            | Source::Query => Probe::Scan,
            Source::MultipleBirth => Probe::Token(query),
            Source::Path(_) => probe_kind(Some(self.kind), query),
        }
    }

    /// Whether finding a resource through the index range that `probe(query)` selects already
    /// proves that `matches(.., query)` holds, so a search need not evaluate the parameter against
    /// the resource a second time.
    ///
    /// That is so for a string, token, reference or uri parameter read from a path. The keys are
    /// written from `path_strings`, which is the list of values `path_match` compares; a token key
    /// holds the whole token behind its length, and a string key the lowercased value followed by
    /// a NUL; so the range holds exactly the resources with a value equal to the token, or starting
    /// with the lowercased prefix. (A bare id for a reference parameter selects several token
    /// ranges, one for each type it can point at. That holds only because `matches_op` reads the id
    /// as the same set of `Type/id` references: the probe and the matcher must expand it alike.)
    /// Two things break that and are refused. An expression that asks the world something when it
    /// is read (a referenced resource, the clock, a profile, a value set) can disagree with keys
    /// saved earlier. And a NUL in a string query would match the NUL that ends every string key.
    ///
    /// This trusts the keys: they are as the code that reads them would write them. After a release
    /// that changes how a value is turned into keys, rows saved by the old release keep their old
    /// keys until they are written again, and a search no longer sets aside the ones that would
    /// not match now, as its second check did. Nothing guards against that but re-indexing.
    pub fn index_proves(&self, query: &str) -> bool {
        let Source::Path(expr) = &self.source else {
            return false;
        };
        if expr.depends_on_host() {
            return false;
        }
        match self.kind {
            SearchKind::Token | SearchKind::Reference | SearchKind::Uri => true,
            SearchKind::String => !query.contains('\0'),
            _ => false,
        }
    }

    pub fn keys(
        &self,
        resource: &Value,
        id: &str,
        index: &IndexId,
        path_model: &Model,
        host: &dyn Host,
    ) -> Vec<(IndexId, Vec<u8>)> {
        if self.kind == SearchKind::Number {
            return number_values(self, resource, path_model, host)
                .into_iter()
                .filter(|value| value.is_finite())
                .map(|value| (index.clone(), number_key(value, id)))
                .collect();
        }
        if self.kind == SearchKind::Quantity {
            return quantity_values(self, resource, path_model, host)
                .into_iter()
                .filter(|span| span.exact.is_finite())
                .map(|span| {
                    (
                        index.clone(),
                        quantity_key(span.code.as_deref().unwrap_or(""), span.exact, id),
                    )
                })
                .collect();
        }
        let values = self.indexed_values(resource, path_model, host);
        match self.probe("") {
            Probe::Token(_) | Probe::Tokens(_) => token_rows(index, id, &values),
            Probe::Prefix(_) | Probe::AtLeast(_) | Probe::AtMost(_) => {
                string_rows(index, id, &values)
            }
            Probe::Range(_) | Probe::Scan => Vec::new(),
        }
    }

    pub fn indexed(&self, resource: &Value, path_model: &Model, host: &dyn Host) -> Vec<String> {
        self.indexed_values(resource, path_model, host)
    }

    pub fn matches_modifier(
        &self,
        resource: &Value,
        modifier: &str,
        expected: &str,
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        match modifier {
            "missing" => self.matches_op(resource, "pr", expected, path_model, host),
            "exact" => self.exact_match(resource, expected, path_model, host),
            "contains" => self.contains_match(resource, expected, path_model, host),
            "not" => !self.matches(resource, expected, path_model, host),
            "text" => self.display_match(resource, expected, path_model, host),
            "of-type" => self.of_type_match(resource, expected, path_model, host),
            "identifier" => self.identifier_match(resource, expected, path_model, host),
            _ => false,
        }
    }

    /// `matches_modifier` for each of `expected`, true when any is: the resource is read once,
    /// not once per value. `not` is true when none of them matches.
    pub fn matches_modifier_any(
        &self,
        resource: &Value,
        modifier: &str,
        expected: &[String],
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        match modifier {
            "exact" => {
                let values = self.indexed_values(resource, path_model, host);
                expected
                    .iter()
                    .any(|wanted| values.iter().any(|text| text == wanted))
            }
            "contains" => {
                let values: Vec<String> = self
                    .indexed_values(resource, path_model, host)
                    .iter()
                    .map(|text| text.to_lowercase())
                    .collect();
                expected.iter().any(|wanted| {
                    let needle = wanted.to_lowercase();
                    values.iter().any(|text| text.contains(&needle))
                })
            }
            "not" => !self.matches_any(resource, expected, path_model, host),
            _ => expected
                .iter()
                .any(|wanted| self.matches_modifier(resource, modifier, wanted, path_model, host)),
        }
    }

    fn exact_match(
        &self,
        resource: &Value,
        expected: &str,
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        self.indexed_values(resource, path_model, host)
            .iter()
            .any(|text| text == expected)
    }

    fn contains_match(
        &self,
        resource: &Value,
        expected: &str,
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        let needle = expected.to_lowercase();
        self.indexed_values(resource, path_model, host)
            .iter()
            .any(|text| text.to_lowercase().contains(&needle))
    }

    fn display_match(
        &self,
        resource: &Value,
        expected: &str,
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        let needle = expected.to_lowercase();
        let Source::Path(expr) = &self.source else {
            return self.contains_match(resource, expected, path_model, host);
        };
        display_strings(&eval_items(expr, resource, path_model, host))
            .iter()
            .any(|text| text.to_lowercase().contains(&needle))
    }

    fn of_type_match(
        &self,
        resource: &Value,
        expected: &str,
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        let mut parts = expected.split('|');
        let Some(system) = parts.next() else {
            return false;
        };
        let Some(code) = parts.next() else {
            return false;
        };
        let Some(value) = parts.next() else {
            return false;
        };
        let Source::Path(expr) = &self.source else {
            return false;
        };
        focus_nodes(expr, resource, path_model, host)
            .iter()
            .any(|node| identifier_of_type(node, system, code, value))
    }

    fn identifier_match(
        &self,
        resource: &Value,
        expected: &str,
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        let Source::Path(expr) = &self.source else {
            return false;
        };
        let (system, value) = expected.split_once('|').unwrap_or(("", expected));
        focus_nodes(expr, resource, path_model, host)
            .iter()
            .any(|node| reference_identifier(node, system, value))
    }

    pub fn matches(
        &self,
        resource: &Value,
        expected: &str,
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        self.matches_op(resource, "eq", expected, path_model, host)
    }

    /// `matches` for each of `expected`, true when any is: the resource is read once, not once per
    /// value.
    pub fn matches_any(
        &self,
        resource: &Value,
        expected: &[String],
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        match &self.source {
            Source::Path(expr) if self.kind == SearchKind::Reference => {
                let values = path_strings(self.kind, expr, resource, path_model, host);
                expected
                    .iter()
                    .any(|wanted| self.names_a_reference(&values, wanted))
            }
            Source::Path(expr) => {
                path_match(self.kind, expr, "eq", resource, expected, path_model, host)
            }
            _ => expected
                .iter()
                .any(|wanted| self.matches(resource, wanted, path_model, host)),
        }
    }

    /// Whether one of `values`, the references a resource holds, is the one `wanted` names: for a
    /// bare id a `Type/id` of a type this parameter points at, otherwise the same text. This runs
    /// for every resource a search checks, so it builds no tokens.
    fn names_a_reference(&self, values: &[String], wanted: &str) -> bool {
        match self.typed_targets(wanted) {
            Some(types) => values.iter().any(|value| {
                value.split_once('/').is_some_and(|(type_name, id)| {
                    id == wanted && types.iter().any(|item| item == type_name)
                })
            }),
            None => values.iter().any(|value| value == wanted),
        }
    }

    pub fn matches_op(
        &self,
        resource: &Value,
        op: &str,
        expected: &str,
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        if op == "pr" {
            let present = self.has_value(resource, path_model, host);
            return (expected == "true" && !present) || (expected == "false" && present);
        }
        match &self.source {
            Source::Filter | Source::Query => false,
            Source::Narrative => {
                let text = narrative_text(resource);
                match op {
                    "ne" => !text_match("co", &text, expected),
                    _ => text_match("co", &text, expected),
                }
            }
            Source::Content => content_match(op, resource, expected),
            Source::Age => number_match(op, &age_years(resource, host), expected),
            Source::MultipleBirth => token_match(op, &multiple_birth_tokens(resource), expected),
            Source::Near => near_match(resource, expected),
            Source::Composite { focus, parts } => {
                composite_match(focus, parts, resource, expected, path_model, host)
            }
            Source::Path(expr) => {
                if self.kind == SearchKind::Reference && matches!(op, "eq" | "ne") {
                    let values = path_strings(self.kind, expr, resource, path_model, host);
                    return self.names_a_reference(&values, expected) == (op == "eq");
                }
                path_match(self.kind, expr, op, resource, &[expected], path_model, host)
            }
        }
    }

    fn indexed_values(&self, resource: &Value, path_model: &Model, host: &dyn Host) -> Vec<String> {
        match &self.source {
            Source::Path(expr) => path_strings(self.kind, expr, resource, path_model, host),
            Source::MultipleBirth => multiple_birth_tokens(resource),
            Source::Composite { focus, parts } => {
                let Some(part) = parts.first() else {
                    return Vec::new();
                };
                if !matches!(
                    part.kind,
                    SearchKind::Token
                        | SearchKind::Reference
                        | SearchKind::Uri
                        | SearchKind::String
                        | SearchKind::Date
                ) {
                    return Vec::new();
                }
                focus_nodes(focus, resource, path_model, host)
                    .into_iter()
                    .flat_map(|node| path_strings(part.kind, &part.expr, &node, path_model, host))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn has_value(&self, resource: &Value, path_model: &Model, host: &dyn Host) -> bool {
        !self.indexed_values(resource, path_model, host).is_empty()
            || match &self.source {
                Source::Path(expr) => !eval_items(expr, resource, path_model, host).is_empty(),
                Source::Age => !age_years(resource, host).is_empty(),
                Source::Narrative => !narrative_text(resource).is_empty(),
                Source::Content => resource_strings(resource).next().is_some(),
                _ => false,
            }
    }
}

pub fn compile_parameter(
    parameter: &Value,
    component_types: &BTreeMap<String, String>,
) -> Option<Extractor> {
    let param_type = parameter.get("type").and_then(|value| value.as_str())?;
    let kind = search_kind(param_type)?;
    let code = parameter
        .get("code")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let expression = parameter
        .get("expression")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if expression.is_none() {
        return builtin(code, kind);
    }
    if kind == SearchKind::Composite {
        return compile_composite(parameter, expression?, component_types);
    }
    if kind == SearchKind::Special && code == "near" {
        return Some(Extractor {
            kind,
            source: Source::Near,
            targets: Vec::new(),
        });
    }
    let expr = refhir_fhirpath::parse(expression?).ok()?;
    if expr.calls("memberOf") {
        return None;
    }
    Some(Extractor {
        kind,
        source: Source::Path(expr),
        targets: target_types(parameter),
    })
}

/// The resource types a `SearchParameter` says a reference can point at, each once.
fn target_types(parameter: &Value) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    for type_name in parameter
        .get("target")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
    {
        if !targets.iter().any(|seen| seen == type_name) {
            targets.push(type_name.to_string());
        }
    }
    targets
}

fn builtin(code: &str, kind: SearchKind) -> Option<Extractor> {
    let source = match code {
        "_text" => Source::Narrative,
        "_content" => Source::Content,
        "_filter" => Source::Filter,
        "_query" => Source::Query,
        "age" => Source::Age,
        "birthOrderBoolean" => Source::MultipleBirth,
        "near" => Source::Near,
        _ => return None,
    };
    Some(Extractor {
        kind,
        source,
        targets: Vec::new(),
    })
}

fn compile_composite(
    parameter: &Value,
    focus: &str,
    component_types: &BTreeMap<String, String>,
) -> Option<Extractor> {
    let focus = refhir_fhirpath::parse(focus).ok()?;
    if focus.calls("memberOf") {
        return None;
    }
    let mut parts = Vec::new();
    for component in parameter
        .get("component")
        .and_then(|value| value.as_array())?
    {
        let definition = component
            .get("definition")
            .and_then(|value| value.as_str())?;
        let url = definition.split('|').next().unwrap_or(definition);
        let param_type = component_types.get(url)?;
        let kind = search_kind(param_type)?;
        if matches!(kind, SearchKind::Composite | SearchKind::Special) {
            return None;
        }
        let expr = refhir_fhirpath::parse(component.get("expression")?.as_str()?).ok()?;
        if expr.calls("memberOf") {
            return None;
        }
        parts.push(Part { kind, expr });
    }
    if parts.is_empty() {
        return None;
    }
    Some(Extractor {
        kind: SearchKind::Composite,
        source: Source::Composite { focus, parts },
        targets: Vec::new(),
    })
}

fn search_kind(param_type: &str) -> Option<SearchKind> {
    Some(match param_type {
        "token" => SearchKind::Token,
        "string" => SearchKind::String,
        "reference" => SearchKind::Reference,
        "date" => SearchKind::Date,
        "uri" => SearchKind::Uri,
        "number" => SearchKind::Number,
        "quantity" => SearchKind::Quantity,
        "composite" => SearchKind::Composite,
        "special" => SearchKind::Special,
        _ => return None,
    })
}

fn probe_kind(kind: Option<SearchKind>, query: &str) -> Probe<'_> {
    match kind {
        Some(SearchKind::Token | SearchKind::Reference | SearchKind::Uri) => Probe::Token(query),
        Some(SearchKind::String) => Probe::Prefix(query),
        Some(SearchKind::Date) => {
            let (op, rest) = split_prefix(query);
            match op {
                "eq" if !query.starts_with("eq") => Probe::Prefix(query),
                "eq" => Probe::Prefix(rest),
                "gt" | "ge" | "sa" => Probe::AtLeast(rest),
                "lt" | "le" | "eb" => Probe::AtMost(rest),
                _ => Probe::Scan,
            }
        }
        Some(SearchKind::Number) => number_probe(query),
        Some(SearchKind::Quantity) => quantity_probe(query),
        _ => Probe::Scan,
    }
}

fn number_values(
    extractor: &Extractor,
    resource: &Value,
    path_model: &Model,
    host: &dyn Host,
) -> Vec<f64> {
    let Source::Path(expr) = &extractor.source else {
        return Vec::new();
    };
    numbers_of(&eval_items(expr, resource, path_model, host))
}

fn quantity_values(
    extractor: &Extractor,
    resource: &Value,
    path_model: &Model,
    host: &dyn Host,
) -> Vec<Span> {
    let Source::Path(expr) = &extractor.source else {
        return Vec::new();
    };
    quantities_of(&eval_items(expr, resource, path_model, host))
}

fn number_probe(query: &str) -> Probe<'_> {
    let (op, rest) = split_prefix(query);
    let Some(span) = number_span(rest) else {
        return Probe::Scan;
    };
    bound_probe(op, &span, "")
}

fn quantity_probe(query: &str) -> Probe<'_> {
    let (op, rest) = split_prefix(query);
    let Some((span, _system, code)) = quantity_query(rest) else {
        return Probe::Scan;
    };
    let Some(code) = code else {
        return Probe::Range(Interval::all());
    };
    bound_probe(op, &span, &code)
}

fn bound_probe(op: &str, span: &Span, code: &str) -> Probe<'static> {
    let (start, end) = match op {
        "gt" | "sa" => (Some(span.exact.next_up()), None),
        "ge" => (Some(span.exact), None),
        "lt" | "eb" => (None, Some(span.exact)),
        "le" => (None, Some(span.exact.next_up())),
        "eq" => (Some(span.start), Some(span.end)),
        "ap" => {
            let width = (span.exact.abs() * 0.1).max(span.end - span.start);
            (
                Some(span.exact - width),
                Some((span.exact + width).next_up()),
            )
        }
        _ => return Probe::Scan,
    };
    let interval = if code.is_empty() {
        number_range(start, end)
    } else {
        quantity_range(code, start, end)
    };
    Probe::Range(interval)
}

fn display_strings(items: &[refhir_fhirpath::Value]) -> Vec<String> {
    let mut found = Vec::new();
    for item in items {
        let refhir_fhirpath::Value::Node { json, .. } = item else {
            continue;
        };
        collect_display(json, &mut found);
    }
    found
}

fn collect_display(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for name in ["display", "text"] {
                if let Some(text) = map.get(name).and_then(|item| item.as_str()) {
                    found.push(text.to_string());
                }
            }
            for item in map.values() {
                collect_display(item, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_display(item, found);
            }
        }
        _ => {}
    }
}

fn identifier_of_type(node: &Value, system: &str, code: &str, value: &str) -> bool {
    let Some(actual) = node.get("value").and_then(|item| item.as_str()) else {
        return false;
    };
    if actual != value {
        return false;
    }
    let Some(codings) = node
        .pointer("/type/coding")
        .and_then(|item| item.as_array())
    else {
        return false;
    };
    codings.iter().any(|coding| {
        coding.get("system").and_then(|item| item.as_str()) == Some(system)
            && coding.get("code").and_then(|item| item.as_str()) == Some(code)
    })
}

fn reference_identifier(node: &Value, system: &str, value: &str) -> bool {
    let Some(identifier) = node.get("identifier") else {
        return false;
    };
    let actual = identifier.get("value").and_then(|item| item.as_str());
    if actual != Some(value) {
        return false;
    }
    if system.is_empty() {
        return true;
    }
    identifier.get("system").and_then(|item| item.as_str()) == Some(system)
}

fn path_strings(
    kind: SearchKind,
    expr: &Expr,
    resource: &Value,
    path_model: &Model,
    host: &dyn Host,
) -> Vec<String> {
    if !matches!(
        kind,
        SearchKind::Token
            | SearchKind::String
            | SearchKind::Reference
            | SearchKind::Date
            | SearchKind::Uri
    ) {
        return Vec::new();
    }
    eval_items(expr, resource, path_model, host)
        .iter()
        .flat_map(|item| encode_value(kind, item))
        .collect()
}

/// Whether any of `expected` matches what the expression finds in the resource, which is read
/// once whatever the number of values.
fn path_match<S: AsRef<str>>(
    kind: SearchKind,
    expr: &Expr,
    op: &str,
    resource: &Value,
    expected: &[S],
    path_model: &Model,
    host: &dyn Host,
) -> bool {
    let items = eval_items(expr, resource, path_model, host);
    let encoded = |kind| {
        items
            .iter()
            .flat_map(|item| encode_value(kind, item))
            .collect::<Vec<_>>()
    };
    match kind {
        SearchKind::Token | SearchKind::Reference => {
            let values = encoded(kind);
            expected
                .iter()
                .any(|wanted| token_match(op, &values, wanted.as_ref()))
        }
        SearchKind::Uri => {
            let values = encoded(kind);
            expected
                .iter()
                .any(|wanted| uri_match(op, &values, wanted.as_ref()))
        }
        SearchKind::String => {
            let values = encoded(kind);
            expected.iter().any(|wanted| {
                values
                    .iter()
                    .any(|text| text_match(op, text, wanted.as_ref()))
            })
        }
        SearchKind::Date => {
            let values = encoded(kind);
            expected.iter().any(|wanted| {
                values
                    .iter()
                    .any(|text| date_match(op, text, wanted.as_ref()))
            })
        }
        SearchKind::Number => {
            let numbers = numbers_of(&items);
            expected
                .iter()
                .any(|wanted| number_match(op, &numbers, wanted.as_ref()))
        }
        SearchKind::Quantity => {
            let quantities = quantities_of(&items);
            expected
                .iter()
                .any(|wanted| quantity_match(op, &quantities, wanted.as_ref()))
        }
        SearchKind::Composite | SearchKind::Special => false,
    }
}

fn eval_items(
    expr: &Expr,
    resource: &Value,
    path_model: &Model,
    host: &dyn Host,
) -> Vec<refhir_fhirpath::Value> {
    refhir_fhirpath::evaluate(expr, resource, path_model, host).unwrap_or_default()
}

fn focus_nodes(expr: &Expr, resource: &Value, path_model: &Model, host: &dyn Host) -> Vec<Value> {
    eval_items(expr, resource, path_model, host)
        .into_iter()
        .filter_map(|item| match item {
            refhir_fhirpath::Value::Node { json, .. } => Some(json),
            _ => None,
        })
        .collect()
}

fn composite_match(
    focus: &Expr,
    parts: &[Part],
    resource: &Value,
    expected: &str,
    path_model: &Model,
    host: &dyn Host,
) -> bool {
    let pieces: Vec<&str> = expected.split('$').collect();
    if pieces.len() != parts.len() {
        return false;
    }
    focus_nodes(focus, resource, path_model, host)
        .into_iter()
        .any(|node| {
            parts.iter().zip(pieces.iter()).all(|(part, piece)| {
                path_match(
                    part.kind,
                    &part.expr,
                    "eq",
                    &node,
                    &[*piece],
                    path_model,
                    host,
                )
            })
        })
}

fn encode_value(kind: SearchKind, item: &refhir_fhirpath::Value) -> Vec<String> {
    let json = match item {
        refhir_fhirpath::Value::Node { json, .. } => json,
        refhir_fhirpath::Value::String(text) => return vec![text.clone()],
        refhir_fhirpath::Value::Boolean { value, .. } => return vec![value.to_string()],
        refhir_fhirpath::Value::Date(_) | refhir_fhirpath::Value::DateTime(_) => {
            return vec![item.render().trim_start_matches('@').to_string()];
        }
        _ => return Vec::new(),
    };
    match kind {
        SearchKind::Token => token_json(json),
        SearchKind::String => string_json(json),
        SearchKind::Reference => reference_tokens(json, &[]),
        SearchKind::Date => date_json(json),
        SearchKind::Uri => string_tokens(json, &[]),
        SearchKind::Number | SearchKind::Quantity | SearchKind::Composite | SearchKind::Special => {
            Vec::new()
        }
    }
}

fn token_match(op: &str, values: &[String], expected: &str) -> bool {
    match op {
        "eq" | "sw" => values.iter().any(|token| token == expected),
        "ne" => values.iter().all(|token| token != expected),
        "co" => values.iter().any(|token| token.contains(expected)),
        _ => false,
    }
}

fn uri_match(op: &str, values: &[String], expected: &str) -> bool {
    match op {
        "eq" => values.iter().any(|token| token == expected),
        "ne" => !values.iter().any(|token| token == expected),
        "sw" => values.iter().any(|token| token.starts_with(expected)),
        "ew" => values.iter().any(|token| token.ends_with(expected)),
        "co" => values.iter().any(|token| token.contains(expected)),
        _ => false,
    }
}

fn text_match(op: &str, text: &str, expected: &str) -> bool {
    let text = text.to_lowercase();
    let expected = expected.to_lowercase();
    match op {
        "eq" | "sw" => text.starts_with(&expected),
        "co" => text.contains(&expected),
        "ew" => text.ends_with(&expected),
        "ne" => !text.starts_with(&expected),
        _ => false,
    }
}

fn content_match(op: &str, resource: &Value, expected: &str) -> bool {
    let needle = expected.to_lowercase();
    let hit = resource_strings(resource).any(|text| text.to_lowercase().contains(&needle));
    match op {
        "eq" | "sw" | "co" => hit,
        "ne" => !hit,
        _ => false,
    }
}

fn resource_strings(value: &Value) -> impl Iterator<Item = String> {
    let mut found = Vec::new();
    collect_strings(value, &mut found);
    found.into_iter()
}

fn collect_strings(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::String(text) => found.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, found);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_strings(item, found);
            }
        }
        _ => {}
    }
}

fn narrative_text(resource: &Value) -> String {
    resource
        .get("text")
        .and_then(|text| text.get("div"))
        .and_then(|div| div.as_str())
        .map(strip_tags)
        .unwrap_or_default()
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for ch in html.chars() {
        if ch == '<' {
            inside = true;
            out.push(' ');
            continue;
        }
        if ch == '>' {
            inside = false;
            continue;
        }
        if !inside {
            out.push(ch);
        }
    }
    out
}

fn age_years(resource: &Value, host: &dyn Host) -> Vec<f64> {
    if resource
        .get("deceasedBoolean")
        .and_then(|value| value.as_bool())
        == Some(true)
        || resource
            .get("deceasedDateTime")
            .and_then(|value| value.as_str())
            .is_some()
    {
        return Vec::new();
    }
    let Some(birth) = resource.get("birthDate").and_then(|value| value.as_str()) else {
        return Vec::new();
    };
    let Some(years) = completed_years(birth, &host.now()) else {
        return Vec::new();
    };
    vec![years]
}

fn completed_years(birth: &str, now: &refhir_fhirpath::Temporal) -> Option<f64> {
    let (year, month, day) = parse_ymd(birth)?;
    let now_year = now.year?;
    let now_month = now.month.unwrap_or(1);
    let now_day = now.day.unwrap_or(1);
    let mut age = now_year - year;
    if (now_month, now_day) < (month, day) {
        age -= 1;
    }
    if age < 0 {
        return None;
    }
    Some(age as f64)
}

fn multiple_birth_tokens(resource: &Value) -> Vec<String> {
    if let Some(value) = resource
        .get("multipleBirthBoolean")
        .and_then(|item| item.as_bool())
    {
        return vec![value.to_string()];
    }
    if resource
        .get("multipleBirthInteger")
        .and_then(|item| item.as_i64())
        .is_some()
    {
        return vec!["true".to_string()];
    }
    Vec::new()
}

fn near_match(resource: &Value, expected: &str) -> bool {
    let Some(position) = resource.get("position") else {
        return false;
    };
    let Some(lat) = position.get("latitude").and_then(|value| value.as_f64()) else {
        return false;
    };
    let Some(lon) = position.get("longitude").and_then(|value| value.as_f64()) else {
        return false;
    };
    let mut parts = expected.split('|');
    let Some(query_lat) = parts.next().and_then(|text| text.parse::<f64>().ok()) else {
        return false;
    };
    let Some(query_lon) = parts.next().and_then(|text| text.parse::<f64>().ok()) else {
        return false;
    };
    let distance = parts
        .next()
        .and_then(|text| text.parse::<f64>().ok())
        .unwrap_or(10.0);
    let units = parts.next().unwrap_or("km");
    let limit_km = if units == "mi" {
        distance * 1.60934
    } else {
        distance
    };
    haversine_km(lat, lon, query_lat, query_lon) <= limit_km
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let radius = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * radius * a.sqrt().asin()
}

struct Span {
    start: f64,
    end: f64,
    exact: f64,
    system: Option<String>,
    code: Option<String>,
}

fn number_match(op: &str, values: &[f64], expected: &str) -> bool {
    let (prefix, number) = prefixed(op, expected);
    let Some(span) = number_span(number) else {
        return false;
    };
    values
        .iter()
        .any(|value| compare_number(prefix, *value, &span))
}

fn quantity_match(op: &str, values: &[Span], expected: &str) -> bool {
    let (prefix, rest) = prefixed(op, expected);
    let Some((span, system, code)) = quantity_query(rest) else {
        return false;
    };
    values.iter().any(|value| {
        unit_ok(value, system.as_deref(), code.as_deref()) && compare_span(prefix, value, &span)
    })
}

fn numbers_of(items: &[refhir_fhirpath::Value]) -> Vec<f64> {
    items.iter().filter_map(as_f64).collect()
}

fn quantities_of(items: &[refhir_fhirpath::Value]) -> Vec<Span> {
    let mut spans = Vec::new();
    for item in items {
        match item {
            refhir_fhirpath::Value::Quantity { value, unit, .. } => {
                if let Ok(exact) = value.to_string().parse::<f64>() {
                    spans.push(point_span(exact, None, unit_code(unit)));
                }
            }
            refhir_fhirpath::Value::Integer(value) => {
                spans.push(point_span(*value as f64, None, None))
            }
            refhir_fhirpath::Value::Decimal { value, .. } => {
                if let Ok(exact) = value.to_string().parse::<f64>() {
                    spans.push(point_span(exact, None, None));
                }
            }
            refhir_fhirpath::Value::Node { json, .. } => spans.extend(spans_from_json(json)),
            _ => {}
        }
    }
    spans
}

fn spans_from_json(json: &Value) -> Vec<Span> {
    if json.get("low").is_some() || json.get("high").is_some() {
        let low = json.get("low").and_then(quantity_point);
        let high = json.get("high").and_then(quantity_point);
        let system = low
            .as_ref()
            .and_then(|item| item.1.clone())
            .or_else(|| high.as_ref().and_then(|item| item.1.clone()));
        let code = low
            .as_ref()
            .and_then(|item| item.2.clone())
            .or_else(|| high.as_ref().and_then(|item| item.2.clone()));
        return vec![Span {
            start: low.as_ref().map(|item| item.0).unwrap_or(f64::NEG_INFINITY),
            end: high.as_ref().map(|item| item.0).unwrap_or(f64::INFINITY),
            exact: low.as_ref().map(|item| item.0).unwrap_or(0.0),
            system,
            code,
        }];
    }
    quantity_point(json)
        .map(|(exact, system, code)| vec![point_span(exact, system, code)])
        .unwrap_or_default()
}

fn quantity_point(json: &Value) -> Option<(f64, Option<String>, Option<String>)> {
    let exact = json.get("value").and_then(as_json_f64)?;
    let system = json
        .get("system")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let code = json
        .get("code")
        .or_else(|| json.get("unit"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    Some((exact, system, code))
}

fn point_span(exact: f64, system: Option<String>, code: Option<String>) -> Span {
    Span {
        start: exact,
        end: exact,
        exact,
        system,
        code,
    }
}

fn unit_code(unit: &str) -> Option<String> {
    if unit.is_empty() || unit == "1" {
        None
    } else {
        Some(unit.to_string())
    }
}

fn unit_ok(value: &Span, system: Option<&str>, code: Option<&str>) -> bool {
    if let Some(code) = code {
        if value.code.as_deref() != Some(code) {
            return false;
        }
    }
    if let Some(system) = system {
        if value.system.as_deref() != Some(system) {
            return false;
        }
    }
    true
}

fn quantity_query(text: &str) -> Option<(Span, Option<String>, Option<String>)> {
    let mut parts = text.split('|');
    let number = parts.next()?;
    let second = parts.next();
    let third = parts.next();
    let (system, code) = match (second, third) {
        (None, _) => (None, None),
        (Some(system), Some(code)) => (empty_none(system), empty_none(code)),
        (Some(code), None) => {
            if code.contains("://") || code.starts_with("urn:") {
                (Some(code.to_string()), None)
            } else {
                (None, empty_none(code))
            }
        }
    };
    let span = number_span(number)?;
    Some((span, system, code))
}

fn number_span(text: &str) -> Option<Span> {
    let exact = text.parse::<f64>().ok()?;
    let decimals = text.split('.').nth(1).map(|part| part.len()).unwrap_or(0) as i32;
    let delta = 0.5 * 10f64.powi(-decimals);
    Some(Span {
        start: exact - delta,
        end: exact + delta,
        exact,
        system: None,
        code: None,
    })
}

fn compare_number(op: &str, value: f64, span: &Span) -> bool {
    compare_span(
        op,
        &Span {
            start: value,
            end: value,
            exact: value,
            system: None,
            code: None,
        },
        span,
    )
}

fn compare_span(op: &str, value: &Span, query: &Span) -> bool {
    match op {
        "eq" => value.start < query.end && value.end >= query.start,
        "ne" => !(value.start < query.end && value.end >= query.start),
        "gt" | "sa" => value.end > query.exact,
        "lt" | "eb" => value.start < query.exact,
        "ge" => value.end >= query.exact,
        "le" => value.start <= query.exact,
        "ap" => {
            let width = (query.exact.abs() * 0.1).max(query.end - query.start);
            (value.exact - query.exact).abs() <= width
        }
        _ => false,
    }
}

fn date_match(op: &str, stored: &str, expected: &str) -> bool {
    let (prefix, query) = prefixed(op, expected);
    let Some(stored) = date_period(stored) else {
        return false;
    };
    let Some(query) = date_period(query) else {
        return false;
    };
    match prefix {
        "eq" | "ap" => stored.0 < query.1 && stored.1 > query.0,
        "ne" => !(stored.0 < query.1 && stored.1 > query.0),
        "gt" | "sa" => stored.0 >= query.1,
        "lt" | "eb" => stored.1 <= query.0,
        "ge" => stored.1 > query.0,
        "le" => stored.0 < query.1,
        _ => false,
    }
}

fn prefixed<'a>(op: &'a str, expected: &'a str) -> (&'a str, &'a str) {
    let (prefix, rest) = split_prefix(expected);
    if prefix != "eq" || expected.starts_with("eq") {
        (prefix, rest)
    } else {
        (op, expected)
    }
}

fn date_period(text: &str) -> Option<(i64, i64)> {
    let text = text.trim().trim_start_matches('@');
    let (date, time) = text.split_once('T').unwrap_or((text, ""));
    let mut parts = date.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month = parts.next().and_then(|text| text.parse::<u8>().ok());
    let day = parts.next().and_then(|text| text.parse::<u8>().ok());
    let start_month = month.unwrap_or(1);
    let start_day = day.unwrap_or(1);
    let start = refhir_fhirpath::days_from_civil(year, start_month, start_day) * 86_400;
    let end = if time.is_empty() {
        if month.is_none() {
            refhir_fhirpath::days_from_civil(year + 1, 1, 1) * 86_400
        } else if day.is_none() {
            let (next_year, next_month) = if start_month == 12 {
                (year + 1, 1)
            } else {
                (year, start_month + 1)
            };
            refhir_fhirpath::days_from_civil(next_year, next_month, 1) * 86_400
        } else {
            start + 86_400
        }
    } else {
        start + clock_seconds(time)
    };
    let start = if time.is_empty() {
        start
    } else {
        start + clock_start(time)
    };
    Some((start, end))
}

fn clock_start(time: &str) -> i64 {
    let (hour, minute, second) = clock_parts(time);
    hour * 3600 + minute * 60 + second
}

fn clock_seconds(time: &str) -> i64 {
    let (hour, minute, second) = clock_parts(time);
    // Fractions of a second and whole seconds both end one second after they start.
    let step = if time.contains('.') || time.split(':').count() >= 3 {
        1
    } else if time.contains(':') {
        60
    } else {
        3600
    };
    hour * 3600 + minute * 60 + second + step
}

fn clock_parts(time: &str) -> (i64, i64, i64) {
    let time = time.split(['+', 'Z']).next().unwrap_or(time);
    let time = time.split('-').next().unwrap_or(time);
    let mut parts = time.split(':');
    let hour = parts.next().and_then(|text| text.parse().ok()).unwrap_or(0);
    let minute = parts.next().and_then(|text| text.parse().ok()).unwrap_or(0);
    let second = parts
        .next()
        .and_then(|text| text.split('.').next())
        .and_then(|text| text.parse().ok())
        .unwrap_or(0);
    (hour, minute, second)
}

fn split_prefix(query: &str) -> (&str, &str) {
    for prefix in ["eq", "ne", "gt", "lt", "ge", "le", "sa", "eb", "ap"] {
        if let Some(rest) = query.strip_prefix(prefix) {
            if rest.starts_with(|ch: char| ch.is_ascii_digit() || ch == '-' || ch == '.') {
                return (prefix, rest);
            }
        }
    }
    ("eq", query)
}

fn parse_ymd(text: &str) -> Option<(i32, u8, u8)> {
    let mut parts = text.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next().and_then(|text| text.parse().ok()).unwrap_or(1);
    let day = parts.next().and_then(|text| text.parse().ok()).unwrap_or(1);
    Some((year, month, day))
}

fn as_f64(item: &refhir_fhirpath::Value) -> Option<f64> {
    match item {
        refhir_fhirpath::Value::Integer(value) => Some(*value as f64),
        refhir_fhirpath::Value::Decimal { value, .. } => value.to_string().parse().ok(),
        refhir_fhirpath::Value::Quantity { value, .. } => value.to_string().parse().ok(),
        refhir_fhirpath::Value::String(text) => text.parse().ok(),
        refhir_fhirpath::Value::Node { json, .. } => as_json_f64(json),
        _ => None,
    }
}

fn as_json_f64(json: &Value) -> Option<f64> {
    json.as_f64()
        .or_else(|| json.as_i64().map(|value| value as f64))
        .or_else(|| json.get("value").and_then(|value| value.as_f64()))
        .or_else(|| {
            json.get("value")
                .and_then(|value| value.as_i64())
                .map(|value| value as f64)
        })
}

fn empty_none(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn token_json(json: &Value) -> Vec<String> {
    if json.as_bool().is_some() {
        return boolean_tokens(json, &[]);
    }
    if json.get("value").and_then(|item| item.as_str()).is_some() {
        return identifier_tokens(json, &[]);
    }
    if json.get("coding").is_some() || json.get("code").is_some() {
        return coding_tokens(json, &[]);
    }
    string_tokens(json, &[])
}

fn string_json(json: &Value) -> Vec<String> {
    if json.get("family").is_some() || json.get("given").is_some() || json.get("text").is_some() {
        let names = name_strings(json, &[]);
        if !names.is_empty() {
            return names;
        }
    }
    string_tokens(json, &[])
}

fn date_json(json: &Value) -> Vec<String> {
    if let Some(text) = json.as_str() {
        return vec![text.to_string()];
    }
    let mut values = Vec::new();
    if let Some(start) = json.get("start").and_then(|item| item.as_str()) {
        values.push(start.to_string());
    }
    if let Some(end) = json.get("end").and_then(|item| item.as_str()) {
        values.push(end.to_string());
    }
    values
}

fn token_rows(index: &IndexId, id: &str, tokens: &[String]) -> Vec<(IndexId, Vec<u8>)> {
    tokens
        .iter()
        .map(|token| (index.clone(), token_key(token, id)))
        .collect()
}

fn string_rows(index: &IndexId, id: &str, values: &[String]) -> Vec<(IndexId, Vec<u8>)> {
    values
        .iter()
        .map(|value| (index.clone(), string_key(value, id)))
        .collect()
}

fn identifier_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    let mut tokens = Vec::new();
    for node in nodes_at(resource, path) {
        let Some(value) = node.get("value").and_then(|item| item.as_str()) else {
            continue;
        };
        tokens.push(value.to_string());
        if let Some(system) = node.get("system").and_then(|item| item.as_str()) {
            tokens.push(format!("{system}|{value}"));
        }
    }
    tokens
}

fn coding_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    let mut tokens = Vec::new();
    for node in nodes_at(resource, path) {
        if let Some(codings) = node.get("coding").and_then(|item| item.as_array()) {
            for coding in codings {
                push_coding(&mut tokens, coding);
            }
            continue;
        }
        push_coding(&mut tokens, node);
    }
    tokens
}

fn push_coding(tokens: &mut Vec<String>, node: &Value) {
    let Some(code) = node.get("code").and_then(|item| item.as_str()) else {
        return;
    };
    tokens.push(code.to_string());
    if let Some(system) = node.get("system").and_then(|item| item.as_str()) {
        tokens.push(format!("{system}|{code}"));
    }
}

fn boolean_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    nodes_at(resource, path)
        .into_iter()
        .filter_map(|node| node.as_bool())
        .map(|value| {
            if value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        })
        .collect()
}

fn string_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    nodes_at(resource, path)
        .into_iter()
        .filter_map(|node| node.as_str().map(|text| text.to_string()))
        .collect()
}

fn name_strings(resource: &Value, path: &[String]) -> Vec<String> {
    let mut parts = Vec::new();
    for node in nodes_at(resource, path) {
        let Some(name) = node.as_object() else {
            continue;
        };
        if let Some(family) = name.get("family").and_then(|value| value.as_str()) {
            parts.push(family.to_string());
        }
        if let Some(text) = name.get("text").and_then(|value| value.as_str()) {
            parts.push(text.to_string());
        }
        if let Some(given) = name.get("given").and_then(|value| value.as_array()) {
            for item in given {
                if let Some(text) = item.as_str() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    parts
}

fn reference_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    nodes_at(resource, path)
        .into_iter()
        .filter_map(|node| {
            node.get("reference")
                .and_then(|item| item.as_str())
                .or_else(|| node.as_str())
                .map(|text| text.to_string())
        })
        .collect()
}

fn nodes_at<'a>(root: &'a Value, path: &[String]) -> Vec<&'a Value> {
    let mut current = vec![root];
    for segment in path {
        let mut next = Vec::new();
        for value in current {
            match value {
                Value::Array(items) => {
                    for item in items {
                        if let Some(child) = item.get(segment) {
                            next.push(child);
                        }
                    }
                }
                other => {
                    if let Some(child) = other.get(segment) {
                        next.push(child);
                    }
                }
            }
        }
        current = next;
    }
    let mut flat = Vec::new();
    for value in current {
        if let Value::Array(items) = value {
            flat.extend(items.iter());
        } else {
            flat.push(value);
        }
    }
    flat
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use refhir_fhirpath::{EmptyHost, Model};
    use serde_json::{json, Value};

    use refhir_types::{token_interval, IndexId};

    use super::{compile_parameter, date_match, number_match, Probe, ReferenceId};

    #[test]
    fn every_r4_search_parameter_compiles() {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/r4"));
        let mut parameters = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !name.starts_with("SearchParameter-") || !name.ends_with(".json") {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            parameters.push(serde_json::from_str::<Value>(&text).unwrap());
        }
        assert!(parameters.len() > 1000);
        let mut kinds = BTreeMap::new();
        for parameter in &parameters {
            let url = parameter["url"].as_str().unwrap().to_string();
            let param_type = parameter["type"].as_str().unwrap().to_string();
            kinds.entry(url).or_insert(param_type);
        }
        let mut failed = Vec::new();
        for parameter in &parameters {
            if compile_parameter(parameter, &kinds).is_none() {
                failed.push(format!(
                    "{} {} {}",
                    parameter["code"].as_str().unwrap_or(""),
                    parameter["type"].as_str().unwrap_or(""),
                    parameter["expression"].as_str().unwrap_or("")
                ));
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    #[test]
    fn number_equality_uses_decimal_precision() {
        assert!(number_match("eq", &[100.0], "100"));
        assert!(number_match("eq", &[99.5], "100"));
        assert!(!number_match("eq", &[100.5], "100"));
        assert!(number_match("eq", &[100.0], "100.00"));
        assert!(!number_match("eq", &[100.01], "100.00"));
        assert!(number_match("gt", &[100.1], "100"));
        assert!(!number_match("gt", &[100.0], "100"));
    }

    #[test]
    fn number_and_quantity_prefixes_use_an_index_range() {
        let host = EmptyHost;
        let model = Model::new();
        let number = compile_parameter(
            &json!({
                "code": "probability",
                "type": "number",
                "expression": "RiskAssessment.prediction.probabilityDecimal"
            }),
            &BTreeMap::new(),
        )
        .unwrap();
        let above = json!({"resourceType": "RiskAssessment", "prediction": [{"probabilityDecimal": 100.1}]});
        let equal = json!({"resourceType": "RiskAssessment", "prediction": [{"probabilityDecimal": 100.0}]});
        let index = IndexId::new("RiskAssessment/probability");
        let above_key = &number.keys(&above, "a", &index, &model, &host)[0].1;
        let equal_key = &number.keys(&equal, "b", &index, &model, &host)[0].1;
        let Probe::Range(interval) = number.probe("gt100") else {
            panic!("gt100 should be a range");
        };
        assert!(interval.contains(above_key));
        assert!(!interval.contains(equal_key));

        let quantity = compile_parameter(
            &json!({
                "code": "value-quantity",
                "type": "quantity",
                "expression": "Observation.valueQuantity"
            }),
            &BTreeMap::new(),
        )
        .unwrap();
        let resource = json!({
            "resourceType": "Observation",
            "valueQuantity": {"value": 5.4, "system": "http://unitsofmeasure.org", "code": "mg"}
        });
        assert!(quantity.matches(&resource, "5.4||mg", &model, &host));
        let keys = quantity.keys(
            &resource,
            "o1",
            &IndexId::new("Observation/value-quantity"),
            &model,
            &host,
        );
        assert_eq!(keys.len(), 1, "quantity was not indexed");
        let key = &keys[0].1;
        let Probe::Range(interval) = quantity.probe("gt5||mg") else {
            panic!("gt5||mg should be a range");
        };
        assert!(interval.contains(key));
        let Probe::Range(low) = quantity.probe("gt6||mg") else {
            panic!("gt6||mg should be a range");
        };
        assert!(!low.contains(key));
    }

    #[test]
    fn quantity_requires_the_code_when_one_is_given() {
        let host = EmptyHost;
        let model = Model::new();
        let parameter = json!({
            "code": "value-quantity",
            "type": "quantity",
            "expression": "Observation.valueQuantity"
        });
        let extractor = compile_parameter(&parameter, &BTreeMap::new()).unwrap();
        let resource = json!({
            "resourceType": "Observation",
            "valueQuantity": {"value": 5.4, "system": "http://unitsofmeasure.org", "code": "mg"}
        });
        assert!(extractor.matches(&resource, "5.4|http://unitsofmeasure.org|mg", &model, &host));
        assert!(extractor.matches(&resource, "5.4||mg", &model, &host));
        assert!(!extractor.matches(&resource, "5.4||g", &model, &host));
    }

    #[test]
    fn composite_matches_one_item() {
        let host = EmptyHost;
        let model = Model::new();
        let mut kinds = BTreeMap::new();
        kinds.insert(
            "http://example.org/SearchParameter/code".to_string(),
            "token".to_string(),
        );
        kinds.insert(
            "http://example.org/SearchParameter/value".to_string(),
            "token".to_string(),
        );
        let parameter = json!({
            "code": "code-value",
            "type": "composite",
            "expression": "Observation.component",
            "component": [
                {"definition": "http://example.org/SearchParameter/code", "expression": "code"},
                {"definition": "http://example.org/SearchParameter/value", "expression": "value"}
            ]
        });
        let extractor = compile_parameter(&parameter, &kinds).unwrap();
        let resource = json!({
            "resourceType": "Observation",
            "component": [
                {"code": {"coding": [{"code": "a"}]}, "value": {"coding": [{"code": "1"}]}},
                {"code": {"coding": [{"code": "b"}]}, "value": {"coding": [{"code": "2"}]}}
            ]
        });
        assert!(!extractor.matches(&resource, "a$2", &model, &host));
        assert!(extractor.matches(&resource, "b$2", &model, &host));
    }

    #[test]
    fn uri_match_is_exact() {
        let host = EmptyHost;
        let model = Model::new();
        let parameter = json!({
            "code": "url",
            "type": "uri",
            "expression": "ValueSet.url"
        });
        let extractor = compile_parameter(&parameter, &BTreeMap::new()).unwrap();
        let resource = json!({"resourceType": "ValueSet", "url": "http://example.org/vs"});
        assert!(extractor.matches(&resource, "http://example.org/vs", &model, &host));
        assert!(!extractor.matches(&resource, "http://example.org", &model, &host));
    }

    #[test]
    fn date_comparator_is_not_part_of_the_value() {
        assert!(date_match("eq", "1990-01-01", "1990-01-01"));
        assert!(date_match("eq", "1990-06-01", "1990"));
        assert!(!date_match("eq", "1990-01-01T10:00:00", "gt1990-01-01"));
        assert!(date_match("eq", "1990-01-02", "gt1990-01-01"));
    }

    fn reference_parameter(kind: &str, targets: Value) -> super::Extractor {
        compile_parameter(
            &json!({
                "code": "subject",
                "type": kind,
                "expression": "Observation.subject",
                "target": targets
            }),
            &BTreeMap::new(),
        )
        .unwrap()
    }

    fn typed(tokens: &[&str]) -> ReferenceId {
        ReferenceId::Typed(tokens.iter().map(|token| token.to_string()).collect())
    }

    #[test]
    fn a_bare_id_stands_for_a_reference_to_each_type_the_parameter_can_point_at() {
        let one = reference_parameter("reference", json!(["Patient"]));
        assert_eq!(one.reference_id("123"), typed(&["Patient/123"]));
        assert_eq!(one.reference_id("a-1.b"), typed(&["Patient/a-1.b"]));
        // A type listed twice is one type, and the order is the parameter's.
        let several = reference_parameter("reference", json!(["Patient", "Group", "Patient"]));
        assert_eq!(several.reference_id("1"), typed(&["Patient/1", "Group/1"]));
        // Sixteen types is the most a bare id stands for.
        let sixteen: Vec<String> = (0..16).map(|n| format!("T{n}")).collect();
        assert!(matches!(
            reference_parameter("reference", json!(sixteen)).reference_id("1"),
            ReferenceId::Typed(types) if types.len() == 16
        ));
    }

    #[test]
    fn a_parameter_shared_by_several_types_is_narrowed_to_what_each_can_hold() {
        let patient_only = ["Patient".to_string()];
        let patient_or_group = ["Patient".to_string(), "Group".to_string()];
        let any = ["Resource".to_string()];
        let elements = |path: &str| match path {
            "patient" => Some(&patient_only[..]),
            "subject" => Some(&patient_or_group[..]),
            "for" => Some(&any[..]),
            _ => None,
        };
        let parameter = |expression: &str, targets: Value| {
            compile_parameter(
                &json!({
                    "code": "patient",
                    "type": "reference",
                    "target": targets,
                    "expression": expression
                }),
                &BTreeMap::new(),
            )
            .unwrap()
        };
        let narrowed = |expression: &str, targets: Value, base: &str| {
            let mut extractor = parameter(expression, targets);
            extractor.narrow_targets(base, elements);
            extractor.reference_id("1")
        };
        let shared =
            "AllergyIntolerance.patient | Observation.subject.where(resolve() is Patient) \
            | DeviceUseStatement.subject | Task.for";
        let both = || json!(["Patient", "Group"]);
        // The element holds a Patient only.
        assert_eq!(
            narrowed(shared, both(), "AllergyIntolerance"),
            typed(&["Patient/1"])
        );
        // The element holds a Patient or a Group, and the branch keeps the Patients.
        assert_eq!(
            narrowed(shared, both(), "Observation"),
            typed(&["Patient/1"])
        );
        // The element holds either, and nothing is filtered.
        assert_eq!(
            narrowed(shared, both(), "DeviceUseStatement"),
            typed(&["Patient/1", "Group/1"])
        );
        // An element that can point at any resource restricts nothing, and nor does a type the
        // expression does not read from.
        for base in ["Task", "Condition"] {
            assert_eq!(
                narrowed(shared, both(), base),
                typed(&["Patient/1", "Group/1"]),
                "{base}"
            );
        }
        // Branches add up: this type can hold what either of them can.
        assert_eq!(
            narrowed(
                "Observation.patient | Observation.subject.where(resolve() is Group)",
                both(),
                "Observation"
            ),
            typed(&["Patient/1", "Group/1"])
        );
        // A resolved type the parameter does not list leaves the list as it was, and a branch this
        // cannot read (here a `where` on something else) may hold any of them.
        assert_eq!(
            narrowed(
                "Observation.subject.where(resolve() is Device)",
                both(),
                "Observation"
            ),
            typed(&["Patient/1", "Group/1"])
        );
        assert_eq!(
            narrowed(
                "Observation.focus.where(status = 'x')",
                both(),
                "Observation"
            ),
            typed(&["Patient/1", "Group/1"])
        );
        // The listed order is kept, and only reference parameters are touched.
        assert_eq!(
            narrowed(
                "Observation.subject.where(resolve() is Group)",
                json!(["Patient", "Group", "Device"]),
                "Observation"
            ),
            typed(&["Group/1"])
        );
        let mut token = compile_parameter(
            &json!({"code": "x", "type": "token", "target": ["Patient"],
                    "expression": "Observation.patient"}),
            &BTreeMap::new(),
        )
        .unwrap();
        token.narrow_targets("Observation", elements);
        assert_eq!(token.reference_id("1"), ReferenceId::AsWritten);
    }

    #[test]
    fn a_value_that_is_more_than_an_id_is_compared_as_written() {
        let one = reference_parameter("reference", json!(["Patient"]));
        for value in [
            "Patient/123",
            "http://example.org/fhir/Patient/123",
            "urn:uuid:11111111-1111-4111-8111-111111111111",
            "",
            "a b",
        ] {
            assert_eq!(one.reference_id(value), ReferenceId::AsWritten, "{value}");
            assert!(!matches!(one.probe(value), Probe::Tokens(_)), "{value}");
        }
        // Only a reference parameter reads an id as a reference.
        let token = reference_parameter("token", json!(["Patient"]));
        assert_eq!(token.reference_id("123"), ReferenceId::AsWritten);
    }

    #[test]
    fn a_bare_id_the_parameter_cannot_place_is_open() {
        assert_eq!(
            reference_parameter("reference", json!([])).reference_id("1"),
            ReferenceId::Open(0)
        );
        let compiled = compile_parameter(
            &json!({"code": "x", "type": "reference", "expression": "Observation.subject"}),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(compiled.reference_id("1"), ReferenceId::Open(0));
        let any: Vec<String> = (0..17).map(|n| format!("T{n}")).collect();
        assert_eq!(
            reference_parameter("reference", json!(any)).reference_id("1"),
            ReferenceId::Open(17)
        );
        // What is not an id is still read as written.
        assert_eq!(
            reference_parameter("reference", json!([])).reference_id("Patient/1"),
            ReferenceId::AsWritten
        );
    }

    #[test]
    fn a_bare_id_is_matched_and_probed_like_each_of_its_typed_references() {
        let host = EmptyHost;
        let model = Model::new();
        let parameter = reference_parameter("reference", json!(["Patient", "Group"]));
        let patient = json!({"resourceType": "Observation", "subject": {"reference": "Patient/1"}});
        let group = json!({"resourceType": "Observation", "subject": {"reference": "Group/1"}});
        let device = json!({"resourceType": "Observation", "subject": {"reference": "Device/1"}});
        let other = json!({"resourceType": "Observation", "subject": {"reference": "Patient/10"}});

        for (resource, expected) in [
            (&patient, true),
            (&group, true),
            (&device, false),
            (&other, false),
        ] {
            assert_eq!(parameter.matches(resource, "1", &model, &host), expected);
            // The negation says the opposite, however the id is given.
            assert_eq!(
                parameter.matches_op(resource, "ne", "1", &model, &host),
                !expected
            );
        }
        assert!(parameter.matches(&patient, "Patient/1", &model, &host));
        assert!(!parameter.matches(&group, "Patient/1", &model, &host));

        // The index has a key for each reference, and the probe covers the ones the id stands for.
        let index = IndexId::new("Observation/subject");
        let Probe::Tokens(tokens) = parameter.probe("1") else {
            panic!("a bare id should probe one token for each type");
        };
        assert_eq!(tokens, ["Patient/1", "Group/1"]);
        let intervals: Vec<_> = tokens.iter().map(|token| token_interval(token)).collect();
        let covered = |resource: &Value| {
            let keys = parameter.keys(resource, "o1", &index, &model, &host);
            keys.iter()
                .any(|(_, key)| intervals.iter().any(|interval| interval.contains(key)))
        };
        assert!(covered(&patient));
        assert!(covered(&group));
        assert!(!covered(&device));
        assert!(!covered(&other));
    }
}
