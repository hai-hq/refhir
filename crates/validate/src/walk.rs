use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use refhir_fhirpath::{Error, ErrorKind, Host, Value};
use serde_json::{Map, Value as Json};

use crate::outcome::{Issue, Outcome, Severity};
use crate::primitive::{check_primitive, is_primitive, type_suffix};
use crate::schema::{Cardinality, ElementDef, Schema, Structure};
use crate::slice::{self, pattern_match};
use crate::terminology::{TermAnswer, Terminology};

const INFRASTRUCTURE: &[&str] = &[
    "resourceType",
    "id",
    "meta",
    "implicitRules",
    "language",
    "text",
    "contained",
    "extension",
    "modifierExtension",
];

/// Where the walker is, in both coordinate systems. `element` is the snapshot path the walk is
/// working from (`Extension`, `Basic.extension`); `instance` is the FHIRPath of the node in the
/// resource being validated (`Basic.extension[0]`). Issues arrive carrying element paths; the
/// innermost frame whose element path they start with turns them into instance paths.
struct Frame {
    element: String,
    instance: String,
}

struct FrameGuard<'a> {
    frames: &'a RefCell<Vec<Frame>>,
}

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        self.frames.borrow_mut().pop();
    }
}

struct State<'a> {
    schema: &'a Schema,
    terms: &'a dyn Terminology,
    host: &'a dyn Host,
    issues: RefCell<Vec<Issue>>,
    depth: Cell<usize>,
    frames: RefCell<Vec<Frame>>,
    /// An invariant is copied into the snapshot of every element of its type, so the element
    /// and the datatype it is walked as both carry it. Evaluated once per node.
    evaluated: RefCell<HashSet<(String, String)>>,
}

const CORE_STRUCTURE: &str = "http://hl7.org/fhir/StructureDefinition/";

pub fn validate(
    instance: &Json,
    profiles: &[String],
    schema: &Schema,
    terms: &dyn Terminology,
    host: &dyn Host,
) -> Outcome {
    let term_host = TermHost { inner: host, terms };
    let state = State {
        schema,
        terms,
        host: &term_host,
        issues: RefCell::new(Vec::new()),
        depth: Cell::new(0),
        frames: RefCell::new(Vec::new()),
        evaluated: RefCell::new(HashSet::new()),
    };
    state.resource(instance, instance, profiles);
    Outcome {
        issues: state.issues.into_inner(),
    }
}

