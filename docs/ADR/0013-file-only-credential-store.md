# ADR-0013: One credential store, and it is a file

- Status: Proposed
- Date: 2026-09-19

## Context

ARSY resolved credentials from two stores: `os`, the platform keyring, and
`file`, a `0600` file beside the user configuration. The keyring was the default
for anything ARSY minted itself — an OAuth preset synthesized a
`secret://os/<id>` handle, and `arsy auth set` wrote there unconditionally.

macOS binds a keychain item to the code identity of the binary that stored it.
A debug build's identity changes on every rebuild, so every rebuild produced an
unlock prompt, and the prompt arrives while the interactive session is painting
its own frame. The documentation already conceded the point by recommending
`file` for "a debug build whose code identity changes on every rebuild", and the
TUI already carried a special case to skip preloading OS handles for exactly
this reason (**V**).

The threat the keyring answers is another local account reading the credential.
A `0600` file answers the same threat on a single-operator machine, which is
what a developer workstation is. It does not answer an attacker who is already
running as the operator — but neither does the keyring, which unlocks for any
process that prompt is granted to.

## Decision

`file` is the only credential store. The `keyring` dependency is removed from
the workspace. `os` remains a recognised store id answered by `WithdrawnOsStore`,
which resolves nothing and refuses with the command that moves the credential,
so a handle written by an older build is not mistaken for a typo.
`credentials.store` accepts only `file` and names the keyring when refusing
`"os"`. Signing in again re-points an endpoint still configured for the keyring.

## Consequences

An operator with a credential in the platform keyring re-runs `arsy auth login`
or `arsy auth set` once per credential; nothing migrates automatically, because
a build without the dependency cannot read what it is being asked to move.
Entries already in the keyring are left there — ARSY can neither read nor delete
them, and `arsy auth remove` on such a handle drops the catalog record without
claiming otherwise. Preparing a task no longer opens any credential store that
can prompt, so the TUI's skip-OS-handles special case is gone. The dependency
surface shrinks by one crate and its three per-platform backends.

## Alternatives

Keeping `os` as a non-default option preserves the prompt for anyone who selects
it and keeps three platform backends compiled in for a path the documentation
already advises against. Reading the keyring once through `/usr/bin/security`
before dropping the dependency buys one automatic migration at the cost of a
shell-out to a macOS-only binary and one final prompt. Signing the debug binary
with a stable identity fixes only macOS and only for developers.

## Invariant

A credential handle names exactly one store, and a store ARSY does not have is
refused rather than resolved somewhere else. No code path opens a credential
store that can prompt the operator.
