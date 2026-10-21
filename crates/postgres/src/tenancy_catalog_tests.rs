//! The catalog tenants share, against a real Postgres (`REFHIR_TEST_POSTGRES_URL`).
//!
//! A package that tenants have in common is stored once. The first tenant to ask for it builds the
//! copy; each other tenant is given it without the package being read again, and gets its own
//! search parameters and expansions, which are the parts that change per tenant.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use refhir_persistence::{
    ConformanceStore, ExpansionChange, InstallError, InstallOutcome, NewExpansion, NewPackage,
    NewSearchParameter, NewStructureDefinition, NewTerminology, ParamStatus, PersistError,
};

use crate::tenancy_tests::{client, digests, open, shared};
use crate::tests::test_url;
use crate::{delete_tenant, PostgresPersistence};

pub(crate) fn demo(name: &str, fhir_version: &str) -> NewPackage {
    NewPackage {
        name: name.to_string(),
        version: "1.0.0".into(),
        fhir_version: fhir_version.to_string(),
        structure_definitions: (0..3)
            .map(|n| NewStructureDefinition {
                url: format!("http://example.org/{name}/sd/{n}"),
                type_name: format!("Type{n}"),
                kind: "resource".into(),
                is_abstract: false,
                json: format!(r#"{{"resourceType":"StructureDefinition","name":"{name}{n}"}}"#),
            })
            .collect(),
        search_parameters: (0..3)
            .map(|n| NewSearchParameter {
                url: format!("http://example.org/{name}/sp/{n}"),
                base: "Patient".into(),
                code: format!("{name}-code{n}"),
                param_type: "string".into(),
                expression: Some(format!("Patient.extension{n}")),
                status: ParamStatus::Pending,
                json: format!(r#"{{"resourceType":"SearchParameter","code":"{name}-code{n}"}}"#),
            })
            .collect(),
        terminology: vec![
            NewTerminology {
                resource_type: "CodeSystem".into(),
                url: format!("http://example.org/{name}/cs"),
                json: r#"{"resourceType":"CodeSystem"}"#.into(),
            },
            NewTerminology {
                resource_type: "ValueSet".into(),
                url: format!("http://example.org/{name}/vs"),
                json: r#"{"resourceType":"ValueSet"}"#.into(),
            },
        ],
        expansions: (0..2)
            .map(|n| NewExpansion {
                url: format!("http://example.org/{name}/vs{n}"),
                json: format!(r#"{{"expansion":{n}}}"#),
            })
            .collect(),
    }
}

/// Installs `package` for the tenant as a package that `content` identifies, and says how many
/// times the store asked for the package to be read.
pub(crate) fn install(
    store: &PostgresPersistence,
    package: &NewPackage,
    content: &str,
) -> Result<(InstallOutcome, usize), InstallError> {
    let mut reads = 0;
    let outcome = store.install_shared_package_in_pieces(
        &package.name,
        &package.version,
        &package.fhir_version,
        content,
        &mut |rows| {
            reads += 1;
            rows.structure_definitions(&package.structure_definitions)?;
            rows.search_parameters(&package.search_parameters)?;
            rows.terminology(&package.terminology)?;
            rows.expansions(&package.expansions)?;
            Ok(())
        },
    )?;
    Ok((outcome, reads))
}

fn count(url: &str, sql: &str) -> i64 {
    client(url).query_one(sql, &[]).expect("count").get(0)
}

fn definitions(store: &PostgresPersistence) -> Vec<(i64, String, String)> {
    store
        .load_structure_definitions()
        .unwrap()
        .into_iter()
        .map(|row| (row.package_id, row.url, row.json))
        .collect()
}

fn paged_definitions(store: &PostgresPersistence) -> Vec<(i64, String, String)> {
    let mut seen = Vec::new();
    store
        .visit_structure_definitions(&mut |page| {
            seen.extend(
                page.into_iter()
                    .map(|row| (row.package_id, row.url, row.json)),
            );
            Ok(())
        })
        .unwrap();
    seen
}

fn terminology(store: &PostgresPersistence) -> Vec<(i64, String)> {
    store
        .load_terminology()
        .unwrap()
        .into_iter()
        .map(|row| (row.package_id, row.url))
        .collect()
}

/// A newer version of a package that tenants share replaces the search parameters of the tenant
/// that installs it, and of no other: another tenant keeps the version it has until it installs the
/// newer one, which it is given from the copy that was built.
#[test]
fn a_newer_version_of_a_shared_package_replaces_the_older_one_for_the_tenant_that_installs_it() {
    let Some(admin) = test_url("shared package versions") else {
        return;
    };
    let url = shared(&admin, "tver", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let rows = |store: &PostgresPersistence| -> Vec<(String, String, ParamStatus, i64)> {
        let mut rows: Vec<_> = store
            .load_search_parameters()
            .unwrap()
            .into_iter()
            .filter(|row| row.code.starts_with("versioned-"))
            .map(|row| {
                (
                    row.code,
                    row.expression.unwrap_or_default(),
                    row.status,
                    row.package_id,
                )
            })
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    };
    let codes = |list: &[refhir_persistence::PendingParam]| -> Vec<String> {
        list.iter().map(|param| param.code.clone()).collect()
    };
    // The versions of the package a tenant has, and the packages its definitions and expansions of
    // it are stored under.
    let versions = |store: &PostgresPersistence| -> Vec<String> {
        store
            .load_packages()
            .unwrap()
            .into_iter()
            .filter(|package| package.name == "versioned")
            .map(|package| package.version)
            .collect()
    };
    let owners = |store: &PostgresPersistence| -> (Vec<i64>, Vec<i64>) {
        let mut definitions: Vec<i64> = store
            .load_structure_definitions()
            .unwrap()
            .into_iter()
            .filter(|row| row.url.contains("/versioned/"))
            .map(|row| row.package_id)
            .collect();
        let mut expansions: Vec<i64> = store
            .load_expansions()
            .unwrap()
            .into_iter()
            .filter(|row| row.url.contains("/versioned/"))
            .map(|row| row.package_id)
            .collect();
        definitions.sort();
        definitions.dedup();
        expansions.sort();
        expansions.dedup();
        (definitions, expansions)
    };
    let first = demo("versioned", "4.0.1");
    let (one_a, _) = install(&a, &first, "one").unwrap();
    a.mark_base_ready(one_a.package_id, "Patient").unwrap();
    let (one_b, _) = install(&b, &first, "one").unwrap();
    assert_eq!(rows(&a).len(), 3);
    assert_eq!(rows(&b).len(), 3);

    // 2.0.0 keeps one parameter, changes another, and leaves the third out.
    let mut second = demo("versioned", "4.0.1");
    second.version = "2.0.0".into();
    second.search_parameters.truncate(2);
    second.search_parameters[1].expression = Some("Patient.changed".into());
    let (two_a, reads) = install(&a, &second, "two").unwrap();
    assert_eq!(reads, 1);
    assert_eq!(codes(&two_a.newly_pending), ["versioned-code1"]);
    let retired = |store: &PostgresPersistence| -> Vec<String> {
        store
            .retired_parameters()
            .unwrap()
            .into_iter()
            .map(|param| param.code)
            .collect()
    };
    // What only the replaced version defines.
    assert_eq!(retired(&a), ["versioned-code2"]);
    let held = rows(&a);
    assert_eq!(
        held,
        [
            (
                "versioned-code0".to_string(),
                "Patient.extension0".to_string(),
                ParamStatus::Ready,
                two_a.package_id
            ),
            (
                "versioned-code1".to_string(),
                "Patient.changed".to_string(),
                ParamStatus::Pending,
                two_a.package_id
            ),
        ]
    );
    assert_eq!(versions(&a), ["2.0.0"]);
    assert_eq!(owners(&a), (vec![two_a.package_id], vec![two_a.package_id]));
    // The other tenant is as it was: the shared copy of 1.0.0 is still its package.
    let untouched = rows(&b);
    assert_eq!(untouched.len(), 3);
    assert!(untouched.iter().all(|row| row.3 == one_b.package_id));
    assert_eq!(versions(&b), ["1.0.0"]);
    assert_eq!(owners(&b), (vec![one_b.package_id], vec![one_b.package_id]));

    // The replaced version is out of every list of the tenant that replaced it, but still there to
    // read by key, for a process that serves from the catalog it loaded before. Collected, the
    // tenant has given up its link to the shared copy, which stays for the tenant that still has it.
    let shared_packages = |version: &str| {
        count(
            &url,
            &format!(
                "SELECT count(*) FROM packages \
                 WHERE instance_id = 0 AND name = 'versioned' AND version = '{version}'"
            ),
        )
    };
    let shared_rows = |package: i64| {
        count(
            &url,
            &format!(
                "SELECT (SELECT count(*) FROM base_search_parameters WHERE package_id = {package}) \
                      + (SELECT count(*) FROM base_expansions WHERE package_id = {package}) \
                      + (SELECT count(*) FROM structure_definitions \
                         WHERE instance_id = 0 AND package_id = {package}) \
                      + (SELECT count(*) FROM terminology \
                         WHERE instance_id = 0 AND package_id = {package})"
            ),
        )
    };
    let definition = "http://example.org/versioned/sd/0";
    assert!(a
        .load_structure_definition_json(one_a.package_id, definition)
        .unwrap()
        .is_some());
    a.collect_superseded().unwrap();
    assert_eq!(versions(&a), ["2.0.0"]);
    assert_eq!(shared_packages("1.0.0"), 1);
    assert!(shared_rows(one_a.package_id) > 0);
    assert!(b
        .load_structure_definition_json(one_b.package_id, definition)
        .unwrap()
        .is_some());
    assert_eq!(versions(&b), ["1.0.0"]);

    // It is given the newer version from the copy, and has not marked any of its own parameters
    // ready, so each of the two it keeps is waiting for its index.
    let (two_b, reads) = install(&b, &second, "two").unwrap();
    assert_eq!(reads, 0);
    assert!(two_b.shared);
    assert_eq!(
        codes(&two_b.newly_pending),
        ["versioned-code0", "versioned-code1"]
    );
    assert_eq!(retired(&b), ["versioned-code2"]);
    assert_eq!(rows(&b).len(), 2);
    assert_eq!(versions(&b), ["2.0.0"]);
    assert_eq!(owners(&b), (vec![two_b.package_id], vec![two_b.package_id]));
    // And the first is not disturbed by it.
    assert_eq!(rows(&a), held);

    // The last tenant to give the older version up takes the shared copy of it out of the catalog,
    // with what it held there, and the newer version is still there for both.
    b.collect_superseded().unwrap();
    assert_eq!(versions(&b), ["2.0.0"]);
    assert_eq!(shared_packages("1.0.0"), 0);
    assert_eq!(shared_rows(one_a.package_id), 0);
    assert_eq!(shared_packages("2.0.0"), 1);
    assert!(shared_rows(two_a.package_id) > 0);
    assert_eq!(rows(&a), held);
}

#[test]
fn tenants_share_one_copy_of_a_package_and_the_second_never_reads_it() {
    let Some(admin) = test_url("shared catalog") else {
        return;
    };
    let url = shared(&admin, "tcat", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let package = demo("shared", "4.0.1");

    let (first, first_reads) = install(&a, &package, "sha-1").unwrap();
    assert!(first.inserted && first.shared);
    assert_eq!(
        first_reads, 1,
        "the first tenant builds the copy from the package"
    );
    let (second, second_reads) = install(&b, &package, "sha-1").unwrap();
    assert!(second.inserted && second.shared);
    assert_eq!(
        second_reads, 0,
        "the second tenant is given the copy: the package is not read"
    );
    assert_eq!(first.package_id, second.package_id);
    assert_eq!(first.newly_pending, second.newly_pending);
    assert_eq!(
        first.newly_pending.len(),
        3,
        "each of the package's three parameters waits for an index"
    );

    // Stored once: the definitions and terminology are the shared catalog's, not either tenant's.
    assert_eq!(
        count(&url, "SELECT count(*) FROM packages WHERE instance_id = 0"),
        1
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM structure_definitions WHERE instance_id = 0"
        ),
        3
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM terminology WHERE instance_id = 0"
        ),
        2
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM structure_definitions WHERE instance_id <> 0"
        ),
        0
    );
    assert_eq!(
        count(&url, "SELECT count(*) FROM base_search_parameters"),
        3
    );
    assert_eq!(count(&url, "SELECT count(*) FROM base_expansions"), 2);

    // Both tenants read it, whole and a page at a time, and alike.
    for store in [&a, &b] {
        let packages = store.load_packages().unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(
            (packages[0].id, packages[0].name.as_str()),
            (first.package_id, "shared")
        );
        let rows = definitions(store);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows, paged_definitions(store));
        assert_eq!(terminology(store).len(), 2);
        let pending = store.load_search_parameters().unwrap();
        assert_eq!(pending.len(), 3);
        assert!(pending.iter().all(|row| row.status == ParamStatus::Pending));
        assert_eq!(store.load_expansions().unwrap().len(), 2);
        let (id, url) = (first.package_id, &package.structure_definitions[1].url);
        assert_eq!(
            store
                .load_structure_definition_json(id, url)
                .unwrap()
                .as_deref(),
            Some(package.structure_definitions[1].json.as_str())
        );
    }
    assert_eq!(definitions(&a), definitions(&b));
    // The parts that change per tenant are the tenant's own.
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM search_parameters WHERE instance_id <> 0"
        ),
        6
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM value_set_expansion WHERE instance_id <> 0"
        ),
        4
    );
}

