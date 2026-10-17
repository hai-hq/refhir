//! Row-level security, against a real Postgres (`REFHIR_TEST_POSTGRES_URL`).
//!
//! Every statement the stores run names its tenant, and the other tenancy tests check that. These
//! check the second line: that the database itself shows a connection the rows of the tenant it
//! serves and no others, whatever the connection asks. They connect as the application's role (not
//! a superuser, which no policy binds), say which tenant they serve, and then ask for everything,
//! with no tenant in the statement.

use postgres::Client;
use refhir_persistence::{auth, conformance, governance_checks, jobs};

use crate::tenancy_catalog_tests::{demo, install};
use crate::tenancy_tests::{
    app_url, check_twins, client, open_restricted, poison, shared, suite, tenant_id, APP_ROLE,
};
use crate::tenant::TENANT_TABLES;
use crate::tests::test_url;
use crate::{delete_tenant, ConnectOptions, PostgresPersistence};

/// A connection as the application's role that serves `tenant`, or serves nobody.
fn app_client(url: &str, tenant: Option<i32>) -> Client {
    let mut c = client(&app_url(url));
    if let Some(id) = tenant {
        c.batch_execute(&format!(
            "SELECT set_config('refhir.tenant', '{id}', false)"
        ))
        .expect("say which tenant");
    }
    c
}

fn count(c: &mut Client, sql: &str) -> i64 {
    c.query_one(sql, &[]).expect("count").get(0)
}

/// Every tenant table that the suites leave a twin in, and so holds rows of another tenant.
fn fill(url: &str, tag: &str) {
    let a = open_restricted(url, "a");
    suite(&a);
    auth::exercise(&a);
    jobs::exercise(&a, &format!("{tag}jobs"));
    governance_checks::exercise(&a, &format!("{tag}gov"));
    governance_checks::exercise_members(&a, &format!("{tag}gov"));
    governance_checks::exercise_chain(&a, "tenant");
    conformance::exercise_paged_reads(&a, &format!("{tag}paged"));
    conformance::exercise_install_in_pieces(&a, &format!("{tag}pieces"));
    conformance::exercise_package_install(&a, &format!("{tag}bulk"));
    conformance::exercise_backfill_batches(&a, &format!("{tag}bulk"));
    conformance::exercise_parameter_clashes(&a, &format!("{tag}clash"));
}

#[test]
fn every_table_with_a_tenant_column_is_bound_by_a_forced_policy() {
    let Some(admin) = test_url("row-level security coverage") else {
        return;
    };
    let url = shared(&admin, "trlscover", &["a"]);
    let mut c = client(&url);
    let rows = c
        .query(
            "SELECT c.relname::text, c.relrowsecurity, c.relforcerowsecurity, \
                    (SELECT count(*) FROM pg_policy p WHERE p.polrelid = c.oid) \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND c.relkind = 'r' AND (c.relname IN \
                   ('base_search_parameters', 'base_expansions') OR EXISTS \
                   (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid \
                    AND a.attname = 'instance_id' AND NOT a.attisdropped)) \
             ORDER BY 1",
            &[],
        )
        .unwrap();
    let mut seen = Vec::new();
    for row in &rows {
        let (name, enabled, forced, policies): (String, bool, bool, i64) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        assert!(
            enabled && forced && policies > 0,
            "{name} holds tenants' rows without a forced policy (enabled {enabled}, forced \
             {forced}, {policies} policies): a table added to the shared layout needs one in the \
             migration that adds it"
        );
        seen.push(name);
    }
    for table in TENANT_TABLES {
        assert!(
            seen.iter().any(|name| name == table),
            "{table} has no policy"
        );
    }
    assert_eq!(seen.len(), TENANT_TABLES.len() + 2, "{seen:?}");
    // The registry is what a process looks its tenant up in, before it can say which it serves.
    for open_table in ["refhir_tenants", "schema_migrations"] {
        assert!(!seen.iter().any(|name| name == open_table), "{open_table}");
    }
}

