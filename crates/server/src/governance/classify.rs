//! What kind of request this is, which decides what it costs and how it is audited.
//!
//! One request is not one request. A read of a resource by id and a bulk `$export` both arrive as
//! a single HTTP call, so a limit that counts calls treats a client that exports as it would one
//! that polls `/metadata`. Each request is put in a class, and each class has its own cost.

use super::audit::interaction_of;

/// A kind of request. The name is what usage rows and the request log record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// A resource, a version, or a job's status or file.
    Read,
    /// A search, or any history, or a compartment listing.
    Search,
    /// A create, update, patch or delete.
    Write,
    /// A transaction or batch `Bundle`.
    Bundle,
    /// A FHIR operation (`$validate`, `$expand`, `$populate`, `$everything`, …) or GraphQL.
    Operation,
    /// A bulk `$export`.
    Export,
    /// A `/sync` websocket, opened or used.
    Sync,
    /// The OAuth endpoints.
    Auth,
    /// `/metadata`, `/.well-known/*` and the verbose `/healthz` (the plain probe is not governed at all).
    Discovery,
    /// The server's own `/_audit`, `/_usage` and `/_requests`.
    Admin,
    /// A live query re-run and sent because a write overlapped it. The server's doing, not a
    /// request, so it is counted and never refused.
    Push,
}

impl Class {
    pub const ALL: [Class; 11] = [
        Class::Read,
        Class::Search,
        Class::Write,
        Class::Bundle,
        Class::Operation,
        Class::Export,
        Class::Sync,
        Class::Auth,
        Class::Discovery,
        Class::Admin,
        Class::Push,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Class::Read => "read",
            Class::Search => "search",
            Class::Write => "write",
            Class::Bundle => "bundle",
            Class::Operation => "operation",
            Class::Export => "export",
            Class::Sync => "sync",
            Class::Auth => "auth",
            Class::Discovery => "discovery",
            Class::Admin => "admin",
            Class::Push => "push",
        }
    }

    pub fn parse(name: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|class| class.name() == name)
    }

    /// Whether this changes data, for the write quota.
    pub fn is_write(self) -> bool {
        matches!(self, Class::Write | Class::Bundle)
    }

    /// Whether this counts against a caller's quota. Discovery and the OAuth endpoints are what
    /// a client needs to get to its quota at all, and the admin endpoints are for whoever runs
    /// the server.
    pub fn counts_toward_quota(self) -> bool {
        !matches!(
            self,
            Class::Discovery | Class::Auth | Class::Admin | Class::Push
        )
    }
}

/// The class of an HTTP request.
pub fn classify(method: &str, path: &str) -> Class {
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let reading = matches!(method, "GET" | "HEAD");
    let Some(&first) = segments.first() else {
        return if method == "POST" {
            Class::Bundle
        } else {
            Class::Read
        };
    };
    match first {
        "metadata" | ".well-known" | "healthz" => return Class::Discovery,
        "oauth" => return Class::Auth,
        "graphql" => return Class::Operation,
        "sync" => return Class::Sync,
        "_audit" | "_usage" | "_requests" => return Class::Admin,
        "_history" => return Class::Search,
        "$export" => return Class::Export,
        // A job's status and its file.
        "export" => return Class::Read,
        _ if first.starts_with('$') => return Class::Operation,
        _ => {}
    }
    match segments.len() {
        1 => {
            if reading {
                Class::Search
            } else {
                Class::Write
            }
        }
        2 => match segments[1] {
            "_history" => Class::Search,
            "$export" => Class::Export,
            second if second.starts_with('$') => Class::Operation,
            _ if reading => Class::Read,
            _ => Class::Write,
        },
        _ => match segments[2] {
            "_history" if segments.len() == 3 => Class::Search,
            "_history" => Class::Read,
            "$websocket" => Class::Sync,
            "$export" => Class::Export,
            "$meta" => {
                if reading {
                    Class::Read
                } else {
                    Class::Write
                }
            }
            third if third.starts_with('$') => Class::Operation,
            // `/Patient/1/Observation`: a compartment listing.
            _ => Class::Search,
        },
    }
}