#[test]
fn a_tenants_own_terminology_changes_only_its_own_expansions() {
    let Some(admin) = test_url("shared catalog changes") else {
        return;
    };
    let url = shared(&admin, "tcatown", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let package = demo("shared", "4.0.1");
    let (installed, _) = install(&a, &package, "sha-1").unwrap();
    install(&b, &package, "sha-1").unwrap();
    let before_b = (b.load_expansions().unwrap(), terminology(&b));
    let base_before = count(&url, "SELECT sum(length(json)) FROM base_expansions");

    // Tenant a writes a code system of its own and changes an expansion of the shared package.
    let local = a.ensure_local_package("4.0.1").unwrap();
    a.upsert_terminology(
        local,
        &NewTerminology {
            resource_type: "CodeSystem".into(),
            url: "http://example.org/a-own".into(),
            json: r#"{"resourceType":"CodeSystem","own":true}"#.into(),
        },
    )
    .unwrap();
    a.apply_expansions(&[ExpansionChange {
        package_id: installed.package_id,
        url: package.expansions[0].url.clone(),
        json: Some(r#"{"expansion":"changed by a"}"#.into()),
    }])
    .unwrap();

    let changed: Vec<String> = a
        .load_expansions()
        .unwrap()
        .into_iter()
        .map(|row| row.json)
        .collect();
    assert!(changed.iter().any(|json| json.contains("changed by a")));
    assert_eq!(
        b.load_expansions().unwrap(),
        before_b.0,
        "tenant b's expansions changed"
    );
    assert_eq!(
        terminology(&b),
        before_b.1,
        "tenant b was given tenant a's code system"
    );
    assert_eq!(
        count(&url, "SELECT sum(length(json)) FROM base_expansions"),
        base_before,
        "the shared copy changed"
    );

    // Tenant a has the shared package's terminology and then its own, in the order it got them.
    let order: Vec<i64> = terminology(&a).into_iter().map(|(id, _)| id).collect();
    assert_eq!(order, [installed.package_id, installed.package_id, local]);
    // A shared package is the same for every tenant: an install cannot add terminology to it.
    let (again, reads) = install(&a, &package, "sha-1").unwrap();
    assert!(!again.shared || reads == 0);
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM terminology WHERE instance_id = 0"
        ),
        2
    );
}

