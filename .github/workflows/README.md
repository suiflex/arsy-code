# Workflows

All workflows here are active. Pull requests and main run the required Linux
fmt, clippy, test, compatibility, and dependency gates. macOS and Windows run
on the weekly CI schedule or manual dispatch; sandbox, fuzz, and performance
are scheduled/manual tiers. Release remains the native six-target gate. This
keeps the required check present for every pull request without paying for the
full platform matrix on each change.

Every workflow derives Rust from `workspace.package.rust-version` through the
local setup action, so release and scheduled jobs cannot drift onto a different
compiler unnoticed.

## Benchmark tiers

`benchmarks.yml` runs `arsy eval` in three cost tiers — smoke on every pull
request, comparison suites on pushes to main, and the isolated agent suites
nightly or on dispatch. The file carries the CI-minute estimate each tier was
enabled on; re-estimate before moving a suite into an earlier tier, because
a suite whose trials each cut a worktree is where the minutes go.

`arsy eval` exits nonzero when its gate blocks, and the gate blocks on a
safety regression — fewer refusals, more violations, more secret exposures
than the baseline — even when the success rate improved. No step parses the
report to decide whether the job failed.

## Release path

`release-please.yml` is the entry point, and it only prepares: dispatch it
manually, or let it run when a `release-please--*` pull request merges. Merging
that pull request tags the release, and **the tag push is what builds and
publishes** — `release.yml` runs on the tag, and `npm-publish.yml` runs on
`release.yml` completing.

release-please deliberately does not call those two itself. Doing so built
every target twice, and npm refuses a publish arriving through `workflow_call`
because the trusted publisher is registered against `npm-publish.yml` rather
than against whatever called it.

`release.yml` builds six archives (macOS, Linux, and Windows on x86_64 and
aarch64), writes a `.sha256` beside each, signs an aggregate `SHA256SUMS` with
keyless Sigstore, attaches the installers and one CycloneDX SBOM per crate,
then pushes the rendered formula and manifest to `suiflex/homebrew-tap` and
`suiflex/scoop-bucket`. A separate post-publish job downloads the final assets,
checks every digest, verifies the exact tag-workflow identity, launches the
Linux archive, and retains a smoke report. The signature ships as
`SHA256SUMS.bundle`; cosign writes no separate `.sig` alongside a bundle.
`docs/34-distribution.md` documents how to verify the result.

A `release.yml` that fails still leaves a published, asset-less GitHub Release
behind, because release-please creates the release when its pull request
merges. Fix forward and re-run rather than assuming the tag is unused.

Secrets the release path needs, all already configured:

- `RELEASE_PLEASE_TOKEN` — a PAT, not `github.token`; pull requests opened with
  the default token don't trigger workflow events, so CI would never run on the
  release PR itself.
- `TAP_PUBLISH_TOKEN` — push access to the tap and bucket repositories.

npm publishes through Trusted Publishing (OIDC), so there is no `NPM_TOKEN`.
It requires this repository and `.github/workflows/npm-publish.yml` to be
registered as a Trusted Publisher for `@suiflex/arsy-code` on npmjs.com, with
the `Release` environment. The package is a single launcher: its postinstall
downloads and verifies the matching release archive, so there are no
per-platform packages to register.

### When npm or smoke goes red

- **npm answers a publish before it serves it.** `npm-publish.yml` polls the
  registry for up to ten minutes after publishing and only then calls
  `release-smoke.yml`; the smoke npm step retries its install too, for runs
  dispatched by hand. A version that is live on npmjs.com while a smoke run
  says `ETARGET` is this delay, not a failed publish. Check with
  `npm view @suiflex/arsy-code@<version> version --prefer-online`; a plain
  `npm view` may answer from the local cache.
- **Re-running is safe.** Dispatch `npm-publish.yml` with the tag: a version
  npm already has is skipped rather than failing on npm's immutability, and
  smoke runs against it. To prove a release without touching npm at all,
  dispatch `release-smoke.yml` with the tag.
- **`latest` never moves backwards.** A version older than the current
  `latest` is published under the `backfill` dist-tag, so publishing a skipped
  release late cannot point installs at it.
- **Packaging mistakes surface on the pull request.** CI's `npm package` job
  checks that the launcher's version matches `Cargo.toml` and the
  release-please manifest, and that `npm pack` succeeds with every file it
  names.

Prefer the `release-please` dispatch over pushing a tag by hand: a hand-pushed
tag builds and publishes the same way, but without the version bump, changelog,
and contributor attribution that precede it.

## Local checks

The same checks run locally, and are what the contributing guide expects before
a push:

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --all-features --locked
    python3 fixtures/compat/check.py