/// The class of an engine operation named as `/sync` names it: `Patient/update`,
/// `Observation/search`, `Bundle/transaction`.
pub fn classify_operation(path: &str) -> Class {
    let op = path.split_once('/').map(|(_, op)| op).unwrap_or(path);
    match op {
        "transaction" | "batch" => Class::Bundle,
        "read" | "vread" | "$meta" => Class::Read,
        "search" | "history" => Class::Search,
        "create" | "update" | "patch" | "delete" | "conditional-create" | "conditional-update"
        | "conditional-delete" | "$meta-add" | "$meta-delete" => Class::Write,
        _ => Class::Operation,
    }
}

/// How a request that never reached the engine is described in the audit trail: the FHIR
/// `AuditEvent.action` and a subtype. These are requests the server refused (no token, scope
/// too narrow) and the ones that are not an interaction with a resource at all.
///
/// An interaction with a resource is described by [`interaction_of`], the table the engine's
/// own audit rows use, so that the same interaction is recorded the same way whether it was
/// refused at the door or failed inside. Everything else is named from a fixed set: nothing a
/// caller put in the path becomes part of a subtype.
pub fn describe(method: &str, path: &str, class: Class) -> (&'static str, String) {
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let kind = segments.first().copied();
    match class {
        Class::Auth => (
            "E",
            match segments.last().copied() {
                Some(
                    endpoint @ ("token" | "authorize" | "introspect" | "revoke" | "register"
                    | "manage" | "ehr-launch" | "interact" | "callback"),
                ) => format!("oauth-{endpoint}"),
                _ => "oauth".to_string(),
            },
        ),
        Class::Sync => ("E", "sync-connect".to_string()),
        Class::Export => ("E", "export".to_string()),
        Class::Admin => (
            "R",
            match kind {
                Some("_audit") if segments.get(1) == Some(&"verify") => "audit-verify",
                Some("_audit") => "audit-read",
                Some("_usage") => "usage-read",
                Some("_requests") => "requests-read",
                _ => "admin-read",
            }
            .to_string(),
        ),
        Class::Push => ("R", "sync-push".to_string()),
        Class::Discovery => ("R", "capabilities".to_string()),
        Class::Operation => interaction_of("$operation", kind, false),
        Class::Bundle => interaction_of("transaction", None, false),
        Class::Search => match segments.as_slice() {
            [] | ["_history"] => interaction_of("history", None, false),
            [kind, "_history"] => interaction_of("history", Some(kind), false),
            [kind, _, "_history"] => interaction_of("history", Some(kind), true),
            [kind, ..] => interaction_of("search", Some(kind), false),
        },
        Class::Read => match segments.as_slice() {
            ["export", ..] => ("R", "export-read".to_string()),
            [kind, id, "_history", _] => interaction_of("vread", Some(kind), !id.is_empty()),
            _ => interaction_of("read", kind, true),
        },
        Class::Write => interaction_of(
            match method {
                "POST" => "create",
                "PATCH" => "patch",
                "DELETE" => "delete",
                _ => "update",
            },
            kind,
            segments.len() > 1,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_lands_in_the_class_that_prices_it() {
        let cases = [
            ("GET", "/metadata", Class::Discovery),
            ("GET", "/healthz", Class::Discovery),
            ("GET", "/.well-known/smart-configuration", Class::Discovery),
            ("POST", "/oauth/token", Class::Auth),
            ("GET", "/oauth/authorize", Class::Auth),
            ("GET", "/sync", Class::Sync),
            ("GET", "/Patient/p1/$websocket", Class::Sync),
            ("POST", "/graphql", Class::Operation),
            ("POST", "/", Class::Bundle),
            ("GET", "/Patient", Class::Search),
            ("POST", "/Patient", Class::Write),
            ("PUT", "/Patient", Class::Write),
            ("DELETE", "/Patient", Class::Write),
            ("GET", "/Patient/p1", Class::Read),
            ("PUT", "/Patient/p1", Class::Write),
            ("PATCH", "/Patient/p1", Class::Write),
            ("DELETE", "/Patient/p1", Class::Write),
            ("GET", "/_history", Class::Search),
            ("GET", "/Patient/_history", Class::Search),
            ("GET", "/Patient/p1/_history", Class::Search),
            ("GET", "/Patient/p1/_history/7", Class::Read),
            ("GET", "/Patient/p1/Observation", Class::Search),
            ("POST", "/Patient/$validate", Class::Operation),
            ("POST", "/ValueSet/$expand", Class::Operation),
            ("POST", "/ImplementationGuide/$install", Class::Operation),
            ("POST", "/Patient/p1/$validate", Class::Operation),
            ("GET", "/Patient/p1/$everything", Class::Operation),
            ("GET", "/Patient/p1/$meta", Class::Read),
            ("POST", "/Patient/p1/$meta", Class::Write),
            ("POST", "/$export", Class::Export),
            ("POST", "/Patient/$export", Class::Export),
            ("POST", "/Patient/p1/$export", Class::Export),
            ("GET", "/export/job-1", Class::Read),
            ("GET", "/export/job-1/file", Class::Read),
            ("GET", "/_audit", Class::Admin),
            ("GET", "/_usage", Class::Admin),
            ("GET", "/_requests", Class::Admin),
        ];
        for (method, path, class) in cases {
            assert_eq!(classify(method, path), class, "{method} {path}");
        }
    }

    #[test]
    fn sync_operations_are_priced_like_the_requests_they_stand_for() {
        for (path, class) in [
            ("Patient/read", Class::Read),
            ("Patient/vread", Class::Read),
            ("Observation/search", Class::Search),
            ("Patient/history", Class::Search),
            ("system/history", Class::Search),
            ("Patient/create", Class::Write),
            ("Patient/update", Class::Write),
            ("Patient/conditional-update", Class::Write),
            ("Patient/delete", Class::Write),
            ("Bundle/transaction", Class::Bundle),
            ("Bundle/batch", Class::Bundle),
            ("ValueSet/$expand", Class::Operation),
            ("Patient/$everything", Class::Operation),
        ] {
            assert_eq!(classify_operation(path), class, "{path}");
        }
    }

    #[test]
    fn classes_round_trip_by_name_and_know_what_they_cost_against() {
        for class in Class::ALL {
            assert_eq!(Class::parse(class.name()), Some(class));
        }
        assert_eq!(Class::parse("nonsense"), None);
        assert!(Class::Write.is_write() && Class::Bundle.is_write());
        assert!(!Class::Read.is_write() && !Class::Operation.is_write());
        assert!(Class::Read.counts_toward_quota() && Class::Export.counts_toward_quota());
        assert!(!Class::Discovery.counts_toward_quota());
        assert!(!Class::Auth.counts_toward_quota());
        assert!(!Class::Admin.counts_toward_quota());
        assert!(!Class::Push.counts_toward_quota());
    }

    #[test]
    fn refused_requests_are_described_as_the_interaction_they_attempted() {
        let describe = |method, path| {
            let (action, subtype) = describe(method, path, classify(method, path));
            (action, subtype)
        };
        assert_eq!(describe("GET", "/Patient/p1"), ("R", "read".into()));
        assert_eq!(
            describe("GET", "/Patient/p1/_history/3"),
            ("R", "vread".into())
        );
        assert_eq!(describe("GET", "/Patient"), ("E", "search-type".into()));
        assert_eq!(
            describe("GET", "/Patient/_history"),
            ("R", "history-type".into())
        );
        assert_eq!(
            describe("GET", "/Patient/p1/_history"),
            ("R", "history-instance".into())
        );
        assert_eq!(describe("POST", "/Patient"), ("C", "create".into()));
        assert_eq!(describe("PUT", "/Patient/p1"), ("U", "update".into()));
        assert_eq!(describe("DELETE", "/Patient/p1"), ("D", "delete".into()));
        assert_eq!(describe("POST", "/"), ("E", "transaction".into()));
        assert_eq!(
            describe("POST", "/oauth/token"),
            ("E", "oauth-token".into())
        );
        assert_eq!(describe("GET", "/_audit"), ("R", "audit-read".into()));
        assert_eq!(describe("GET", "/_usage"), ("R", "usage-read".into()));
        assert_eq!(describe("POST", "/Patient/$export"), ("E", "export".into()));
        assert_eq!(
            describe("GET", "/export/j/file"),
            ("R", "export-read".into())
        );
    }
}