#[test]
fn a_package_made_from_other_content_is_stored_for_the_tenant_alone() {
    let Some(admin) = test_url("shared catalog digest") else {
        return;
    };
    let url = shared(&admin, "tcatdigest", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let package = demo("shared", "4.0.1");
    let (shared_copy, _) = install(&a, &package, "sha-1").unwrap();

    // The same name and version, other content: not given the shared copy, and not read as it.
    let mut other = demo("shared", "4.0.1");
    other.structure_definitions[0].json =
        r#"{"resourceType":"StructureDefinition","changed":1}"#.into();
    let (own, reads) = install(&b, &other, "sha-2").unwrap();
    assert!(!own.shared, "other content was given the shared copy");
    assert_eq!(reads, 1);
    assert_ne!(own.package_id, shared_copy.package_id);

    let b_rows = definitions(&b);
    assert_eq!(b_rows.len(), 3);
    assert!(b_rows.iter().any(|(_, _, json)| json.contains("changed")));
    assert!(definitions(&a)
        .iter()
        .all(|(_, _, json)| !json.contains("changed")));
    assert_eq!(
        count(&url, "SELECT count(*) FROM packages WHERE instance_id = 0"),
        1
    );
}

#[test]
fn tenants_that_start_together_build_a_package_once() {
    let Some(admin) = test_url("shared catalog concurrency") else {
        return;
    };
    let names: Vec<String> = (0..6).map(|n| format!("t{n}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let url = shared(&admin, "tcatrace", &refs);
    let package = Arc::new(demo("shared", "4.0.1"));
    let reads = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(names.len()));
    let handles: Vec<_> = names
        .iter()
        .map(|name| {
            let (url, name, package) = (url.clone(), name.clone(), package.clone());
            let (reads, start) = (reads.clone(), start.clone());
            std::thread::spawn(move || {
                let store = open(&url, &name);
                start.wait();
                let (outcome, read) = install(&store, &package, "sha-1").unwrap();
                reads.fetch_add(read, Ordering::SeqCst);
                outcome.package_id
            })
        })
        .collect();
    let ids: Vec<i64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "the package was read once, by one tenant"
    );
    assert_eq!(
        count(&url, "SELECT count(*) FROM packages WHERE instance_id = 0"),
        1
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM tenant_packages WHERE instance_id > 0"
        ),
        6
    );
}