#[test]
fn a_connection_sees_only_its_tenants_rows_whatever_it_asks_for() {
    let Some(admin) = test_url("row-level security reads and writes") else {
        return;
    };
    let tag = format!("trls{}", std::process::id());
    let url = shared(&admin, "trls", &["a", "b", "twins"]);
    poison(&url, "twins");
    fill(&url, &tag);
    let (a, twins) = (tenant_id(&url, "a"), tenant_id(&url, "twins"));
    let mut admin_client = client(&url);
    let mut covered = 0;
    for table in TENANT_TABLES {
        // The poison tenant was given a twin of everything the suites wrote.
        let others = count(
            &mut admin_client,
            &format!("SELECT count(*) FROM {table} WHERE instance_id = {twins}"),
        );
        if others == 0 {
            continue;
        }
        covered += 1;
        let own = count(
            &mut admin_client,
            &format!("SELECT count(*) FROM {table} WHERE instance_id = {a}"),
        );
        let shared_rows = count(
            &mut admin_client,
            &format!("SELECT count(*) FROM {table} WHERE instance_id = 0"),
        );

        // Asking for everything returns this tenant's rows (and the shared catalog's), no more.
        let mut c = app_client(&url, Some(a));
        assert_eq!(
            count(&mut c, &format!("SELECT count(*) FROM {table}")),
            own + shared_rows,
            "{table}: a table scan reached another tenant's rows"
        );
        assert_eq!(
            count(
                &mut c,
                &format!("SELECT count(*) FROM {table} WHERE instance_id NOT IN ({a}, 0)")
            ),
            0,
            "{table}: asked for another tenant's rows by name"
        );
        // Nor can it change or remove them.
        for statement in [
            format!("UPDATE {table} SET instance_id = instance_id WHERE instance_id = {twins}"),
            format!("DELETE FROM {table} WHERE instance_id = {twins}"),
            format!("DELETE FROM {table} WHERE instance_id <> {a} AND instance_id <> 0"),
        ] {
            assert_eq!(
                c.execute(statement.as_str(), &[]).unwrap(),
                0,
                "{statement} reached another tenant's rows"
            );
        }
        // Nor write a row for it: every twin the poison tenant holds is refused when this
        // connection tries to insert it.
        let made: Vec<serde_json::Value> = admin_client
            .query(
                "SELECT row FROM twin_log WHERE tbl = $1 AND (row ->> 'instance_id')::int = $2",
                &[table, &twins],
            )
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert!(!made.is_empty());
        let insert =
            format!("INSERT INTO {table} SELECT * FROM jsonb_populate_record(NULL::{table}, $1)");
        let refused = c.execute(insert.as_str(), &[&made[0]]).unwrap_err();
        assert_eq!(
            refused.code().map(|code| code.code()),
            Some("42501"),
            "{table}: a row for another tenant was accepted ({refused})"
        );

        // A connection that has not said which tenant it serves sees nothing at all.
        let mut nobody = app_client(&url, None);
        assert_eq!(
            count(&mut nobody, &format!("SELECT count(*) FROM {table}")),
            0,
            "{table}: a connection serving no tenant saw rows"
        );
        assert!(nobody.execute(insert.as_str(), &[&made[0]]).is_err());
    }
    assert!(
        covered >= 10,
        "the suites left rows of other tenants in only {covered} tables"
    );
    assert!(check_twins(&url, "twins") > 0);
}

