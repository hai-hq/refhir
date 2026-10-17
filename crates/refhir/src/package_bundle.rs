//! `refhir package-bundle`: prepare the FHIR packages ahead of time, so that a server can install
//! them on its first start without reaching a registry and without the work of reading them.
//!
//! A first start otherwise downloads the release's packages and everything they depend on, reads
//! and expands them, and stores the result, which takes the most memory and time the server ever
//! uses. This does that once, and writes what it comes to into one file. A server that finds the
//! file (next to itself, or where `[conformance].package_bundle` says) stores those rows as they
//! are. A release or a container image builds the file when it is built.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use refhir_fhir::{default_r4_registry_specs, write_prepared, NpmRegistry, BUNDLE_FILE};

pub const HELP: &str = "\
refhir package-bundle [--out file] [--spec package]... [--registry location] [--cache dir]

Prepares FHIR packages ahead of time and writes them to one file, which a server installs on
its first start instead of downloading and reading them: the server needs no registry then, and
a first start takes a fraction of the memory and time.

With no --spec it prepares what the server installs by default (hl7.fhir.r4.core and
hl7.fhir.uv.sdc), and what they depend on, except packages that only hold examples. --spec adds
a package (name@version, a .tgz, or a directory) and may be repeated; naming any replaces the
default. The file is written to refhir-packages.bundle in the working directory unless --out
says otherwise. A server finds it beside the refhir binary, in ../share/refhir/ from the binary,
or at [conformance].package_bundle (REFHIR_PACKAGE_BUNDLE).

Downloading is what this command is for, so it downloads whatever the server's own setting says. Packages are downloaded from --registry (default REFHIR_PACKAGE_REGISTRY, then
https://packages.fhir.org; a mirror URL or s3://bucket/prefix also works) and kept in --cache
(default REFHIR_PACKAGE_CACHE, then .refhir/packages).

Exit codes: 0 the file was written; 2 it was not, or the arguments were wrong.";

pub fn run(args: &[String]) -> ExitCode {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match build(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("refhir package-bundle: {message}");
            ExitCode::from(2)
        }
    }
}

fn build(args: &[String]) -> Result<(), String> {
    let mut out = PathBuf::from(BUNDLE_FILE);
    let mut specs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let mut value = || -> Result<String, String> {
            index += 1;
            match args.get(index) {
                Some(value) if !value.starts_with("--") => Ok(value.clone()),
                _ => Err(format!("{flag} needs a value")),
            }
        };
        match flag {
            "--out" => out = PathBuf::from(value()?),
            "--spec" => specs.push(value()?),
            "--registry" => env::set_var("REFHIR_PACKAGE_REGISTRY", value()?),
            "--cache" => env::set_var("REFHIR_PACKAGE_CACHE", value()?),
            other => return Err(format!("unknown argument {other} (see --help)")),
        }
        index += 1;
    }
    if specs.is_empty() {
        specs = default_r4_registry_specs()
            .iter()
            .map(|spec| spec.to_string())
            .collect();
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("refhir_engine=info"))
        .init();
    // Downloading is what this command is for, whatever the server's own setting says.
    let registry = NpmRegistry::from_env()
        .map_err(|err| err.to_string())?
        .allowing_download(true);
    let names = write_prepared(&out, &specs, &registry, &registry.cache_dir)
        .map_err(|err| err.to_string())?;
    let size = std::fs::metadata(&out).map(|meta| meta.len()).unwrap_or(0);
    println!(
        "wrote {} ({:.1} MiB): {}",
        out.display(),
        size as f64 / (1024.0 * 1024.0),
        names.join(", ")
    );
    Ok(())
}