#[test]
fn a_build_that_fails_leaves_no_shared_package_visible() {
    let Some(admin) = test_url("shared catalog failed build") else {
        return;
    };
    let url = shared(&admin, "tcatfail", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let package = demo("shared", "4.0.1");

    let failed = a.install_shared_package_in_pieces(
        &package.name,
        &package.version,
        &package.fhir_version,
        "sha-1",
        &mut |rows| {
            rows.structure_definitions(&package.structure_definitions)?;
            Err(InstallError::Persist(PersistError::Definite(
                "the source ended".into(),
            )))
        },
    );
    assert!(failed.is_err());
    assert_eq!(
        count(&url, "SELECT count(*) FROM packages WHERE instance_id = 0"),
        0
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM structure_definitions WHERE instance_id = 0"
        ),
        0,
        "half a package was left behind"
    );
    assert!(a.load_packages().unwrap().is_empty());

    // Another tenant builds it, and the one that failed gets it.
    let (built, reads) = install(&b, &package, "sha-1").unwrap();
    assert_eq!(reads, 1);
    let (given, reads) = install(&a, &package, "sha-1").unwrap();
    assert_eq!((reads, given.package_id), (0, built.package_id));
}

#[test]
fn the_shared_catalog_is_read_only_to_a_tenants_process() {
    let Some(admin) = test_url("shared catalog guard") else {
        return;
    };
    let url = shared(&admin, "tcatguard", &["a"]);
    let a = open(&url, "a");
    install(&a, &demo("shared", "4.0.1"), "sha-1").unwrap();
    let mut c = client(&url);

    let refused = |c: &mut postgres::Client, sql: &str| -> String {
        c.batch_execute(sql)
            .expect_err(&format!("{sql} was allowed"))
            .to_string()
            + &c.query("SELECT 1", &[])
                .map(|_| String::new())
                .unwrap_or_default()
    };
    for sql in [
        "UPDATE structure_definitions SET json = 'x' WHERE instance_id = 0",
        "DELETE FROM terminology WHERE instance_id = 0",
        "DELETE FROM packages WHERE instance_id = 0",
        "UPDATE packages SET digest = 'other' WHERE instance_id = 0",
        "INSERT INTO base_expansions (package_id, url, json) \
         SELECT id, 'u', '{}' FROM packages WHERE instance_id = 0",
        "DELETE FROM base_search_parameters",
        // A row cannot be moved out of the shared catalog, or into it.
        "UPDATE structure_definitions SET instance_id = 1 WHERE instance_id = 0",
    ] {
        // Each statement is its own transaction, so a refusal leaves nothing to roll back.
        let text = refused(&mut c, sql);
        assert!(text.contains("db error"), "{sql}: {text}");
    }
    let message = c
        .batch_execute("DELETE FROM packages WHERE instance_id = 0")
        .unwrap_err();
    assert!(
        message
            .as_db_error()
            .is_some_and(|db| db.message().contains("read-only")),
        "{message:?}"
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM structure_definitions WHERE instance_id = 0"
        ),
        3
    );

    // A tenant's own rows are its to change.
    let own = a.ensure_local_package("4.0.1").unwrap();
    c.batch_execute(&format!(
        "UPDATE packages SET digest = 'mine' WHERE id = {own}"
    ))
    .expect("a tenant's own package");
}

