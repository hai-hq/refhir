## What and why

<!-- What changes, and why. Link the issue it closes: "Closes #123". -->

## How it was tested

<!-- The tests you added or ran. For behavior that tests cannot show, how you checked it. -->

## Checklist

- [ ] `cargo fmt --all` and `cargo test --workspace` pass
- [ ] I added or updated tests
- [ ] I updated the docs and `refhir.toml.example` if a flag, setting, endpoint or behavior changed
- [ ] I added a line under *Unreleased* in `CHANGELOG.md`
- [ ] A schema change is a new numbered migration in **both** SQLite and Postgres, and no shipped migration was edited
- [ ] No secrets, tokens or real patient data are included
