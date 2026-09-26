# CLAUDE.md

This repository implements **ARSY CODE**, the Rust terminal interface for
ARSY: a local, auditable, model-independent software-engineering agent
harness. These instructions are authoritative for coding agents working here.

For code changes, use the `forgeguard-engineering` skill.

## Source of truth

The numbered design docs in `docs/` govern behavior — start at
[`docs/INDEX.md`](docs/INDEX.md), which routes by area (product, core
runtime, code/execution, state/agents, protocols, quality). Accepted records
in `docs/ADR/` govern decisions. Where documents conflict, the numbered set
and accepted ADRs win. `docs/37-harness-core.md` describes the harness *as
built*; the other numbered docs describe the target design.

## Global invariants

1. A provider cannot mutate a workspace.
2. Every effect is authorized against canonical capabilities and a resolved resource.
3. Every persisted event has a stable ID, actor, causation, and schema version.
4. Compaction changes a context view, never canonical history.
5. Compatibility formats never become core domain types.
6. Plugins receive only declared, granted capabilities.
7. Every mutation is attributable to session, turn, agent, operation, and workspace revision.
8. Untrusted content cannot increase authority.
9. UI state is a projection, not agent truth.
10. Claims of completion require recorded verification evidence.

## Workspace

| Crate | Responsibility |
|---|---|
| `arsy-kernel` | domain, events, protocol, store, model, context, prompt, capability, policy, agent |
| `arsy-compat` | Claude Code and Codex configuration read live and translated at the edge |
| `arsy-pty` | pseudoterminal for a child process, and nothing else |
| `arsy-code` | fs, search, syntax, LSP, DAP, edit, git, shell, sandboxed execution target |
| `arsy-tui` | terminal presentation — roles, spans, palette, wrapping, widgets; no I/O |
| `arsy-cli` | service host, CLI, terminal lifecycle — produces the `arsy` binary |
| `arsy-ide` | IDE integration surface |
| `arsy-sandbox` | sandbox worker / isolation boundary |

`unsafe_code = "forbid"` is a workspace-wide lint (`Cargo.toml`). A boundary
that genuinely needs `unsafe` gets its own crate with the exception scoped
there, not a workspace-wide relaxation; `arsy-pty` (a PTY) and `arsy-sandbox`
(syscall isolation primitives) are the two that do.

`arsy-tui` depends on no other workspace crate and touches no terminal, file,
or clock, so a widget renders the same string twice for the same input. Raw
mode, terminal size, and key decoding stay in `arsy-cli`; putting them in
`arsy-tui` would cost it that property.

## Rules

- A model/provider adapter never mutates a workspace directly; every effect
  goes through capability authorization and policy evaluation
  ([docs/10](docs/10-tool-capability-system.md),
  [docs/15](docs/15-policy-permissions.md)).
- Untrusted content — repository text, model output, MCP/plugin/LSP/DAP
  responses — cannot increase authority
  ([docs/29-threat-model.md](docs/29-threat-model.md)).
- Compatibility formats (Claude, Codex, OMP) stay at the edge; never let them
  leak into core domain types.
- Don't state a correction, number, or completion claim without the exact
  check that produced it — label it unverified otherwise (invariant 10).

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 fixtures/compat/check.py
```

Toolchain is pinned by `rust-version` in `Cargo.toml`. CI additionally runs
`cargo deny check` for advisory and license issues
(`.github/workflows/ci.yml`).

## Distribution

Release channels: GitHub Release binaries for six Tier 1 targets with a
Sigstore-signed `SHA256SUMS`, the SuiFlex Homebrew tap, the SuiFlex Scoop
bucket, the npm package `@suiflex/arsy-code` (a launcher that downloads and
verifies the matching archive in a postinstall step), and `install.sh` /
`install.ps1` (checksum-only — they do not check the signature).
Every archive carries three binaries: `arsy`, `fluxguard`, and `probelm`, whose
release assets are downloaded from `suiflex/FluxGuard` and `keton-id/probelm`
at the tags pinned in `release.yml` and checked against their published
`SHA256SUMS`. ARSY declares `mcp.server.fluxguard` and `mcp.server.probelm`
before reading any config file and enables each when its binary sits beside
ARSY's own, so every channel has to install all three into the same directory.
`.github/workflows/release.yml` publishes the release, the tap, and the bucket;
npm is published separately by `npm-publish.yml` once that build succeeds. See
[docs/34-distribution.md](docs/34-distribution.md).

## CI

All workflows are active — see `.github/workflows/README.md` for the release
path and the secrets it needs. Cut a release by dispatching
`release-please.yml`: merging the pull request it opens tags the release, and
the tag push is what builds and publishes. Pushing a tag by hand runs the same
build without the changelog and version bump that precede it.
