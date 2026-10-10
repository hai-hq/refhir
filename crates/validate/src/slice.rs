use serde_json::Value as Json;

use crate::primitive::type_suffix;
use crate::schema::{DiscriminatorKind, ElementDef, SliceRules, Structure};

pub fn assign<'a>(
    item: &Json,
    structure: &'a Structure,
    path: &str,
) -> Option<(usize, &'a ElementDef)> {
    let slicing = structure.base_element(path)?.slicing.as_ref()?;
    for (index, slice) in structure.slices(path) {
        if matches_slice(item, structure, index, slicing) {
            return Some((index, slice));
        }
    }
    None
}

pub fn rules_reject_unmatched(path: &str, structure: &Structure) -> bool {
    matches!(
        structure
            .base_element(path)
            .and_then(|element| element.slicing.as_ref())
            .map(|slicing| slicing.rules),
        Some(SliceRules::Closed)
    )
}

fn matches_slice(
    item: &Json,
    structure: &Structure,
    slice_index: usize,
    slicing: &crate::schema::Slicing,
) -> bool {
    if slicing.discriminators.is_empty() {
        return false;
    }
    slicing.discriminators.iter().all(|discriminator| {
        let value = at_path(item, &discriminator.path);
        let child = slice_child(structure, slice_index, &discriminator.path);
        match discriminator.kind {
            DiscriminatorKind::Exists => {
                let absent = child
                    .map(|element| matches!(element.max, crate::schema::Cardinality::Zero))
                    .unwrap_or(false);
                if absent {
                    value.is_none()
                } else {
                    value.is_some()
                }
            }
            DiscriminatorKind::Value | DiscriminatorKind::Pattern => {
                let Some(value) = value else {
                    return false;
                };
                let source = child.unwrap_or(&structure.elements[slice_index]);
                if let Some(fixed) = &source.fixed {
                    return fixed == value || pattern_match(fixed, value);
                }
                if let Some(pattern) = &source.pattern {
                    return pattern_match(pattern, value);
                }
                false
            }
            DiscriminatorKind::Type => {
                let Some(value) = value else {
                    return false;
                };
                let types = child
                    .map(|element| &element.types)
                    .unwrap_or(&structure.elements[slice_index].types);
                let actual = value
                    .get("resourceType")
                    .and_then(|item| item.as_str())
                    .unwrap_or("");
                types
                    .iter()
                    .any(|item| item.code == actual || type_suffix(&item.code) == actual)
            }
            DiscriminatorKind::Profile => {
                let Some(value) = value else {
                    return false;
                };
                let profiles = value
                    .pointer("/meta/profile")
                    .and_then(|item| item.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let types = child
                    .map(|element| &element.types)
                    .unwrap_or(&structure.elements[slice_index].types);
                types.iter().any(|item| {
                    item.profiles.iter().any(|profile| {
                        profiles.iter().any(|stated| {
                            stated.trim_end_matches(|ch| ch == '|') == profile
                                || stated.starts_with(profile)
                        })
                    }) || item.target_profiles.iter().any(|profile| {
                        profiles
                            .iter()
                            .any(|stated| stated.starts_with(profile.as_str()))
                    })
                })
            }
        }
    })
}

fn slice_child<'a>(
    structure: &'a Structure,
    slice_index: usize,
    path: &str,
) -> Option<&'a ElementDef> {
    if path.is_empty() || path == "$this" {
        return Some(&structure.elements[slice_index]);
    }
    let slice_path = &structure.elements[slice_index].path;
    let full = format!("{slice_path}.{path}");
    structure
        .slice_children(slice_index)
        .into_iter()
        .find(|element| element.path == full)
        .or_else(|| {
            structure
                .elements
                .iter()
                .skip(slice_index + 1)
                .find(|element| element.path == full)
        })
}

pub fn at_path<'a>(value: &'a Json, path: &str) -> Option<&'a Json> {
    if path.is_empty() || path == "$this" {
        return Some(value);
    }
    let mut current = value;
    for part in path.split('.') {
        if part.is_empty() || part == "$this" {
            continue;
        }
        current = current.get(part).or_else(|| choice_value(current, part))?;
    }
    Some(current)
}

fn choice_value<'a>(value: &'a Json, name: &str) -> Option<&'a Json> {
    let object = value.as_object()?;
    object.iter().find_map(|(key, child)| {
        key.strip_prefix(name)
            .filter(|rest| {
                !rest.is_empty()
                    && rest
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_ascii_uppercase())
            })
            .map(|_| child)
    })
}

pub fn pattern_match(pattern: &Json, value: &Json) -> bool {
    match pattern {
        Json::Object(map) => {
            let Some(object) = value.as_object() else {
                return false;
            };
            map.iter().all(|(key, expected)| {
                object
                    .get(key)
                    .is_some_and(|actual| pattern_match(expected, actual))
            })
        }
        Json::Array(items) => {
            let Some(array) = value.as_array() else {
                return items.len() == 1 && pattern_match(&items[0], value);
            };
            items
                .iter()
                .all(|expected| array.iter().any(|actual| pattern_match(expected, actual)))
        }
        other => other == value,
    }
}
