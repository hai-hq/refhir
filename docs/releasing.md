# Releasing

For maintainers. A release is a version tag. Pushing `vX.Y.Z` runs
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which builds the binaries and
the container image, publishes a GitHub release whose notes come from
[`CHANGELOG.md`](../CHANGELOG.md), tags the Go client, and **stages** the TypeScript client on npm.
npm does not let a CI token publish, so one step is left to you after the tag: approving the staged
npm version with two-factor authentication ([below](#approving-the-npm-release)).

## What a release contains

| Artifact | Where |
|----------|-------|
| `refhir-X.Y.Z-linux-amd64.tar.gz`, `-linux-arm64.tar.gz`, `-macos-arm64.tar.gz` (the binary, `LICENSE`, `NOTICE`, `README.md`, `CHANGELOG.md`) | the GitHub release |
| `SHA256SUMS` | the GitHub release. Verify with `shasum -a 256 -c SHA256SUMS` |
| `ghcr.io/hai-hq/refhir:X.Y.Z` and `:X.Y` (a multi-architecture image for amd64 and arm64), plus `:X` from 1.0 and `:latest` for stable releases | GitHub Container Registry |
| the Go module `github.com/hai-hq/refhir/clients/go` at `vX.Y.Z` (the workflow creates the tag `clients/go/vX.Y.Z`) | the Go module proxy, on first `go get` |
| `@haihq/refhir@X.Y.Z` (with npm provenance once the repository is public). The workflow stages it; it is public once you approve it | npm |

There are no Windows or Intel macOS binaries; build those from source. A pre-release tag such as
`v0.2.0-rc.1` is marked as a pre-release on GitHub, never moves `:latest`, and is published to npm
under the `next` tag so `npm install` does not pick it up.

The arm64 Linux builds use GitHub's hosted arm64 runners, which are free for public repositories.
In a private repository the `linux-arm64` jobs wait for a runner that does not exist.

## Versioning

[Semantic Versioning](https://semver.org). The server has one version: `[workspace.package]
version` in the root `Cargo.toml`, which every crate inherits and `refhir --version` prints. Before
1.0, a minor release may change configuration, the HTTP API or the `/sync` protocol, and the
changelog says so. The clients are released at the same version:

- `clients/ts`: the `version` in `package.json` and `package-lock.json` (`@haihq/refhir` on npm).
  The workflow refuses to run if either differs from the server's version.
- `clients/go`: a Go module in a subdirectory, so its tag carries the path: `clients/go/vX.Y.Z`.
  The workflow creates it at the commit the release tag points to.

## Procedure

1. **Check `main` is green** on the commit you want to release.
2. **Dry run the workflow.** *Actions → Release → Run workflow*, on a branch. It builds every
   binary and the image, checks the versions and the notes, and packs the npm package with
   `npm stage publish --dry-run`; it publishes nothing. It cannot run the jobs that only a tag
   triggers (the image tags, the GitHub release, the Go tag and the npm stage), so a change to
   those is first tested by the release itself.
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

   The workflow refuses a tag that does not match the version in `Cargo.toml`, a version with no
   changelog section, or a TypeScript client at another version, before it builds anything.
5. **Approve the npm release** ([details](#approving-the-npm-release)). The workflow's run summary
   links the steps. Until you approve it, `@haihq/refhir@X.Y.Z` is not public.
6. **Check the result.** The release page lists three archives and `SHA256SUMS`;
   `docker run --rm ghcr.io/hai-hq/refhir:X.Y.Z --version` prints `refhir X.Y.Z`;
   `go list -m github.com/hai-hq/refhir/clients/go@vX.Y.Z` finds the Go client; and once the npm
   version is approved, `npm view @haihq/refhir@X.Y.Z version` finds the TypeScript one. The
   website rebuilds by itself when `main` changes, so its version badge and its changelog page follow
   the release; look at [refhir.haihq.org](https://refhir.haihq.org) once that build has finished.

The clients are handled after the GitHub release, and only if it succeeded, in two jobs of their
own: **Tag the Go client** and **Stage the TypeScript client on npm**. Both are safe to re-run: a
tag that already points at the release commit, and a version that is already on npm or already
staged, are left alone.

## Approving the npm release

The workflow runs `npm stage publish`, which puts the version in the registry without making it
public. A maintainer with two-factor authentication on their npm account makes it public:

- on npmjs.com: open **@haihq/refhir**, then the **Staged Packages** tab, review it, and
  **Approve**; or
- in a terminal (npm 11.15 or later): `npm stage list @haihq/refhir`, then
  `npm stage approve <stage-id>`. `npm stage view <stage-id>` shows what is in it, and
  `npm stage reject <stage-id>` throws it away.

Look at what is staged before you approve it: it is the tarball built from the tagged commit, and
approving is what makes it permanent, because npm never lets a published version be replaced.

The first time a package is staged, npm also publishes a public placeholder version,
`0.0.0-stage`. The real version stays private until it is approved.

The tag the version goes under is fixed when it is staged (`latest`, or `next` for a pre-release).
To change it, reject the staged version and re-run the job.

## One-time setup

1. **npm.** The organization is `haihq` on [npmjs.com](https://www.npmjs.com) (not `hai-hq`, which
   is the GitHub organization), so the package is `@haihq/refhir`. Create a *granular access
   token* with the **Read and write (stage only)** permission for packages in the `@haihq` scope.
   Such a token can stage a version but cannot publish one: npm rejects a direct `npm publish`
   with it, so a leaked token cannot put code on npm. Store it as the repository secret
   `NPM_TOKEN`:

   ```bash
   gh secret set NPM_TOKEN --repo hai-hq/refhir
   ```

   Without the secret, a release still goes out; the npm job prints a warning and stages nothing,
   and you can re-run just that job after adding it. Staging needs npm 11.15 or later, which the
   workflow installs. npm has said it will stop accepting direct publishes from tokens that bypass
   two-factor authentication, so use the stage-only type rather than that one.
2. **Go.** Nothing to set up, but the Go module proxy can only fetch a public repository. The
   workflow asks the proxy for each new version so `go get` finds it at once, and warns if the
   proxy cannot.
3. **The container package.** After the first release, open the new package on GitHub
   (*Packages → refhir → Package settings*) and make it public, and connect it to the repository
   so the README shows on its page. GitHub creates packages as private when the repository is
   private.

## A failed release

If a build job fails, nothing is published: the GitHub release is created only when every binary
and the image have succeeded, and the clients only after that. Fix the problem on `main`, delete
the tag (`git push --delete origin vX.Y.Z && git tag -d vX.Y.Z`), and tag again. *Re-run failed
jobs* does not help here: a re-run uses the workflow file as it was at the tag, so a fix on `main`
is ignored until the tag moves. Retagging rebuilds the image and moves its tags, which is safe only
while nobody has pulled it, so do it at once.

If only a client job fails (the release is already out) for a reason outside the workflow, such as
a missing secret, fix the cause and use *Re-run failed jobs* on the workflow run. Do not delete the
tag for that. A version that is already staged cannot be
staged again: approve it, or reject it (`npm stage reject <stage-id>`) and re-run the job.

If the release was published and is wrong, do not reuse the version: npm and the Go module proxy
both refuse to replace a published version. Fix forward with a patch release.