impl State<'_> {
    /// A frame for `element` at the current instance node. Dropped when the node is done.
    fn frame(&self, element: &str, instance: Option<String>) -> FrameGuard<'_> {
        let instance = instance.unwrap_or_else(|| {
            self.frames
                .borrow()
                .last()
                .map(|frame| frame.instance.clone())
                .unwrap_or_else(|| element.to_string())
        });
        self.frames.borrow_mut().push(Frame {
            element: element.to_string(),
            instance,
        });
        FrameGuard {
            frames: &self.frames,
        }
    }

    /// The child of the current node named `segment`, with `[index]` when the element repeats.
    fn child_instance(&self, segment: &str) -> String {
        match self.frames.borrow().last() {
            Some(frame) => format!("{}.{segment}", frame.instance),
            None => segment.to_string(),
        }
    }

    fn locate(&self, expression: String) -> String {
        let frames = self.frames.borrow();
        if expression.is_empty() {
            return frames
                .last()
                .map(|frame| frame.instance.clone())
                .unwrap_or(expression);
        }
        for frame in frames.iter().rev() {
            if expression == frame.element {
                return frame.instance.clone();
            }
            if let Some(rest) = expression.strip_prefix(frame.element.as_str()) {
                if rest.starts_with('.') {
                    return format!("{}{rest}", frame.instance);
                }
            }
        }
        expression
    }

    fn resource(&self, instance: &Json, root: &Json, profiles: &[String]) {
        let Some(type_name) = instance
            .get("resourceType")
            .and_then(|value| value.as_str())
        else {
            self.issue(Severity::Error, "structure", "resourceType is required", "");
            return;
        };
        let mut urls = Vec::new();
        if let Some(structure) = self.schema.base_type(type_name) {
            urls.push(structure.url.clone());
        }
        let stated = if profiles.is_empty() {
            profile_urls(instance)
        } else {
            profiles.to_vec()
        };
        for url in stated {
            if !urls.iter().any(|existing| existing == &url) {
                urls.push(url);
            }
        }
        if urls.is_empty() {
            self.issue(
                Severity::Error,
                "not-found",
                format!("unknown resource type {type_name}"),
                type_name,
            );
            return;
        }
        for url in urls {
            let Some(structure) = self.schema.structure(&url).cloned() else {
                self.issue(
                    Severity::Error,
                    "not-found",
                    format!("unknown profile {url}"),
                    type_name,
                );
                continue;
            };
            if structure.type_name != type_name && structure.kind == "resource" {
                self.issue(
                    Severity::Error,
                    "structure",
                    format!("profile {url} is not a {type_name}"),
                    type_name,
                );
                continue;
            }
            let root_path = structure.root_path().to_string();
            self.walk_object(instance, &structure, &root_path, root, instance);
        }
    }

    fn walk_object(
        &self,
        value: &Json,
        structure: &Structure,
        path: &str,
        root: &Json,
        resource: &Json,
    ) {
        let _frame = self.frame(path, None);
        if self.depth.get() > 64 {
            self.issue(
                Severity::Error,
                "too-costly",
                "validation nesting is too deep",
                path,
            );
            return;
        }
        let Some(object) = value.as_object() else {
            self.issue(Severity::Error, "structure", "expected an object", path);
            return;
        };
        if let Some(element) = structure.base_element(path).cloned() {
            self.constraints(&element, value, path, root, resource);
        }
        let children: Vec<ElementDef> = structure
            .direct_children(path)
            .into_iter()
            .cloned()
            .collect();
        let mut used = Vec::new();
        for element in &children {
            let name = child_name(&element.path);
            if name == "contained" {
                used.push("contained".to_string());
                self.contained(object.get("contained"), element, root);
                continue;
            }
            if element.path.ends_with("[x]") {
                let base = name.trim_end_matches("[x]").to_string();
                let matches = choice_entries(object, &base, element);
                used.extend(matches.iter().map(|(key, _)| key.clone()));
                self.cardinality(element, matches.len(), &element.path);
                if matches.len() > 1 {
                    self.issue(
                        Severity::Error,
                        "structure",
                        format!("choice {base} has more than one value"),
                        &element.path,
                    );
                }
                for (key, type_code) in &matches {
                    if let Some(item) = object.get(key) {
                        let underscore = object.get(&format!("_{key}"));
                        self.repetitions(
                            item,
                            element,
                            structure,
                            root,
                            resource,
                            Some(type_code.clone()),
                            underscore,
                        );
                    }
                }
                continue;
            }
            used.push(name.clone());
            let underscore = object.get(&format!("_{name}"));
            match object.get(&name) {
                Some(item) => {
                    self.repetitions(item, element, structure, root, resource, None, underscore)
                }
                None if underscore.is_some() => self.repetitions(
                    &Json::Null,
                    element,
                    structure,
                    root,
                    resource,
                    None,
                    underscore,
                ),
                None => self.cardinality(element, 0, &element.path),
            }
        }
        let closed = !children.is_empty();
        for key in object.keys() {
            if !closed
                || used.iter().any(|name| name == key)
                || key.starts_with('_')
                || INFRASTRUCTURE.contains(&key.as_str())
            {
                continue;
            }
            self.issue(
                Severity::Error,
                "structure",
                format!("unknown element {key}"),
                format!("{path}.{key}"),
            );
        }
        if object.contains_key("extension") && !used.iter().any(|name| name == "extension") {
            self.extensions(object.get("extension"), path, false, root, resource);
        }
        if object.contains_key("modifierExtension")
            && !used.iter().any(|name| name == "modifierExtension")
        {
            self.extensions(object.get("modifierExtension"), path, true, root, resource);
        }
        if object.contains_key("contained") && !used.iter().any(|name| name == "contained") {
            self.contained(object.get("contained"), &bare_contained(path), root);
        }
    }

    // Part of the validator's recursive walk: the arguments are the state it carries down.
    #[allow(clippy::too_many_arguments)]
    fn repetitions(
        &self,
        value: &Json,
        element: &ElementDef,
        structure: &Structure,
        root: &Json,
        resource: &Json,
        choice_type: Option<String>,
        underscore: Option<&Json>,
    ) {
        let items = match value {
            Json::Array(items) => items.iter().collect::<Vec<_>>(),
            other => vec![other],
        };
        self.cardinality(element, items.len(), &element.path);
        if element.max == Cardinality::One && value.is_array() && items.len() > 1 {
            self.issue(
                Severity::Error,
                "structure",
                format!("{} must not repeat", element.path),
                &element.path,
            );
        }
        if !matches!(element.max, Cardinality::One | Cardinality::Zero)
            && !value.is_array()
            && !items.is_empty()
        {
            self.issue(
                Severity::Error,
                "structure",
                format!("{} must be an array", element.path),
                &element.path,
            );
        }
        let ordered = element
            .slicing
            .as_ref()
            .is_some_and(|slicing| slicing.ordered);
        let mut seen_order: Option<usize> = None;
        let name = child_name(&element.path);
        let key = match &choice_type {
            Some(code) => format!("{}{}", name.trim_end_matches("[x]"), type_suffix(code)),
            None => name,
        };
        let indexed = value.is_array() || underscore.is_some_and(|extra| extra.is_array());
        for (index, item) in items.iter().enumerate() {
            let segment = if indexed {
                format!("{key}[{index}]")
            } else {
                key.clone()
            };
            let _frame = self.frame(&element.path, Some(self.child_instance(&segment)));
            let extra = sibling_at(underscore, index);
            if item.is_null() {
                let focus = extra.cloned().unwrap_or(Json::Null);
                self.constraints(element, &focus, &element.path, root, resource);
                self.primitive_extensions(extra, &element.path, root, resource);
                continue;
            }
            if element.slicing.is_some() {
                if let Some((index, slice)) = slice::assign(item, structure, &element.path) {
                    if ordered {
                        if let Some(previous) = seen_order {
                            if index < previous {
                                self.issue(
                                    Severity::Error,
                                    "structure",
                                    format!("{} slices are out of order", element.path),
                                    &element.path,
                                );
                            }
                        }
                        seen_order = Some(index);
                    }
                    let slice = slice.clone();
                    self.one(item, &slice, structure, root, resource, choice_type.clone());
                    self.primitive_extensions(extra, &slice.path, root, resource);
                    let children: Vec<ElementDef> = structure
                        .slice_children(index)
                        .into_iter()
                        .cloned()
                        .collect();
                    if !children.is_empty() {
                        self.apply_children(item, structure, &children, root, resource);
                    }
                    continue;
                }
                if slice::rules_reject_unmatched(&element.path, structure) {
                    self.issue(
                        Severity::Error,
                        "structure",
                        format!("{} does not match a closed slice", element.path),
                        &element.path,
                    );
                    continue;
                }
            }
            self.one(
                item,
                element,
                structure,
                root,
                resource,
                choice_type.clone(),
            );
            self.primitive_extensions(extra, &element.path, root, resource);
        }
    }

    fn primitive_extensions(&self, extra: Option<&Json>, path: &str, root: &Json, resource: &Json) {
        let Some(extra) = extra else {
            return;
        };
        if let Some(extension) = extra.get("extension") {
            self.extensions(Some(extension), path, false, root, resource);
        }
        if let Some(extension) = extra.get("modifierExtension") {
            self.extensions(Some(extension), path, true, root, resource);
        }
    }

    fn apply_children(
        &self,
        item: &Json,
        structure: &Structure,
        children: &[ElementDef],
        root: &Json,
        resource: &Json,
    ) {
        let Some(object) = item.as_object() else {
            return;
        };
        for element in children {
            let name = child_name(&element.path);
            let underscore = object.get(&format!("_{name}"));
            match object.get(&name) {
                Some(value) => {
                    self.repetitions(value, element, structure, root, resource, None, underscore)
                }
                None if underscore.is_some() => self.repetitions(
                    &Json::Null,
                    element,
                    structure,
                    root,
                    resource,
                    None,
                    underscore,
                ),
                None => self.cardinality(element, 0, &element.path),
            }
        }
    }

    fn one(
        &self,
        value: &Json,
        element: &ElementDef,
        structure: &Structure,
        root: &Json,
        resource: &Json,
        choice_type: Option<String>,
    ) {
        self.constraints(element, value, &element.path, root, resource);
        if let Some(fixed) = &element.fixed {
            if fixed != value && !pattern_match(fixed, value) {
                self.issue(
                    Severity::Error,
                    "value",
                    format!("{} does not match fixed value", element.path),
                    &element.path,
                );
            }
        }
        if let Some(pattern) = &element.pattern {
            if !pattern_match(pattern, value) {
                self.issue(
                    Severity::Error,
                    "value",
                    format!("{} does not match pattern", element.path),
                    &element.path,
                );
            }
        }
        self.bounds(element, value);
        let type_codes: Vec<String> = match &choice_type {
            Some(code) => vec![code.clone()],
            None => element.types.iter().map(|item| item.code.clone()).collect(),
        };
        if type_codes.is_empty() {
            if value.is_object() {
                self.descend(value, structure, &element.path, root, resource);
            }
            return;
        }
        if type_codes.iter().all(|code| is_primitive(code)) {
            let mut failure = None;
            for code in &type_codes {
                match check_primitive(self.schema, code, value) {
                    None => {
                        failure = None;
                        break;
                    }
                    Some(message) => failure = Some(message),
                }
            }
            if let Some(message) = failure {
                self.issue(
                    Severity::Error,
                    "value",
                    format!("{} {message}", element.path),
                    &element.path,
                );
            }
            self.binding(element, value, type_codes.first().map(String::as_str));
            return;
        }
        if type_codes.iter().any(|code| code == "Reference") {
            self.reference(value, element);
        }
        self.binding(element, value, type_codes.first().map(String::as_str));
        if !value.is_object() {
            self.issue(
                Severity::Error,
                "structure",
                format!("{} expected a complex value", element.path),
                &element.path,
            );
            return;
        }
        // Element and BackboneElement children live on this snapshot, including
        // nodes reached through contentReference. A named type such as HumanName
        // has its own structure.
        let inline = type_codes.is_empty()
            || type_codes
                .iter()
                .all(|code| code == "Element" || code == "BackboneElement");
        if inline {
            self.referenced_constraints(element, structure, value, root, resource);
            self.descend(value, structure, &element.path, root, resource);
            return;
        }
        if let Some(code) = type_codes
            .iter()
            .find(|code| !is_primitive(code) && code.as_str() != "Reference")
        {
            let nested_info = self
                .schema
                .base_type(code)
                .map(|nested| (nested.url.clone(), nested.root_path().to_string()));
            if let Some((url, root_path)) = nested_info {
                if let Some(nested) = self.schema.structure(&url).cloned() {
                    self.enter();
                    self.walk_object(value, &nested, &root_path, root, resource);
                    self.leave();
                    return;
                }
            }
        }
        self.descend(value, structure, &element.path, root, resource);
    }

    fn referenced_constraints(
        &self,
        element: &ElementDef,
        structure: &Structure,
        value: &Json,
        root: &Json,
        resource: &Json,
    ) {
        let Some(target_id) = &element.content_reference else {
            return;
        };
        let Some(target) = structure.element_by_id(target_id).cloned() else {
            return;
        };
        self.constraints(&target, value, &element.path, root, resource);
    }

    fn descend(
        &self,
        value: &Json,
        structure: &Structure,
        path: &str,
        root: &Json,
        resource: &Json,
    ) {
        self.enter();
        self.walk_object(value, structure, path, root, resource);
        self.leave();
    }

    fn enter(&self) {
        self.depth.set(self.depth.get() + 1);
    }

    fn leave(&self) {
        self.depth.set(self.depth.get().saturating_sub(1));
    }

    fn cardinality(&self, element: &ElementDef, count: usize, path: &str) {
        if count == 0 && element.must_support && element.min == 0 {
            self.issue(
                Severity::Warning,
                "informational",
                format!("{path} is must-support"),
                path,
            );
        }
        if count < element.min as usize {
            self.issue(
                Severity::Error,
                "required",
                format!("{path} requires at least {}", element.min),
                path,
            );
        }
        if !element.max.allows(count) {
            self.issue(
                Severity::Error,
                "structure",
                format!("{path} has too many values"),
                path,
            );
        }
    }

    fn bounds(&self, element: &ElementDef, value: &Json) {
        if let Some(min) = &element.min_value {
            if !ordered(min, value) {
                self.issue(
                    Severity::Error,
                    "value",
                    format!("{} is below minValue", element.path),
                    &element.path,
                );
            }
        }
        if let Some(max) = &element.max_value {
            if !ordered(value, max) {
                self.issue(
                    Severity::Error,
                    "value",
                    format!("{} is above maxValue", element.path),
                    &element.path,
                );
            }
        }
    }

    fn binding(&self, element: &ElementDef, value: &Json, type_code: Option<&str>) {
        let Some(binding) = &element.binding else {
            return;
        };
        if binding.strength == "preferred" || binding.strength == "example" {
            return;
        }
        for bound in bound_codes(value, type_code.unwrap_or("")) {
            match self
                .terms
                .contains(&binding.value_set, bound.system.as_deref(), &bound.code)
            {
                TermAnswer::Yes => self.bound_concept(
                    element,
                    &binding.value_set,
                    binding.strength.as_str(),
                    &bound,
                ),
                TermAnswer::No if binding.strength == "required" => self.issue(
                    Severity::Error,
                    "code-invalid",
                    format!(
                        "{} code {} is not in {}",
                        element.path, bound.code, binding.value_set
                    ),
                    &element.path,
                ),
                TermAnswer::No => self.issue(
                    Severity::Warning,
                    "code-invalid",
                    format!(
                        "{} code {} is not in {}",
                        element.path, bound.code, binding.value_set
                    ),
                    &element.path,
                ),
                TermAnswer::Unknown => {
                    let diagnostics = match self.terms.expansion_problem(&binding.value_set) {
                        Some(reason) => format!(
                            "{} value set {} cannot be expanded: {reason}",
                            element.path, binding.value_set
                        ),
                        None => format!(
                            "{} value set {} is not loaded",
                            element.path, binding.value_set
                        ),
                    };
                    self.issue(
                        Severity::Warning,
                        "not-supported",
                        diagnostics,
                        &element.path,
                    );
                }
            }
        }
    }

    fn bound_concept(
        &self,
        element: &ElementDef,
        value_set: &str,
        strength: &str,
        bound: &BoundCode,
    ) {
        let Some(described) =
            self.terms
                .describe_bound(value_set, bound.system.as_deref(), &bound.code)
        else {
            return;
        };
        if described.inactive && strength == "required" {
            self.issue(
                Severity::Error,
                "code-invalid",
                format!("{} code {} is inactive", element.path, bound.code),
                &element.path,
            );
        }
        if let (Some(given), Some(expected)) = (&bound.display, &described.display) {
            if given != expected {
                self.issue(
                    Severity::Warning,
                    "code-invalid",
                    format!("{} display {given} does not match {expected}", element.path),
                    &element.path,
                );
            }
        }
    }

    fn reference(&self, value: &Json, element: &ElementDef) {
        let Some(reference) = value.get("reference").and_then(|item| item.as_str()) else {
            return;
        };
        if reference.contains("://") || !reference.contains('/') {
            return;
        }
        let Some((type_name, _)) = reference.split_once('/') else {
            return;
        };
        let Some(targets) = self.target_types(element) else {
            return;
        };
        if targets.is_empty()
            || targets
                .iter()
                .any(|target| target == "Resource" || target == type_name)
        {
            return;
        }
        self.issue(
            Severity::Error,
            "value",
            format!(
                "{} reference {reference} is not one of {}",
                element.path,
                targets.join(", ")
            ),
            &element.path,
        );
    }

    /// The resource types a `Reference` element may point at. A target is a profile URL, not a
    /// type name: `.../us-core-patient` constrains `Patient`. Targets are resolved to the type
    /// of the installed definition; a core definition that is not installed falls back to the
    /// name in its URL. `None` means a target that cannot be resolved, which leaves the element
    /// unrestricted rather than guessing from the profile's id.
    fn target_types(&self, element: &ElementDef) -> Option<Vec<String>> {
        let mut types = Vec::new();
        for url in element
            .types
            .iter()
            .filter(|item| item.code == "Reference")
            .flat_map(|item| item.target_profiles.iter())
        {
            if let Some(structure) = self.schema.structure(url) {
                types.push(structure.type_name.clone());
                continue;
            }
            match url.strip_prefix(CORE_STRUCTURE) {
                Some(name) if !name.is_empty() && !name.contains('/') => {
                    types.push(name.to_string());
                }
                _ => return None,
            }
        }
        Some(types)
    }

    fn contained(&self, value: Option<&Json>, element: &ElementDef, root: &Json) {
        let Some(value) = value else {
            self.cardinality(element, 0, &element.path);
            return;
        };
        let items: Vec<&Json> = match value {
            Json::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        self.cardinality(element, items.len(), &element.path);
        let indexed = value.is_array();
        for (index, item) in items.into_iter().enumerate() {
            let segment = if indexed {
                format!("contained[{index}]")
            } else {
                "contained".to_string()
            };
            let _frame = self.frame(&element.path, Some(self.child_instance(&segment)));
            self.enter();
            self.resource(item, root, &[]);
            self.leave();
        }
    }

    fn extensions(
        &self,
        value: Option<&Json>,
        path: &str,
        modifier: bool,
        root: &Json,
        resource: &Json,
    ) {
        let Some(value) = value else {
            return;
        };
        let items: Vec<&Json> = match value {
            Json::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        let field = if modifier {
            "modifierExtension"
        } else {
            "extension"
        };
        let indexed = value.is_array();
        for (index, item) in items.into_iter().enumerate() {
            let segment = if indexed {
                format!("{field}[{index}]")
            } else {
                field.to_string()
            };
            let _frame = self.frame(
                &format!("{path}.{field}"),
                Some(self.child_instance(&segment)),
            );
            let Some(url) = item.get("url").and_then(|value| value.as_str()) else {
                self.issue(
                    Severity::Error,
                    "structure",
                    format!("{path}.{field} requires url"),
                    format!("{path}.{field}"),
                );
                continue;
            };
            // An extension whose definition is not installed is still an Extension: it carries
            // the datatype's invariants, which is where `ext-1` lives.
            let structure = self
                .schema
                .structure(url)
                .or_else(|| self.schema.base_type("Extension"))
                .cloned();
            if let Some(structure) = structure {
                let root_path = structure.root_path().to_string();
                self.enter();
                self.walk_object(item, &structure, &root_path, root, resource);
                self.leave();
            }
        }
    }

    fn constraints(
        &self,
        element: &ElementDef,
        focus: &Json,
        path: &str,
        root: &Json,
        resource: &Json,
    ) {
        let node = self.locate(path.to_string());
        for constraint in &element.constraints {
            // The key alone is not the rule: a profile can redefine an inherited key with another
            // severity or wording, and that one still has to run.
            let rule = format!(
                "{}|{:?}|{}",
                constraint.key, constraint.severity, constraint.human
            );
            if !self.evaluated.borrow_mut().insert((node.clone(), rule)) {
                continue;
            }
            let Some(expr) = &constraint.parsed else {
                self.issue(
                    Severity::Warning,
                    "invariant",
                    format!("{path} constraint {} did not parse", constraint.key),
                    path,
                );
                continue;
            };
            let focus_path = if element.types.iter().any(|kind| kind.code == "Extension") {
                "Extension"
            } else {
                path
            };
            match refhir_fhirpath::evaluate_invariant(
                expr,
                focus,
                focus_path,
                resource,
                root,
                self.schema.model(),
                self.host,
            ) {
                Ok(items) if is_true(&items) => {}
                Ok(_) => self.issue(
                    constraint.severity,
                    "invariant",
                    format!("{}: {}", constraint.key, constraint.human),
                    path,
                ),
                Err(error) if error.kind == ErrorKind::Unsupported => self.issue(
                    Severity::Warning,
                    "not-supported",
                    format!("{}: {}", constraint.key, error.message),
                    path,
                ),
                Err(error) => self.issue(
                    constraint.severity,
                    "invariant",
                    format!("{}: {}", constraint.key, error.message),
                    path,
                ),
            }
        }
    }

    fn issue(
        &self,
        severity: Severity,
        code: &str,
        diagnostics: impl Into<String>,
        expression: impl Into<String>,
    ) {
        let expression = self.locate(expression.into());
        self.issues.borrow_mut().push(Issue {
            severity,
            code: code.to_string(),
            diagnostics: diagnostics.into(),
            expression,
        });
    }
}

fn profile_urls(instance: &Json) -> Vec<String> {
    instance
        .pointer("/meta/profile")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .map(|url| url.split('|').next().unwrap_or(url).to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn child_name(path: &str) -> String {
    path.rsplit('.').next().unwrap_or(path).to_string()
}

fn sibling_at(underscore: Option<&Json>, index: usize) -> Option<&Json> {
    let value = underscore?;
    match value {
        Json::Array(items) => items.get(index).filter(|item| !item.is_null()),
        Json::Object(_) if index == 0 => Some(value),
        _ => None,
    }
}

fn choice_entries(
    object: &Map<String, Json>,
    base: &str,
    element: &ElementDef,
) -> Vec<(String, String)> {
    let mut matches = Vec::new();
    for type_ref in &element.types {
        let key = format!("{base}{}", type_suffix(&type_ref.code));
        if object.contains_key(&key) {
            matches.push((key, type_ref.code.clone()));
        }
    }
    matches
}

fn bare_contained(path: &str) -> ElementDef {
    ElementDef {
        id: format!("{path}.contained"),
        path: format!("{path}.contained"),
        slice_name: None,
        min: 0,
        max: Cardinality::Many,
        types: Vec::new(),
        constraints: Vec::new(),
        binding: None,
        fixed: None,
        pattern: None,
        min_value: None,
        max_value: None,
        slicing: None,
        content_reference: None,
        must_support: false,
    }
}

struct BoundCode {
    system: Option<String>,
    code: String,
    display: Option<String>,
}

fn bound_codes(value: &Json, type_code: &str) -> Vec<BoundCode> {
    if let Some(text) = value.as_str() {
        if type_code == "code" || type_code.is_empty() || is_primitive(type_code) {
            return vec![BoundCode {
                system: None,
                code: text.to_string(),
                display: None,
            }];
        }
    }
    if value.get("coding").is_some() {
        return value
            .get("coding")
            .and_then(|item| item.as_array())
            .into_iter()
            .flatten()
            .filter_map(coding)
            .collect();
    }
    coding(value).into_iter().collect()
}

fn coding(value: &Json) -> Option<BoundCode> {
    let code = value.get("code").and_then(|item| item.as_str())?;
    Some(BoundCode {
        system: value
            .get("system")
            .and_then(|item| item.as_str())
            .map(|system| system.to_string()),
        code: code.to_string(),
        display: value
            .get("display")
            .and_then(|item| item.as_str())
            .map(|display| display.to_string()),
    })
}

fn ordered(left: &Json, right: &Json) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) => left <= right,
        _ => match (left.as_str(), right.as_str()) {
            (Some(left), Some(right)) => left <= right,
            _ => true,
        },
    }
}