#[test]
fn the_shared_catalog_is_read_by_every_tenant_and_written_only_by_its_installer() {
    let Some(admin) = test_url("row-level security of the shared catalog") else {
        return;
    };
    let url = shared(&admin, "trlscat", &["a", "b"]);
    let (a, b) = (open_restricted(&url, "a"), open_restricted(&url, "b"));
    let package = demo("rls", "4.0.1");
    install(&a, &package, "sha-rls").unwrap();
    let b_id = tenant_id(&url, "b");

    // Tenant b is given the catalog that tenant a built, and the connection of tenant b reads it.
    let (installed, reads) = install(&b, &package, "sha-rls").unwrap();
    assert_eq!(reads, 0);
    let mut c = app_client(&url, Some(b_id));
    for table in [
        "packages",
        "structure_definitions",
        "terminology",
        "base_search_parameters",
        "base_expansions",
    ] {
        let sql = if table.starts_with("base_") {
            format!("SELECT count(*) FROM {table}")
        } else {
            format!("SELECT count(*) FROM {table} WHERE instance_id = 0")
        };
        assert!(count(&mut c, &sql) > 0, "{table} is not readable");
    }

    // It does not write it: not a row, not a change, not a removal.
    let package_id = installed.package_id;
    for statement in [
        format!(
            "INSERT INTO structure_definitions (instance_id, package_id, url, type_name, kind, \
             is_abstract, json) VALUES (0, {package_id}, 'http://x/y', 'T', 'resource', 0, '{{}}')"
        ),
        format!(
            "INSERT INTO base_expansions (package_id, url, json) VALUES ({package_id}, 'u', '{{}}')"
        ),
        "INSERT INTO packages (instance_id, name, version, fhir_version) \
         VALUES (0, 'sneaked', '1', '4.0.1')"
            .to_string(),
    ] {
        assert!(
            c.execute(statement.as_str(), &[]).is_err(),
            "accepted without the installer's say: {statement}"
        );
    }
    for statement in [
        "UPDATE structure_definitions SET json = '{\"changed\":true}' WHERE instance_id = 0",
        "DELETE FROM structure_definitions WHERE instance_id = 0",
        "DELETE FROM terminology WHERE instance_id = 0",
        "DELETE FROM packages WHERE instance_id = 0",
    ] {
        assert_eq!(c.execute(statement, &[]).unwrap(), 0, "{statement}");
    }
    assert_eq!(
        count(
            &mut client(&url),
            "SELECT count(*) FROM structure_definitions WHERE instance_id = 0 \
             AND json LIKE '%changed%'"
        ),
        0
    );

    // The installer says it may, for one transaction, and then can.
    let mut tx = c.transaction().unwrap();
    tx.batch_execute("SET LOCAL refhir.shared_writer = 'on'")
        .unwrap();
    assert!(
        tx.execute(
            "UPDATE structure_definitions SET json = '{\"changed\":true}' WHERE instance_id = 0",
            &[]
        )
        .unwrap()
            > 0
    );
    tx.rollback().unwrap();
    // And a tenant's own rows in the same tables are still its own to write.
    assert_eq!(
        c.execute(
            &format!(
                "INSERT INTO packages (instance_id, name, version, fhir_version) \
                 VALUES ({b_id}, 'mine', '1', '4.0.1')"
            ),
            &[]
        )
        .unwrap(),
        1
    );
}

#[test]
fn a_role_that_row_security_does_not_bind_is_told_so() {
    let Some(admin) = test_url("row-level security warning") else {
        return;
    };
    let url = shared(&admin, "trlswarn", &["a"]);
    let tenant = |url: &str| {
        PostgresPersistence::connect_with(
            url,
            &ConnectOptions {
                tenant: Some("a".into()),
                ..ConnectOptions::default()
            },
        )
        .expect("open")
    };
    assert!(
        tenant(&app_url(&url)).row_security_applies(),
        "{APP_ROLE} is bound by row-level security"
    );
    assert!(
        !tenant(&url).row_security_applies(),
        "a superuser is not bound by row-level security"
    );
    let classic = crate::tests::fresh_database(&admin, "trlsclassic");
    assert!(!PostgresPersistence::connect(&classic)
        .unwrap()
        .row_security_applies());
}

#[test]
fn deleting_a_tenant_as_the_application_role_removes_its_rows_and_only_its_rows() {
    let Some(admin) = test_url("row-level security tenant delete") else {
        return;
    };
    let url = shared(&admin, "trlsdel", &["a", "b"]);
    for tenant in ["a", "b"] {
        suite(&open_restricted(&url, tenant));
    }
    // What a tenant leaves behind that the suites clean up is nothing; give both something to keep.
    for tenant in ["a", "b"] {
        let store = open_restricted(&url, tenant);
        auth::exercise(&store);
        install(&store, &demo("keep", "4.0.1"), "sha-keep").unwrap();
    }
    let kept = crate::tenancy_tests::digests(&url, "b");
    let removed = delete_tenant(&app_url(&url), "a").unwrap();
    assert!(removed > 0, "nothing was removed");
    for table in TENANT_TABLES {
        assert_eq!(
            crate::tenancy_tests::rows_of(&url, "a", table),
            0,
            "{table}"
        );
    }
    assert_eq!(crate::tenancy_tests::digests(&url, "b"), kept);
    // The shared catalog the deleted tenant helped build is still there for the other.
    assert!(
        count(
            &mut client(&url),
            "SELECT count(*) FROM structure_definitions WHERE instance_id = 0"
        ) > 0
    );
}
