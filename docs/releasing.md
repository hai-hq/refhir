# Releasing

For maintainers. A release is a version tag. Pushing `vX.Y.Z` runs
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which builds the binaries and
the container image and publishes a GitHub release whose notes come from
[`CHANGELOG.md`](../CHANGELOG.md).

## What a release contains

| Artifact | Where |
|----------|-------|
| `refhir-X.Y.Z-linux-amd64.tar.gz`, `-linux-arm64.tar.gz`, `-macos-arm64.tar.gz` (the binary, `LICENSE`, `NOTICE`, `README.md`, `CHANGELOG.md`) | the GitHub release |
| `SHA256SUMS` | the GitHub release. Verify with `shasum -a 256 -c SHA256SUMS` |
| `ghcr.io/hai-hq/refhir:X.Y.Z` and `:X.Y` (a multi-architecture image for amd64 and arm64), plus `:X` from 1.0 and `:latest` for stable releases | GitHub Container Registry |

There are no Windows or Intel macOS binaries; build those from source. A pre-release tag such as
`v0.2.0-rc.1` is marked as a pre-release on GitHub and never moves `:latest`.

The arm64 Linux builds use GitHub's hosted arm64 runners, which are free for public repositories.
In a private repository the `linux-arm64` jobs wait for a runner that does not exist.

## Versioning

[Semantic Versioning](https://semver.org). The server has one version: `[workspace.package]
version` in the root `Cargo.toml`, which every crate inherits and `refhir --version` prints. Before
1.0, a minor release may change configuration, the HTTP API or the `/sync` protocol, and the
changelog says so. The clients are versioned alongside it:

- `clients/ts`: the `version` in `package.json` (`@hai-hq/refhir` on npm).
- `clients/go`: a Go module in a subdirectory, so its tag carries the path: `clients/go/vX.Y.Z`.

## Procedure

1. **Check `main` is green** on the commit you want to release.
2. **Dry run the workflow.** *Actions → Release → Run workflow*, on a branch. It builds every
   binary and the image and checks the version and the notes; it publishes nothing.
3. **Prepare the release in a pull request:**
   - Set the new version in the root `Cargo.toml`, then run `cargo check` so `Cargo.lock`
     follows. CI builds with `--locked` and fails on a stale lockfile.
   - In `clients/ts`, run `npm version X.Y.Z --no-git-tag-version`.
   - In `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add a new empty
     `## [Unreleased]` above it, and update the link definitions at the bottom.
   - Preview the notes: `scripts/release-notes.sh X.Y.Z`.
4. **Merge it, then tag the merge commit on `main`:**

   ```bash
   git switch main && git pull
   git tag -a vX.Y.Z -m "ReFHIR X.Y.Z"
   git push origin vX.Y.Z
   ```

   The workflow refuses a tag that does not match the version in `Cargo.toml`, or a version with
   no changelog section, before it builds anything.
5. **Check the result.** The release page lists three archives and `SHA256SUMS`;
   `docker run --rm ghcr.io/hai-hq/refhir:X.Y.Z --version` prints `refhir X.Y.Z`.
6. **Tag the Go client** so `go get` can resolve it, and publish the npm package:

   ```bash
   git tag clients/go/vX.Y.Z && git push origin clients/go/vX.Y.Z
   (cd clients/ts && npm publish --access public)
   ```

The first time, also open the new container package on GitHub (*Packages → refhir → Package
settings*) and make it public, and connect it to the repository so the README shows on its page.
GitHub creates packages as private when the repository is private.

## A failed release

If a build job fails, nothing is published: the GitHub release is created last, and only when
every binary and the image have succeeded. Fix the problem on `main`, delete the tag
(`git push --delete origin vX.Y.Z && git tag -d vX.Y.Z`), and tag again. If the release was
published and is wrong, do not reuse the version: fix forward with a patch release.
