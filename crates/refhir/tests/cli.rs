//! Command-line flags, through the real binary: the ones that print something and exit, and the
//! rules about which flags may be combined.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;
use common::{exits, free_port, serves, start, Scratch, UNUSABLE_POSTGRES};

fn refhir(flag: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_refhir"))
        .arg(flag)
        // Never pick up a config file or database from the machine running the tests.
        .env_remove("REFHIR_CONFIG")
        .env_remove("REFHIR_POSTGRES_URL")
        .current_dir(std::env::temp_dir())
        .output()
        .expect("run refhir")
}

#[test]
fn version_flags_print_the_crate_version_and_exit() {
    let expected = format!("refhir {}", env!("CARGO_PKG_VERSION"));
    for flag in ["--version", "-V"] {
        let out = refhir(flag);
        assert!(out.status.success(), "{flag} failed: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected);
    }
}

#[test]
fn help_mentions_the_version_flag() {
    let out = refhir("--help");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--version"));
}

const ONE_DATABASE: &str = "pass only one of --sqlite, --memory, or --postgres";

fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut all = Vec::new();
    for at in 0..items.len() {
        let mut rest = items.to_vec();
        let head = rest.remove(at);
        for mut tail in permutations(&rest) {
            tail.insert(0, head.clone());
            all.push(tail);
        }
    }
    all
}

/// The flags used to be applied in a fixed order (`--sqlite`, then `--memory`, then `--postgres`),
/// each clearing the others, so one of them won whatever order the command line gave: `--memory`
/// beat `--sqlite`, and `--postgres` beat both. A SQLite file that was asked for was left alone.
#[test]
fn naming_more_than_one_database_is_refused_in_every_order() {
    let dir = Scratch::new("cli-conflict");
    let file = dir.join("named.sqlite");
    let file_arg = file.to_str().unwrap();
    let flags: [Vec<&str>; 3] = [
        vec!["--sqlite", file_arg],
        vec!["--memory"],
        vec!["--postgres", UNUSABLE_POSTGRES],
    ];

    // Every ordered pair, and every ordering of all three.
    let mut orders = permutations(&[0, 1, 2]);
    for first in 0..3 {
        for second in 0..3 {
            if first != second {
                orders.push(vec![first, second]);
            }
        }
    }
    assert_eq!(orders.len(), 12);
    let mut lines: Vec<Vec<&str>> = orders
        .iter()
        .map(|order| order.iter().flat_map(|at| flags[*at].clone()).collect())
        .collect();
    // The same one twice, and other flags in between.
    lines.push(vec!["--sqlite", file_arg, "--sqlite", file_arg]);
    lines.push(vec![
        "--postgres",
        UNUSABLE_POSTGRES,
        "--postgres",
        UNUSABLE_POSTGRES,
    ]);
    lines.push(vec![
        "--memory",
        "--bind",
        "127.0.0.1:0",
        "--sqlite",
        file_arg,
    ]);
    for mut args in lines {
        args.extend(["--bind", "127.0.0.1:0"]);
        let output = exits(&dir, &args);
        let said = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {said}");
        assert_eq!(said, format!("refhir: {ONE_DATABASE}\n"), "{args:?}");
        assert!(!file.exists(), "{args:?} opened the SQLite file");
    }

    // It is the command line that is refused, before the settings are read.
    let broken = dir.join("broken.toml");
    std::fs::write(&broken, "[database\nkind = = =\n").unwrap();
    let output = exits(
        &dir,
        &[
            "--config",
            broken.to_str().unwrap(),
            "--memory",
            "--sqlite",
            file_arg,
        ],
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("refhir: {ONE_DATABASE}\n")
    );
}

