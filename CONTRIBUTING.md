# Contributing to ReFHIR

Thanks for helping. ReFHIR stores and serves clinical data, so changes are reviewed for
correctness and safety first, and for speed second.

- **Found a security problem?** Do not open an issue. Follow [SECURITY.md](SECURITY.md).
- **Found a bug or want a feature?** Search the [issues](https://github.com/hai-hq/refhir/issues)
  first, then open one with the template. For anything large, open an issue *before* writing code
  so we can agree on the approach.
- **Fixing a typo or a small bug?** Send the pull request directly.

Everyone taking part is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Set up

You need Rust 1.88 (`rustup` installs it from `rust-toolchain.toml`). Go 1.23+ and Node 20+ are
needed only for the clients, and Docker only for the image and the Postgres TLS tests. The full
list is in [docs/development.md](docs/development.md).

```bash
git clone https://github.com/hai-hq/refhir.git
cd refhir
just check            # or: cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

## Before you open a pull request

CI runs these, so run them first:

| Change touches | Run |
|----------------|-----|
| Rust | `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` |
| Postgres code | the Postgres tests, with `REFHIR_TEST_POSTGRES_URL` set ([how](docs/development.md#tests-that-need-a-database)) |
| `clients/go` | `cd clients/go && go vet ./... && go test ./...` |
| `clients/ts` | `cd clients/ts && npm ci && npm run build && npm test` |
| Markdown | `python3 scripts/check-doc-links.py` |

Then:

1. **Add a test** that fails without your change. Handlers and the engine have unit tests next to
   them; behavior over HTTP and `/sync` is covered in `crates/server/src/tests.rs`, and
   multi-process behavior in `crates/refhir/tests/`.
2. **Update the docs** if a flag, endpoint, setting or behavior changes. Setting names live in
   [`refhir.toml.example`](refhir.toml.example), which should document every setting.
3. **Add a line to [CHANGELOG.md](CHANGELOG.md)** under *Unreleased*, in the section that fits
   (Added, Changed, Fixed, Security). Say what a user notices, not how you did it.
4. **Keep the change focused.** A bug fix does not need a refactor alongside it; send that
   separately.

Write commit messages as a short imperative sentence ("Add /readyz probe"), with detail in the
body when the *why* is not obvious. Squash merging is used, so the pull request title becomes the
commit message.

## Things that need extra care

- **Schema changes.** Migrations are forward-only and shipped ones are frozen: a database refuses
  to start if a migration's checksum changed. Add a new numbered migration to **both** SQLite and
  Postgres, and never edit an old one. Read [Schema migrations](docs/migrations.md#adding-a-migration).
- **The `/sync` protocol and HTTP API.** Existing clients depend on them. Add fields rather than
  change or remove them, and note any break in the changelog.
- **Authorization.** Anything that reads or writes a resource must go through scope and
  compartment enforcement, and anything that touches a record belongs in the audit trail. A new
  endpoint needs a test that a token without the scope is refused.
- **Secrets.** Never commit `refhir.toml`, tokens, keys or real patient data. Use synthetic data;
  the examples in [`fixtures/mock-fhir`](fixtures/mock-fhir/README.md) are safe to copy.

## Licensing of contributions

ReFHIR is licensed under the [Apache License 2.0](LICENSE). Unless you say otherwise, a
contribution you submit is licensed under the same terms, as section 5 of that license provides.
Only submit code you wrote, or that you have the right to submit under that license. If you copy
anything, say where it came from in the pull request and keep its notices.