#[test]
fn deleting_a_tenant_leaves_the_shared_catalog_and_the_other_tenants() {
    let Some(admin) = test_url("shared catalog delete") else {
        return;
    };
    let url = shared(&admin, "tcatdelete", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let package = demo("shared", "4.0.1");
    install(&a, &package, "sha-1").unwrap();
    install(&b, &package, "sha-1").unwrap();
    a.ensure_local_package("4.0.1").unwrap();
    let before = (
        definitions(&b),
        terminology(&b),
        b.load_expansions().unwrap(),
    );
    a.release_writer_lease();
    drop(a);

    delete_tenant(&crate::tenancy_tests::operator_url(&url), "a").expect("delete tenant a");

    assert_eq!(
        count(&url, "SELECT count(*) FROM packages WHERE instance_id = 0"),
        1
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM structure_definitions WHERE instance_id = 0"
        ),
        3
    );
    assert_eq!(
        count(&url, "SELECT count(*) FROM base_search_parameters"),
        3
    );
    assert_eq!(count(&url, "SELECT count(*) FROM base_expansions"), 2);
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM tenant_packages WHERE instance_id = 1"
        ),
        0
    );
    assert_eq!(
        (
            definitions(&b),
            terminology(&b),
            b.load_expansions().unwrap()
        ),
        before,
        "deleting tenant a changed what tenant b reads"
    );
}

