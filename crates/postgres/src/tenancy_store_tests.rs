//! The four stores (conformance, auth, jobs, governance) in the shared layout.
//!
//! Each runs the same suite the classic layout runs, as one tenant, with another tenant already
//! holding what that suite writes (same tag, so the same ids, keys and days). The suite asserts
//! exact results, so a statement that forgot its tenant fails it; the other tenant's rows must
//! also be exactly what they were afterwards, so a statement that wrote or deleted across tenants
//! fails too. A decoy changes the other tenant's rows where the suite alone would leave them equal
//! to the first tenant's.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use refhir_persistence::{auth, conformance, governance_checks, jobs, GovernanceStore, JobStore};

use crate::tenancy_tests::{
    check_twins, client, digests, open, poison, poison_exact, shared, tenant_id,
};
use crate::tests::test_url;

fn unique() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Runs `run(url, tenant)` for tenant `b`, then `decoy` (SQL, with `{b}` standing for b's id) over
/// b's rows, then `run` for tenant `a`, and requires b's rows to be exactly as they were before
/// a ran.
fn beside_another_tenant(what: &str, tag: &str, decoy: &str, run: impl Fn(&str, &str) + Sync) {
    let Some(admin) = test_url(what) else {
        return;
    };
    // `twins` and `exact` get a twin of every row `a` and `b` write, as it is written: a decoy one
    // and an exact copy. `b` holds what the suite writes before `a` starts (and, with the decoy, a
    // version that is live and wrong).
    let url = shared(&admin, tag, &["a", "b", "twins", "exact"]);
    poison(&url, "twins");
    poison_exact(&url, "exact");
    run(&url, "b");
    if !decoy.is_empty() {
        let b = tenant_id(&url, "b");
        client(&url)
            .batch_execute(&decoy.replace("{b}", &b.to_string()))
            .expect("decoy rows");
    }
    let before = digests(&url, "b");
    run(&url, "a");
    assert_eq!(
        digests(&url, "b"),
        before,
        "tenant a's {what} run changed tenant b's rows"
    );
    for poisoned in ["twins", "exact"] {
        assert!(
            check_twins(&url, poisoned) > 0,
            "the poison tenant {poisoned} was given no twins"
        );
    }
}

#[test]
fn the_auth_store_is_per_tenant() {
    beside_another_tenant(
        "tenant auth store",
        "tauth",
        // Far-future expiry and other content, so a read that crosses tenants finds a row that is
        // live and wrong.
        "UPDATE smart_oauth SET json = '{\"decoy\":true}', exp = 9000000000000000000 \
         WHERE instance_id = {b}",
        |url, tenant| auth::exercise(&open(url, tenant)),
    );
}

#[test]
fn the_job_store_is_per_tenant() {
    let tag = format!("tjobs{}-{}", std::process::id(), unique());
    beside_another_tenant(
        "tenant job store",
        "tjobs",
        // Every job of b waits to be claimed, so a claim that reaches across tenants takes them.
        "UPDATE jobs SET status = 'accepted', lease_owner = NULL, lease_until = NULL, \
         result = NULL, error = NULL, completed_at = NULL WHERE instance_id = {b}",
        |url, tenant| jobs::exercise(&open(url, tenant), &tag),
    );
}

#[test]
fn competing_job_claimers_of_one_tenant_leave_another_tenants_jobs_alone() {
    let tag = format!("trace{}-{}", std::process::id(), unique());
    beside_another_tenant(
        "tenant job claimers",
        "tclaim",
        "UPDATE jobs SET status = 'accepted', lease_owner = NULL, lease_until = NULL, \
         result = NULL, error = NULL, completed_at = NULL WHERE instance_id = {b}",
        |url, tenant| {
            let open_one = {
                let (url, tenant) = (url.to_string(), tenant.to_string());
                move || -> Arc<dyn JobStore> { Arc::new(open(&url, &tenant)) }
            };
            jobs::exercise_competing_claimers(&open_one, &tag);
        },
    );
}

#[test]
fn the_governance_store_and_its_chain_are_per_tenant() {
    let tag = format!("tgov{}", unique());
    beside_another_tenant("tenant governance store", "tgov", "", |url, tenant| {
        let store = open(url, tenant);
        governance_checks::exercise(&store, &tag);
        governance_checks::exercise_members(&store, &tag);
        // Each tenant has a chain of its own: the first row of this tenant's chain starts at
        // the start, however many rows another tenant has already chained.
        governance_checks::exercise_chain(&store, "tenant");
    });
}

#[test]
fn tenants_extend_their_audit_chains_at_the_same_time() {
    let Some(admin) = test_url("tenant concurrent chains") else {
        return;
    };
    let url = shared(&admin, "tchains", &["a", "b"]);
    let handles: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|tenant| {
            let url = url.clone();
            std::thread::spawn(move || {
                let open_one = {
                    let url = url.clone();
                    move || -> Arc<dyn GovernanceStore> { Arc::new(open(&url, tenant)) }
                };
                governance_checks::exercise_concurrent_chain(&open_one, "tenant");
            })
        })
        .collect();
    for handle in handles {
        handle
            .join()
            .expect("a tenant's chain was broken by the other");
    }
}

#[test]
fn the_conformance_store_is_per_tenant() {
    let tag = format!("tconf{}", std::process::id());
    beside_another_tenant("tenant conformance store", "tconf", "", |url, tenant| {
        let store = open(url, tenant);
        // A tag per suite, as in the classic tests: the suites assert on every row they can see
        // under their tag, so two of them must not share one.
        conformance::exercise_paged_reads(&store, &format!("{tag}paged"));
        conformance::exercise_install_in_pieces(&store, &format!("{tag}pieces"));
        let bulk = format!("{tag}bulk");
        conformance::exercise_package_install(&store, &bulk);
        conformance::exercise_backfill_batches(&store, &bulk);
        let clashes = format!("{tag}clash");
        conformance::exercise_parameter_clashes(&store, &clashes);
        conformance::exercise_terminology_changes(&store, &clashes);
    });
}
