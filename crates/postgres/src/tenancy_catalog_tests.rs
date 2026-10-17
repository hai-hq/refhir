//! The catalog tenants share, against a real Postgres (`REFHIR_TEST_POSTGRES_URL`).
//!
//! A package that tenants have in common is stored once. The first tenant to ask for it builds the
//! copy; each other tenant is given it without the package being read again, and gets its own
//! search parameters and expansions, which are the parts that change per tenant.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

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
