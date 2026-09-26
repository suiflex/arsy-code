# ARSY CODE

A local, auditable, model-independent software-engineering agent harness.

ARSY runs on your machine and keeps the authority there: a provider can propose
an effect, but only a resolved capability and an explicit policy decision let it
happen, and every mutation is recorded against the session, turn, and workspace
revision that produced it.

## Install

```console
npm install --global @suiflex/arsy-code
arsy --version
```

## What installing does

Postinstall downloads the same-version GitHub Release archive for your
platform, verifies its published SHA-256, and unpacks `arsy`, `fluxguard`, and
`probelm` beside each other. It fails loudly rather than leaving behind a
launcher that cannot run.

| Platform | Architectures |
|---|---|
| macOS 13+ | x86-64, Apple silicon |
| Linux (glibc) | x86-64, ARM64 |
| Windows 10 22H2+ | x86-64, ARM64 |

An unsupported platform refuses the install with a diagnostic naming it.

## Other ways to install

```console
brew install suiflex/tap/arsy-code
```

```console
scoop bucket add suiflex https://github.com/suiflex/scoop-bucket
scoop install arsy-code
```

Every release also publishes the binaries directly, with a `SHA256SUMS` signed
through keyless Sigstore for anyone who wants provenance rather than a checksum
alone. [Install and verify](https://github.com/suiflex/arsy-code/blob/main/docs/34-distribution.md#install-and-verify)
gives the exact commands.

## Links

- Homepage — https://arsy.suiflex.dev
- Source and issues — https://github.com/suiflex/arsy-code
- Distribution and verification — [docs/34-distribution.md](https://github.com/suiflex/arsy-code/blob/main/docs/34-distribution.md)

MIT licensed.