/// A database flag followed by another flag has no value, and was read as the value instead: it
/// opened a SQLite file named `--memory`.
#[test]
fn a_database_flag_does_not_take_the_next_flag_as_its_value() {
    let dir = Scratch::new("cli-novalue");
    for (args, said) in [
        (
            ["--sqlite", "--memory"],
            "refhir: --sqlite needs a value, not the flag --memory\n",
        ),
        (
            ["--postgres", "--sqlite"],
            "refhir: --postgres needs a value, not the flag --sqlite\n",
        ),
    ] {
        let output = exits(&dir, &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(String::from_utf8_lossy(&output.stderr), said, "{args:?}");
    }
    assert!(!dir.join("--memory").exists(), "a file named after a flag");
}

/// "CLI flags override the file", and the environment: one flag on the command line replaces
/// whatever database the settings name, and is not a conflict with it.
#[test]
fn one_database_flag_still_overrides_the_settings() {
    let dir = Scratch::new("cli-override");
    let from_file = dir.join("from-file.sqlite");
    let from_flag = dir.join("from-flag.sqlite");
    let from_env = dir.join("from-env.sqlite");
    let write_config = |body: String| {
        let path = dir.join("settings.toml");
        std::fs::write(&path, format!("[database]\n{body}")).unwrap();
        path
    };

    // The file names a SQLite database; --memory replaces it.
    let config = write_config(format!(
        "kind = \"sqlite\"\nsqlite_path = '{}'\n",
        from_file.display()
    ));
    let config = config.to_str().unwrap();
    serves(&dir, &["--config", config, "--memory"], &[]);
    assert!(
        !from_file.exists(),
        "--memory did not replace the file's database"
    );

    // ...and so does Postgres, which here is refused at once, and not opened.
    let output = exits(
        &dir,
        &[
            "--config",
            config,
            "--postgres",
            UNUSABLE_POSTGRES,
            "--bind",
            "127.0.0.1:0",
        ],
    );
    let said = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{said}");
    assert!(!said.contains("pass only one"), "{said}");
    assert!(
        !from_file.exists(),
        "--postgres did not replace the file's database"
    );

    // The file names a database in memory; --sqlite replaces it.
    let config = write_config("kind = \"memory\"\n".to_string());
    serves(
        &dir,
        &[
            "--config",
            config.to_str().unwrap(),
            "--sqlite",
            from_flag.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        from_flag.exists(),
        "--sqlite did not replace the file's database"
    );

    // The environment names a Postgres that cannot be used; --sqlite and --memory replace it.
    let postgres = [("REFHIR_POSTGRES_URL", UNUSABLE_POSTGRES)];
    serves(&dir, &["--sqlite", from_env.to_str().unwrap()], &postgres);
    assert!(
        from_env.exists(),
        "--sqlite did not replace the environment's database"
    );
    serves(&dir, &["--memory"], &postgres);
}

/// The setting reaches the server the binary starts: with a page of 2, a search that names no
/// `_count` returns two of three patients and a `next` link, and one that names it gets what it
/// asked for.
#[test]
fn the_default_page_size_setting_is_the_page_of_a_search_that_names_no_count() {
    struct Stop(std::process::Child);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = Scratch::new("cli-page");
    let bind = format!("127.0.0.1:{}", free_port());
    let package = concat!(env!("CARGO_MANIFEST_DIR"), "/../fhir/fixtures/minir4");
    let _server = Stop(start(
        &dir,
        &["--memory", "--bind", &bind, "--package", package],
        &[("REFHIR_DEFAULT_PAGE_SIZE", "2")],
        Stdio::null(),
        Stdio::null(),
    ));
    let base = format!("http://{bind}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while ureq::get(&format!("{base}/readyz")).call().is_err() {
        assert!(Instant::now() < deadline, "the server was not ready in 30s");
        std::thread::sleep(Duration::from_millis(100));
    }
    for family in ["A", "B", "C"] {
        ureq::post(&format!("{base}/Patient"))
            .set("content-type", "application/fhir+json")
            .send_string(&format!(
                r#"{{"resourceType":"Patient","name":[{{"family":"{family}"}}]}}"#
            ))
            .unwrap();
    }
    let search = |query: &str| -> serde_json::Value {
        ureq::get(&format!("{base}/Patient{query}"))
            .call()
            .unwrap()
            .into_string()
            .map(|body| serde_json::from_str(&body).unwrap())
            .unwrap()
    };
    let page = search("");
    assert_eq!(page["entry"].as_array().unwrap().len(), 2, "{page}");
    assert_eq!(page["total"], 3);
    assert!(page["link"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["relation"] == "next"));
    assert_eq!(search("?_count=3")["entry"].as_array().unwrap().len(), 3);
}
