//! SMART v1 and v2 resource scopes.

pub(crate) fn http_interaction(method: &str, path: &str) -> &'static str {
    if let Some(op) = path.split('/').find(|segment| segment.starts_with('$')) {
        if op == "$install" {
            return "install";
        }
        if matches!(
            op,
            "$populate" | "$assemble" | "$extract" | "$next-question" | "$behavior"
        ) {
            return "read";
        }
    }
    match method {
        "GET" | "HEAD" => {
            if is_search_path(path) {
                "search"
            } else {
                "read"
            }
        }
        "POST" => "create",
        "PUT" | "PATCH" => "update",
        "DELETE" => "delete",
        _ => "update",
    }
}

fn is_search_path(path: &str) -> bool {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.starts_with('_') || trimmed.starts_with('$') {
        return true;
    }
    let mut parts = trimmed.split('/');
    let resource = parts.next().unwrap_or("");
    if resource.is_empty() || matches!(resource, "graphql" | "sync" | "export") {
        return true;
    }
    match (parts.next(), parts.next()) {
        (None, _) => true,
        (Some("_history"), None) => true,
        (Some(id), None) if !id.is_empty() && !id.starts_with('$') && !id.starts_with('_') => false,
        (Some(id), Some("_history"))
            if !id.is_empty() && !id.starts_with('$') && !id.starts_with('_') =>
        {
            false
        }
        _ => false,
    }
}

pub(crate) fn interaction_for(op: &str, subscribe: bool) -> Result<&'static str, String> {
    const READ_OPS: &[&str] = &[
        "read",
        "vread",
        "history",
        "$validate",
        "$meta",
        "$everything",
        "$document",
        "$expand",
        "$lookup",
        "$validate-code",
        "$translate",
        "$subsumes",
        "$populate",
        "$assemble",
        "$next-question",
        "$behavior",
        "$extract",
    ];
    const SEARCH_OPS: &[&str] = &["search"];
    const CREATE_OPS: &[&str] = &["create", "conditional-create"];
    const INSTALL_OPS: &[&str] = &["$install"];
    const UPDATE_OPS: &[&str] = &[
        "update",
        "patch",
        "conditional-update",
        "$meta-add",
        "$meta-delete",
    ];
    const DELETE_OPS: &[&str] = &["delete", "conditional-delete"];
    let interaction = if SEARCH_OPS.contains(&op) {
        "search"
    } else if READ_OPS.contains(&op) {
        "read"
    } else if CREATE_OPS.contains(&op) {
        "create"
    } else if UPDATE_OPS.contains(&op) {
        "update"
    } else if DELETE_OPS.contains(&op) {
        "delete"
    } else if INSTALL_OPS.contains(&op) {
        "install"
    } else {
        return Err(format!("unsupported operation {op}"));
    };
    if subscribe && !matches!(interaction, "read" | "search") {
        return Err(format!("{op} writes and cannot be subscribed"));
    }
    Ok(interaction)
}

pub(crate) fn scope_allows(scope: &str, resource_type: &str, interaction: &str) -> bool {
    if interaction == "install" {
        return install_scope_allows(scope, resource_type);
    }
    let scope = scope.split_once('?').map(|(head, _)| head).unwrap_or(scope);
    let mut parts = scope.split('/');
    let context = parts.next().unwrap_or("");
    if !matches!(context, "patient" | "user" | "system") {
        return false;
    }
    let Some(rest) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let (type_name, permission) = rest.split_once('.').unwrap_or((rest, "*"));
    if type_name != "*" && type_name != resource_type {
        return false;
    }
    letters_allow(&v2_letters(permission), interaction)
}

fn install_scope_allows(scope: &str, resource_type: &str) -> bool {
    if resource_type != "ImplementationGuide" {
        return false;
    }
    let scope = scope.split_once('?').map(|(head, _)| head).unwrap_or(scope);
    let mut parts = scope.split('/');
    let context = parts.next().unwrap_or("");
    if context != "system" {
        return false;
    }
    let Some(rest) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let (type_name, permission) = rest.split_once('.').unwrap_or((rest, "*"));
    if type_name != "*" && type_name != "ImplementationGuide" {
        return false;
    }
    letters_allow(&v2_letters(permission), "create")
}

fn v2_letters(permission: &str) -> String {
    match permission {
        "read" => "rs".into(),
        "write" => "cud".into(),
        "*" => "cruds".into(),
        letters if is_cruds(letters) => letters.to_string(),
        _ => String::new(),
    }
}

fn is_cruds(letters: &str) -> bool {
    if letters.is_empty() {
        return false;
    }
    let mut last = 0usize;
    for ch in letters.chars() {
        let Some(index) = "cruds".find(ch) else {
            return false;
        };
        if index < last {
            return false;
        }
        last = index + 1;
    }
    true
}

fn letters_allow(letters: &str, interaction: &str) -> bool {
    let needed = match interaction {
        "create" => "c",
        "read" | "vread" => "r",
        "update" | "write" | "patch" => "u",
        "delete" => "d",
        "search" => "s",
        "*" => "cruds",
        _ => return false,
    };
    if interaction == "write" {
        return letters.contains('c') || letters.contains('u') || letters.contains('d');
    }
    needed.chars().all(|letter| letters.contains(letter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_read_covers_search() {
        assert!(scope_allows("user/Patient.read", "Patient", "search"));
        assert!(scope_allows("user/Patient.read", "Patient", "read"));
        assert!(!scope_allows("user/Patient.read", "Patient", "create"));
        assert!(!scope_allows("user/Patient.read", "Observation", "read"));
        assert!(!scope_allows("user/Patient.read", "*", "read"));
        assert!(scope_allows("user/*.read", "*", "search"));
    }

    #[test]
    fn v2_letters_distinguish_search_and_create() {
        assert!(scope_allows(
            "patient/Observation.rs",
            "Observation",
            "read"
        ));
        assert!(scope_allows(
            "patient/Observation.rs",
            "Observation",
            "search"
        ));
        assert!(!scope_allows(
            "patient/Observation.rs",
            "Observation",
            "create"
        ));
        assert!(scope_allows("user/*.cruds", "Patient", "delete"));
        assert!(scope_allows(
            "patient/Observation.rs?category=vital-signs",
            "Observation",
            "read"
        ));
        assert!(!scope_allows("launch/patient", "Patient", "read"));
        assert!(!scope_allows("openid", "Patient", "read"));
    }

    #[test]
    fn install_requires_system_write_on_implementation_guide() {
        assert!(scope_allows(
            "system/*.write",
            "ImplementationGuide",
            "install"
        ));
        assert!(scope_allows(
            "system/ImplementationGuide.write",
            "ImplementationGuide",
            "install"
        ));
        assert!(scope_allows(
            "system/*.cruds",
            "ImplementationGuide",
            "install"
        ));
        assert!(!scope_allows(
            "user/*.cruds",
            "ImplementationGuide",
            "install"
        ));
        assert!(!scope_allows(
            "patient/*.write",
            "ImplementationGuide",
            "install"
        ));
        assert!(!scope_allows("system/*.write", "Patient", "install"));
        assert!(!scope_allows(
            "system/Patient.write",
            "ImplementationGuide",
            "install"
        ));
        assert!(!scope_allows(
            "system/*.read",
            "ImplementationGuide",
            "install"
        ));
        assert_eq!(
            http_interaction("POST", "/ImplementationGuide/$install"),
            "install"
        );
    }
}
