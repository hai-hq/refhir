//! What a tenant can do with another tenant's identifiers: nothing.
//!
//! Many statements are keyed by an id that no two tenants share (a package, a backfill), so the
//! tenant column adds nothing to them unless something hands them the wrong tenant's id. These
//! tests do exactly that: tenant `a` is given tenant `b`'s ids, urls and job names, and calls every
//! method that takes one. Whatever `b` held must be exactly as it was, and `a` must find nothing.

use refhir_persistence::{
    ConformanceStore, ExpansionChange, JobStatus, JobStore, NewExpansion, NewPackage,
    NewSearchParameter, NewStructureDefinition, NewTerminology, ParamStatus, StoredBackfill,
};

use crate::tenancy_tests::{digests, open, shared};
use crate::tests::test_url;

fn package(name: &str, fhir_version: &str) -> NewPackage {
    NewPackage {
        name: name.to_string(),
        version: "1.0.0".into(),
        fhir_version: fhir_version.to_string(),
        structure_definitions: vec![NewStructureDefinition {
            url: format!("http://example.org/{name}/sd"),
            type_name: "Thing".into(),
            kind: "resource".into(),
            is_abstract: false,
            json: r#"{"resourceType":"StructureDefinition"}"#.into(),
        }],
        search_parameters: vec![NewSearchParameter {
            url: format!("http://example.org/{name}/sp"),
            base: "Patient".into(),
            code: format!("{name}-code"),
            param_type: "string".into(),
            expression: Some("Patient.name".into()),
            status: ParamStatus::Pending,
            json: r#"{"resourceType":"SearchParameter"}"#.into(),
        }],
        terminology: vec![NewTerminology {
            resource_type: "CodeSystem".into(),
            url: format!("http://example.org/{name}/cs"),
            json: r#"{"resourceType":"CodeSystem"}"#.into(),
        }],
        expansions: vec![NewExpansion {
            url: format!("http://example.org/{name}/vs"),
            json: r#"{"expansion":1}"#.into(),
        }],
    }
}

#[test]
fn a_tenant_cannot_act_on_another_tenants_catalog_ids() {
    let Some(admin) = test_url("foreign catalog ids") else {
        return;
    };
    let url = shared(&admin, "tforeign", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));

    // Tenant b holds a package of its own, the work of indexing it, and a local package.
    let held = package("held", "4.0.1");
    let installed = b.install_package(&held).unwrap();
    let pkg = installed.package_id;
    let backfill = b.insert_backfill(pkg, 5, "Patient", &[]).unwrap();
    let local = b.ensure_local_package("4.0.1").unwrap();
    b.upsert_terminology(
        local,
        &NewTerminology {
            resource_type: "CodeSystem".into(),
            url: "http://example.org/b-local".into(),
            json: r#"{"resourceType":"CodeSystem","local":true}"#.into(),
        },
    )
    .unwrap();
    let kept = digests(&url, "b");
    let sd = held.structure_definitions[0].url.clone();
    let sp = held.search_parameters[0].url.clone();
    let cs = held.terminology[0].url.clone();
    let vs = held.expansions[0].url.clone();

    // Tenant a asks for tenant b's rows by their ids, and finds none of them.
    assert_eq!(a.load_structure_definition_json(pkg, &sd).unwrap(), None);
    assert_eq!(a.load_terminology_json(pkg, &cs).unwrap(), None);
    assert_eq!(a.load_expansion_json(pkg, &vs).unwrap(), None);
    assert!(a.load_terminology_of(pkg).unwrap().is_empty());
    assert!(a.load_terminology_of(local).unwrap().is_empty());
    assert!(a.load_search_parameters_of(pkg).unwrap().is_empty());
    assert!(a.load_backfills().unwrap().is_empty());
    assert!(a.load_packages().unwrap().is_empty());

    // And changes none of them.
    a.delete_search_parameter(pkg, &sp).unwrap();
    a.delete_terminology(pkg, &cs).unwrap();
    a.delete_terminology(local, "http://example.org/b-local")
        .unwrap();
    a.apply_expansions(&[ExpansionChange {
        package_id: pkg,
        url: vs.clone(),
        json: None,
    }])
    .unwrap();
    a.apply_expansions(&[ExpansionChange {
        package_id: pkg,
        url: vs.clone(),
        json: Some("{\"expansion\":\"a's\"}".into()),
    }])
    .unwrap();
    a.replace_expansions(pkg, &[]).unwrap();
    a.set_search_parameter_status(pkg, &sp, "Patient", ParamStatus::Unsupported)
        .unwrap();
    a.mark_base_ready(pkg, "Patient").unwrap();
    a.complete_backfill(backfill, pkg, "Patient").unwrap();
    a.update_backfill(backfill, Some("a's cursor"), true)
        .unwrap();
    assert!(
        !a.claim_backfill(backfill).unwrap(),
        "tenant a claimed a backfill of tenant b"
    );
    a.complete_empty_backfills(&[StoredBackfill {
        id: backfill,
        package_id: pkg,
        watermark_ts: 5,
        tablet: "Patient".into(),
        cursor_id: None,
        done: false,
        claimed_epoch: None,
    }])
    .unwrap();
    // Jobs that tenant a makes for a package of tenant b read tenant a's search parameters (none),
    // and not tenant b's.
    a.insert_backfills(pkg, 9, &["Patient".to_string()])
        .unwrap();
    assert_eq!(
        crate::tenancy_tests::rows_of(&url, "b", "backfill_parameter"),
        1,
        "a backfill of tenant a took tenant b's search parameter"
    );
    assert_eq!(
        crate::tenancy_tests::rows_of(&url, "a", "backfill_parameter"),
        0
    );
    // Tenant a's own install of the same name and version is its own package, not tenant b's.
    let own = a.install_package(&held).unwrap();
    assert!(own.inserted);
    assert_ne!(own.package_id, pkg);

    // The backfill rows that tenant a's calls made are tenant a's; what tenant b held is intact.
    let after = digests(&url, "b");
    assert_eq!(after, kept, "tenant a changed what tenant b held");
    assert_eq!(b.load_backfills().unwrap().len(), 1);
    assert!(!b.load_backfills().unwrap()[0].done);
}