#[test]
fn a_tenant_reads_its_packages_in_the_order_it_got_them() {
    let Some(admin) = test_url("shared catalog order") else {
        return;
    };
    let url = shared(&admin, "tcatorder", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let (one, two) = (demo("first", "4.0.1"), demo("second", "4.0.1"));
    // The shared copies are made in tenant a's order, so `first` has the lower id; tenant b got
    // them the other way round, and reads them that way.
    install(&a, &one, "sha-1").unwrap();
    install(&a, &two, "sha-2").unwrap();
    install(&b, &two, "sha-2").unwrap();
    install(&b, &one, "sha-1").unwrap();

    let names = |store: &PostgresPersistence| -> Vec<String> {
        store
            .load_packages()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect()
    };
    assert_eq!(names(&a), ["first", "second"]);
    assert_eq!(names(&b), ["second", "first"]);
    for store in [&a, &b] {
        let loaded: Vec<i64> = definitions(store)
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        let paged: Vec<i64> = paged_definitions(store)
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(loaded, paged);
        let order: Vec<i64> = store
            .load_packages()
            .unwrap()
            .iter()
            .map(|p| p.id)
            .collect();
        let mut seen: Vec<i64> = loaded.clone();
        seen.dedup();
        assert_eq!(seen, order, "definitions follow the order of the packages");
    }
}

#[test]
fn a_package_for_another_release_is_refused_as_it_is_without_tenants() {
    let Some(admin) = test_url("shared catalog release") else {
        return;
    };
    let url = shared(&admin, "tcatrelease", &["a"]);
    let a = open(&url, "a");
    install(&a, &demo("r4", "4.0.1"), "sha-1").unwrap();
    let refused = install(&a, &demo("r5", "5.0.0"), "sha-2").unwrap_err();
    assert!(
        matches!(refused, InstallError::FhirVersion { .. }),
        "{refused:?}"
    );
    assert_eq!(a.load_packages().unwrap().len(), 1);
}

/// Two tenants given the same shared package, so each holds its own copy of the package's search
/// parameters under one package id. A statement that is keyed by package, url and base and forgets
/// the tenant would change both copies at once.
struct SamePackage {
    url: String,
    a: PostgresPersistence,
    b: PostgresPersistence,
    package_id: i64,
    package: NewPackage,
}

fn same_package(tag: &str) -> Option<SamePackage> {
    let admin = test_url("tenants holding one shared package")?;
    let url = shared(&admin, tag, &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let package = demo("same", "4.0.1");
    let (first, _) = install(&a, &package, "sha-same").unwrap();
    let (second, _) = install(&b, &package, "sha-same").unwrap();
    assert_eq!(first.package_id, second.package_id);
    Some(SamePackage {
        url,
        a,
        b,
        package_id: first.package_id,
        package,
    })
}

/// (url, status, json) of the tenant's copy of the package's search parameters, by url.
fn parameters(store: &PostgresPersistence, package_id: i64) -> Vec<(String, String, String)> {
    let mut rows: Vec<_> = store
        .load_search_parameters_of(package_id)
        .unwrap()
        .into_iter()
        .map(|row| (row.url, row.status.as_str().to_string(), row.json))
        .collect();
    rows.sort();
    rows
}

fn statuses(rows: &[(String, String, String)]) -> Vec<&str> {
    rows.iter().map(|row| row.1.as_str()).collect()
}

#[test]
fn a_tenant_changing_a_shared_packages_parameter_leaves_the_other_tenants_copy_alone() {
    let Some(same) = same_package("tsamepar") else {
        return;
    };
    let SamePackage {
        url,
        a,
        b,
        package_id,
        package,
    } = same;
    let (dropped, unsupported, rewritten) = (
        &package.search_parameters[0],
        &package.search_parameters[2].url,
        &package.search_parameters[1],
    );
    // Both tenants hold the third parameter as unsupported, as indexing can leave it.
    client(&url)
        .batch_execute(&format!(
            "UPDATE search_parameters SET status = 'unsupported' \
             WHERE instance_id <> 0 AND url = '{unsupported}'"
        ))
        .unwrap();
    let before = digests(&url, "b");
    assert_eq!(statuses(&parameters(&b, package_id))[2], "unsupported");

    // One tenant's indexing finds it supported: only its copy changes.
    a.set_search_parameter_status(package_id, unsupported, "Patient", ParamStatus::Pending)
        .unwrap();
    assert_eq!(
        statuses(&parameters(&a, package_id)),
        ["pending", "pending", "pending"]
    );
    assert_eq!(
        statuses(&parameters(&b, package_id)),
        ["pending", "pending", "unsupported"]
    );
    assert_eq!(digests(&url, "b"), before);

    // A definition that says the same with other json is rewritten in place, in its copy only.
    let mut again = rewritten.clone();
    again.json = r#"{"resourceType":"SearchParameter","rewritten":true}"#.into();
    assert!(
        !a.upsert_search_parameter(package_id, &again).unwrap(),
        "the same definition is a rewrite, not a new parameter"
    );
    let ours = parameters(&a, package_id);
    assert_eq!(ours[1].2, again.json);
    assert_eq!(digests(&url, "b"), before);

    // One dropped from a tenant's copy stays in the other's.
    a.delete_search_parameter(package_id, &dropped.url).unwrap();
    assert_eq!(parameters(&a, package_id).len(), 2);
    assert_eq!(parameters(&b, package_id).len(), 3);
    assert_eq!(digests(&url, "b"), before);
}

#[test]
fn backfills_of_tenants_on_one_package_snapshot_and_finish_apart() {
    let Some(same) = same_package("tsamejob") else {
        return;
    };
    let SamePackage {
        url,
        a,
        b,
        package_id,
        ..
    } = same;
    // Each tenant's job snapshots the parameters waiting in its own copy, three each.
    let b_job = b.insert_backfill(package_id, 5, "Patient", &[]).unwrap();
    let a_job = a.insert_backfill(package_id, 5, "Patient", &[]).unwrap();
    assert_ne!(a_job, b_job);
    for tenant in ["a", "b"] {
        let id = crate::tenancy_tests::tenant_id(&url, tenant);
        assert_eq!(
            count(
                &url,
                &format!("SELECT count(*) FROM backfill_parameter WHERE instance_id = {id}")
            ),
            3,
            "tenant {tenant}'s snapshot"
        );
    }
    let before = digests(&url, "b");

    // One tenant finishing its job makes its parameters ready, and not the other's.
    a.complete_backfill(a_job, package_id, "Patient").unwrap();
    assert_eq!(
        statuses(&parameters(&a, package_id)),
        ["ready", "ready", "ready"]
    );
    assert_eq!(
        statuses(&parameters(&b, package_id)),
        ["pending", "pending", "pending"]
    );
    assert_eq!(digests(&url, "b"), before);

    // And the base of a package, by itself, the same way.
    b.mark_base_ready(package_id, "Patient").unwrap();
    assert_eq!(
        statuses(&parameters(&b, package_id)),
        ["ready", "ready", "ready"]
    );
    assert_eq!(
        statuses(&parameters(&a, package_id)),
        ["ready", "ready", "ready"]
    );
}

#[test]
fn a_tenant_does_not_read_another_tenants_snapshot_of_a_job() {
    let Some(same) = same_package("tsamesnap") else {
        return;
    };
    let SamePackage {
        url,
        a,
        b,
        package_id,
        package,
    } = same;
    // Tenant b's job names one parameter. Tenant a has no snapshot under that job id, so for it the
    // job covers whatever is waiting for the package, as any job without a snapshot does: it must
    // not take b's one-parameter snapshot for its own.
    let b_job = b
        .insert_backfill(
            package_id,
            5,
            "Patient",
            &[package.search_parameters[0].url.clone()],
        )
        .unwrap();
    let before = digests(&url, "b");
    a.complete_backfill(b_job, package_id, "Patient").unwrap();
    assert_eq!(
        statuses(&parameters(&a, package_id)),
        ["ready", "ready", "ready"]
    );
    assert_eq!(digests(&url, "b"), before);
}

#[test]
fn a_tenant_waits_for_as_long_as_another_tenant_builds_the_package() {
    let Some(admin) = test_url("shared catalog slow build") else {
        return;
    };
    let url = shared(&admin, "tcatwait", &["a", "b"]);
    let package = Arc::new(demo("slow", "4.0.1"));
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let building = Arc::new(Barrier::new(2));
    let builder = {
        let (package, building) = (package.clone(), building.clone());
        std::thread::spawn(move || {
            a.install_shared_package_in_pieces(
                &package.name,
                &package.version,
                &package.fhir_version,
                "sha-slow",
                &mut |rows| {
                    // The build is under way and holds the package's lock for a while.
                    building.wait();
                    std::thread::sleep(Duration::from_millis(1500));
                    rows.structure_definitions(&package.structure_definitions)?;
                    rows.search_parameters(&package.search_parameters)?;
                    rows.terminology(&package.terminology)?;
                    rows.expansions(&package.expansions)?;
                    Ok(())
                },
            )
            .unwrap()
        })
    };
    building.wait();
    let started = Instant::now();
    let (outcome, reads) = install(&b, &package, "sha-slow").unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "tenant b did not wait for the build ({:?})",
        started.elapsed()
    );
    assert_eq!(reads, 0, "tenant b read the package instead of waiting");
    assert_eq!(builder.join().unwrap().package_id, outcome.package_id);
}

#[test]
fn a_shared_copy_that_lost_rows_is_not_given_to_another_tenant() {
    let Some(admin) = test_url("shared catalog damage") else {
        return;
    };
    let url = shared(&admin, "tcatdamage", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let package = demo("damaged", "4.0.1");
    install(&a, &package, "sha-d").unwrap();
    // Something removes a definition from the shared copy (as an administrator, saying it is the
    // installer: the guard that keeps a tenant's process from doing it does not bind this).
    client(&url)
        .batch_execute(
            "SET refhir.shared_writer = 'on'; \
             DELETE FROM structure_definitions WHERE instance_id = 0 AND url LIKE '%/sd/1'",
        )
        .unwrap();
    let refused = format!("{:?}", install(&b, &package, "sha-d").expect_err("refused"));
    assert!(refused.contains("is damaged"), "{refused}");
    assert!(
        b.load_packages().unwrap().is_empty(),
        "tenant b was given a damaged package"
    );
}

/// A database in the shared layout with tenants `a` and `c`, in which `a` has installed 1.0.0 of a
/// package that tenants share and then 2.0.0, so that 1.0.0 is a package `a` gives up the next time
/// it collects, and the last tenant to do so removes it from the shared catalog. Returns the URL,
/// both tenants, the 1.0.0 package as `a` installed it, and its id.
fn a_shared_package_that_a_has_replaced(
    admin: &str,
    tag: &str,
) -> (
    String,
    PostgresPersistence,
    PostgresPersistence,
    NewPackage,
    i64,
) {
    let url = shared(admin, tag, &["a", "c"]);
    let (a, c) = (open(&url, "a"), open(&url, "c"));
    let first = demo("raced", "4.0.1");
    let (one, _) = install(&a, &first, "one").unwrap();
    let mut second = demo("raced", "4.0.1");
    second.version = "2.0.0".into();
    install(&a, &second, "two").unwrap();
    (url, a, c, first, one.package_id)
}

/// Waits until `blocked` sessions of the database are waiting for a lock, which is how a test knows
/// that a statement it started is held up where it means it to be.
fn wait_for_sessions_blocked(url: &str, blocked: i64) {
    let started = Instant::now();
    loop {
        let waiting = count(
            url,
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        );
        if waiting == blocked {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "{waiting} sessions are waiting for a lock, not {blocked}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How many rows of the shared catalog the package holds: its parameters, expansions, definitions
/// and terminology, and its own row.
fn shared_catalog_rows(url: &str, package: i64) -> i64 {
    count(
        url,
        &format!(
            "SELECT (SELECT count(*) FROM base_search_parameters WHERE package_id = {package}) \
                  + (SELECT count(*) FROM base_expansions WHERE package_id = {package}) \
                  + (SELECT count(*) FROM structure_definitions \
                     WHERE instance_id = 0 AND package_id = {package}) \
                  + (SELECT count(*) FROM terminology \
                     WHERE instance_id = 0 AND package_id = {package}) \
                  + (SELECT count(*) FROM packages WHERE instance_id = 0 AND id = {package})"
        ),
    )
}

/// A tenant that is being given a shared package links it before it reads anything else of it, and
/// the tenant that gives it up locks it before it removes anything. When the link comes first the
/// removal waits for it, finds that a tenant has the package, and leaves it whole.
#[test]
fn a_tenant_given_a_shared_package_keeps_it_from_the_tenant_that_gives_it_up() {
    let Some(admin) = test_url("shared package race, activation first") else {
        return;
    };
    let (url, a, c, first, package) = a_shared_package_that_a_has_replaced(&admin, "traceact");
    let held = shared_catalog_rows(&url, package);
    assert!(held > 1);

    // A lock on a table of the shared catalog holds the activation up after it has linked the
    // package, where it counts what the copy holds.
    let mut locker = client(&url);
    locker
        .batch_execute("BEGIN; LOCK TABLE base_search_parameters IN ACCESS EXCLUSIVE MODE")
        .unwrap();
    std::thread::scope(|scope| {
        let given = scope.spawn(|| install(&c, &first, "one").unwrap());
        wait_for_sessions_blocked(&url, 1);
        let collected = scope.spawn(|| a.collect_superseded().unwrap());
        // The removal waits for the lock on the package that the link holds.
        wait_for_sessions_blocked(&url, 2);
        assert!(!collected.is_finished());
        locker.batch_execute("COMMIT").unwrap();
        collected.join().unwrap();
        let (outcome, reads) = given.join().unwrap();
        assert_eq!(
            outcome.package_id, package,
            "c was given the copy that was there"
        );
        assert!(outcome.shared);
        assert_eq!(reads, 0, "c did not build a copy of its own");
    });

    assert_eq!(
        shared_catalog_rows(&url, package),
        held,
        "the shared copy is whole"
    );
    assert_eq!(
        c.load_packages()
            .unwrap()
            .into_iter()
            .map(|package| (package.name, package.version, package.id))
            .collect::<Vec<_>>(),
        [("raced".to_string(), "1.0.0".to_string(), package)]
    );
    assert!(c
        .load_structure_definition_json(package, "http://example.org/raced/sd/0")
        .unwrap()
        .is_some());
    // The tenant that gave it up has only the newer version.
    let versions: Vec<_> = a
        .load_packages()
        .unwrap()
        .into_iter()
        .map(|package| package.version)
        .collect();
    assert_eq!(versions, ["2.0.0"]);
}

/// The other order: the removal has locked the package, and a tenant that is being given it waits
/// for the removal, finds that the package is not there, and is given a copy built anew.
#[test]
fn a_tenant_given_a_shared_package_that_is_being_removed_gets_a_copy_built_anew() {
    let Some(admin) = test_url("shared package race, removal first") else {
        return;
    };
    let (url, a, c, first, package) = a_shared_package_that_a_has_replaced(&admin, "tracerem");
    assert!(shared_catalog_rows(&url, package) > 1);

    // A lock on a table of the shared catalog holds the removal up after it has locked the package
    // and begun to remove what it holds.
    let mut locker = client(&url);
    locker
        .batch_execute("BEGIN; LOCK TABLE base_expansions IN ACCESS EXCLUSIVE MODE")
        .unwrap();
    std::thread::scope(|scope| {
        let collected = scope.spawn(|| a.collect_superseded().unwrap());
        wait_for_sessions_blocked(&url, 1);
        let given = scope.spawn(|| install(&c, &first, "one").unwrap());
        // The link waits for the lock the removal holds on the package.
        wait_for_sessions_blocked(&url, 2);
        assert!(!given.is_finished());
        locker.batch_execute("COMMIT").unwrap();
        collected.join().unwrap();
        let (outcome, reads) = given.join().unwrap();
        assert_ne!(
            outcome.package_id, package,
            "c was not given the copy that was removed"
        );
        assert!(outcome.shared);
        assert_eq!(reads, 1, "c built the copy from the package");
    });

    assert_eq!(
        shared_catalog_rows(&url, package),
        0,
        "the removed copy is gone"
    );
    let held = c.load_packages().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(
        (held[0].name.as_str(), held[0].version.as_str()),
        ("raced", "1.0.0")
    );
    assert_ne!(held[0].id, package);
    assert!(c
        .load_structure_definition_json(held[0].id, "http://example.org/raced/sd/0")
        .unwrap()
        .is_some());
}

/// A tenant gives up a shared package and the package leaves the shared catalog in one
/// transaction. When the removal fails the tenant still has its link, so that the next collection
/// finds the package again; if the link were gone, nothing would name a package nobody has.
#[test]
fn a_removal_of_a_shared_package_that_fails_leaves_the_tenant_its_link() {
    let Some(admin) = test_url("shared package removal that fails") else {
        return;
    };
    let (url, a, _c, _first, package) = a_shared_package_that_a_has_replaced(&admin, "tracefail");
    let held = shared_catalog_rows(&url, package);
    let links = |url: &str| {
        count(
            url,
            &format!("SELECT count(*) FROM tenant_packages WHERE package_id = {package}"),
        )
    };
    assert_eq!(links(&url), 1);

    // Something in the database refuses to let the package's parameters go.
    client(&url)
        .batch_execute(
            "CREATE FUNCTION refuse_removal() RETURNS trigger LANGUAGE plpgsql AS \
             $f$ BEGIN RAISE EXCEPTION 'removal refused'; END $f$; \
             CREATE TRIGGER refuse_removal BEFORE DELETE ON base_search_parameters \
             FOR EACH ROW EXECUTE FUNCTION refuse_removal()",
        )
        .unwrap();
    assert!(a.collect_superseded().is_err());
    assert_eq!(
        links(&url),
        1,
        "the tenant gave the package up, and it is still there"
    );
    assert_eq!(shared_catalog_rows(&url, package), held);

    client(&url)
        .batch_execute("DROP TRIGGER refuse_removal ON base_search_parameters")
        .unwrap();
    a.collect_superseded().unwrap();
    assert_eq!(links(&url), 0);
    assert_eq!(shared_catalog_rows(&url, package), 0);
}
