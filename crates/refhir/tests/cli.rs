//! Flags that print something and exit, through the real binary.

use std::process::Command;

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