#[test]
fn tenants_count_catalog_changes_separately_and_each_has_a_local_package_and_a_release() {
    let Some(admin) = test_url("tenant catalog counters") else {
        return;
    };
    let url = shared(&admin, "tcounters", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));

    assert_eq!(a.bump_catalog_version().unwrap(), 1);
    assert_eq!(a.bump_catalog_version().unwrap(), 2);
    assert_eq!(b.bump_catalog_version().unwrap(), 1);
    assert_eq!(b.bump_catalog_revision().unwrap(), 1);
    assert_eq!(
        (a.catalog_version().unwrap(), b.catalog_version().unwrap()),
        (2, 1)
    );
    assert_eq!(
        (a.catalog_revision().unwrap(), b.catalog_revision().unwrap()),
        (0, 1)
    );
    assert_eq!(
        (
            a.terminology_version().unwrap(),
            b.terminology_version().unwrap()
        ),
        (0, 0)
    );

    // Each tenant has a local package of its own, and its own release.
    let (local_a, local_b) = (
        a.ensure_local_package("4.0.1").unwrap(),
        b.ensure_local_package("5.0.0").unwrap(),
    );
    assert_ne!(local_a, local_b);
    assert_eq!(a.ensure_local_package("4.0.1").unwrap(), local_a);
    assert_eq!(b.ensure_local_package("5.0.0").unwrap(), local_b);
    let release = |store: &crate::PostgresPersistence| -> Vec<String> {
        store
            .load_packages()
            .unwrap()
            .into_iter()
            .map(|p| p.fhir_version)
            .collect()
    };
    assert_eq!(release(&a), ["4.0.1"]);
    assert_eq!(release(&b), ["5.0.0"]);

    // A package for another release is refused for the tenant on the first, and not for the other.
    let other = package("later", "5.0.0");
    assert!(a.install_package(&other).is_err());
    assert!(b.install_package(&other).is_ok());
}

#[test]
fn a_tenant_cannot_renew_start_or_finish_another_tenants_job() {
    let Some(admin) = test_url("foreign jobs") else {
        return;
    };
    let url = shared(&admin, "tforeignjobs", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    b.job_create("export-1", "export", &serde_json::json!({}), 1000, None)
        .unwrap();
    let claimed = b.job_claim("worker", &["export"], 1000, 500, 1).unwrap();
    assert_eq!(claimed.len(), 1);
    let kept = digests(&url, "b");

    // Tenant a has no job of that name: nothing it does with the name or the worker finds one.
    assert!(!a.job_renew("export-1", "worker", 1100, 500).unwrap());
    assert_eq!(a.job_start("export-1", "worker", 1100, 500).unwrap(), None);
    assert!(!a
        .job_finish(
            "export-1",
            "worker",
            JobStatus::Completed,
            Some(&serde_json::json!({"a": 1})),
            None,
            1200
        )
        .unwrap());
    assert_eq!(a.job_get("export-1").unwrap(), None);
    assert!(a
        .job_claim("other", &["export"], 2000, 500, 5)
        .unwrap()
        .is_empty());
    assert_eq!(digests(&url, "b"), kept, "tenant a changed tenant b's job");

    // The job is still tenant b's to finish.
    assert!(b
        .job_finish(
            "export-1",
            "worker",
            JobStatus::Completed,
            Some(&serde_json::json!({})),
            None,
            1300
        )
        .unwrap());
}
