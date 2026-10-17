//! What the test suites of several crates share, kept in one place: rewriting a connection string,
//! and an ordinary role that row-level security binds. Hidden from the docs; nothing else uses it.

use postgres::{Client, NoTls};

/// The role the row-level security tests connect as: not a superuser, no BYPASSRLS, and the owner
/// of the databases they make, as the application's own role is of its database.
pub const ORDINARY_ROLE: &str = "refhir_rls_app";
const ORDINARY_PASSWORD: &str = "refhir-rls-app";

/// `url` with its database replaced by `name`. Handles `postgres://...` URLs and the
/// `host=... user=...` key/value form CI uses.
pub fn with_database(url: &str, name: &str) -> String {
    if url.contains("://") {
        let (head, tail) = match url.split_once('?') {
            Some((head, query)) => (head, format!("?{query}")),
            None => (url, String::new()),
        };
        let slash = head.rfind('/').expect("database name in the url");
        format!("{}/{name}{tail}", &head[..slash])
    } else {
        let kept: Vec<&str> = url
            .split_whitespace()
            .filter(|part| !part.starts_with("dbname="))
            .collect();
        format!("{} dbname={name}", kept.join(" "))
    }
}

/// The name of the database `url` connects to.
pub fn database_name(url: &str) -> String {
    if let Some((_, rest)) = url.split_once("://") {
        let path = rest.split_once('/').map_or("", |(_, path)| path);
        path.split('?').next().unwrap_or("").to_string()
    } else {
        url.split_whitespace()
            .find_map(|part| part.strip_prefix("dbname="))
            .expect("dbname in the url")
            .to_string()
    }
}

/// `url` with another user and password. Handles `postgres://...` URLs and the key/value form.
pub fn with_role(url: &str, user: &str, password: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        format!("{scheme}://{user}:{password}@{host}/{tail}")
    } else {
        let kept: Vec<&str> = url
            .split_whitespace()
            .filter(|part| !part.starts_with("user=") && !part.starts_with("password="))
            .collect();
        format!("{} user={user} password={password}", kept.join(" "))
    }
}

/// `url` as the ordinary role, which row-level security binds.
pub fn as_ordinary_role(url: &str) -> String {
    with_role(url, ORDINARY_ROLE, ORDINARY_PASSWORD)
}

/// Makes the ordinary role in the cluster `admin` connects to, if it is not there. Several test
/// binaries may do it at once, so a clash is not an error.
///
/// The role has a password everyone who reads this file knows, and is left in the cluster, so it
/// expires: it can log in for six hours after the last test run that asked for it.
pub fn ensure_ordinary_role(admin: &str) {
    Client::connect(admin, NoTls)
        .expect("admin connection")
        .batch_execute(&format!(
            "DO $r$ BEGIN \
               CREATE ROLE {ORDINARY_ROLE} LOGIN PASSWORD '{ORDINARY_PASSWORD}' \
                 NOSUPERUSER NOBYPASSRLS; \
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $r$; \
             DO $r$ BEGIN \
               EXECUTE format('ALTER ROLE {ORDINARY_ROLE} VALID UNTIL %L', \
                              (now() + interval '6 hours')::text); \
             EXCEPTION WHEN OTHERS THEN NULL; END $r$"
        ))
        .expect("create the ordinary role (the test role needs CREATEROLE)");
}