fn is_true(items: &[Value]) -> bool {
    items.len() == 1 && items[0].as_singleton_bool() == Some(true)
}

struct TermHost<'a> {
    inner: &'a dyn Host,
    terms: &'a dyn Terminology,
}

impl Host for TermHost<'_> {
    fn resolve(&self, reference: &str) -> Result<Option<Json>, Error> {
        self.inner.resolve(reference)
    }

    fn member_of(
        &self,
        system: Option<&str>,
        code: &str,
        value_set: &str,
    ) -> Result<Option<bool>, Error> {
        match self.terms.contains(value_set, system, code) {
            TermAnswer::Yes => Ok(Some(true)),
            TermAnswer::No => Ok(Some(false)),
            TermAnswer::Unknown => self.inner.member_of(system, code, value_set),
        }
    }
}

#[cfg(test)]
mod ops_setup_tests {
    use crate::{validate_resource, MemoryTerminology, Schema};
    use refhir_fhirpath::Host;
    use serde_json::json;
    struct NoHost;
    impl Host for NoHost {}

    #[test]
    fn generic_subject_and_string_extension_are_valid() {
        let ext = json!({"key":"ext-1","severity":"error","human":"value or nested extension","expression":"extension.exists() != value.exists()"});
        let schema = Schema::from_definitions(&[
            json!({"resourceType":"StructureDefinition","url":"http://hl7.org/fhir/StructureDefinition/Basic","name":"Basic","type":"Basic","kind":"resource","derivation":"specialization","snapshot":{"element":[
                {"path":"Basic","min":0,"max":"*"},
                {"path":"Basic.subject","min":0,"max":"1","type":[{"code":"Reference","targetProfile":["http://hl7.org/fhir/StructureDefinition/Resource"]}]},
                {"path":"Basic.extension","min":0,"max":"*","type":[{"code":"Extension"}],"constraint":[ext.clone()]}
            ]}}),
            json!({"resourceType":"StructureDefinition","url":"http://hl7.org/fhir/StructureDefinition/Extension","name":"Extension","type":"Extension","kind":"complex-type","derivation":"specialization","snapshot":{"element":[
                {"path":"Extension","min":0,"max":"*","constraint":[ext]},
                {"path":"Extension.url","min":1,"max":"1","type":[{"code":"uri"}]},
                {"path":"Extension.extension","min":0,"max":"*","type":[{"code":"Extension"}]},
                {"path":"Extension.value[x]","min":0,"max":"1","type":[{"code":"string"}]}
            ]}}),
        ]);
        let instance = json!({"resourceType":"Basic","subject":{"reference":"Organization/org"},"extension":[{"url":"https://example.org/fhir/StructureDefinition/onboarding-json","valueString":"{}"}]});
        let outcome = validate_resource(&instance, &schema, &MemoryTerminology::default(), &NoHost);
        assert!(!outcome.has_errors(), "{:?}", outcome.issues);
        let invalid = json!({"resourceType":"Basic","extension":[{"url":"https://example.invalid","valueString":"{}","extension":[{"url":"https://example.invalid/child","valueString":"child"}]}]});
        assert!(
            validate_resource(&invalid, &schema, &MemoryTerminology::default(), &NoHost)
                .has_errors()
        );
    }
}
