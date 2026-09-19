# Native configuration

## Format and discovery

ARSY native configuration is UTF-8 JSON named `arsy.json`. `schema_version` is optional and, when written, must be `1`: a settings file someone just created is `{}`. Unknown keys are errors, and unknown keys under `policy`, `sandbox`, `secrets`, or `telemetry` fail closed.

The user layer lives in `~/.arsy/`, the directory ARSY and ARSY CODE share for
settings, credentials, and the rest of the ecosystem's global state. It is one
path on every platform, because two separate products have to agree on it. The
directory can be replaced with `ARSY_CONFIG_HOME`, which points a run at a
throwaway configuration without editing the operator's own file.

`~/.arsy/arsy.json` is created on install and, failing that, on the first run
that reads configuration. An older ARSY's `config.toml` in the platform
configuration directory is converted into it once, at that point — never when
`ARSY_CONFIG_HOME` is set, and never over a file that already exists.

The resolver reads these six layers in authority order, then returns the effective value and a source trace for every key, which [`arsy config explain`](36-cli-tui.md) prints:

1. enterprise `arsy.json`: `/etc/arsy/` on Linux, `/Library/Application Support/ARSY/` on macOS, or `%ProgramData%\ARSY\` on Windows;
2. user `arsy.json`: `~/.arsy/` on every platform;
3. `.arsy/arsy.json` at the workspace root;
4. nested `.arsy/arsy.json` files from the workspace root toward the working directory, parent before child;
5. enabled Claude and Codex configuration, read live on every launch by `arsy-compat` and placed below every `arsy.json` layer: MCP servers, permission rules, and a fallback model (see [Claude](21-compatibility-claude.md) and [Codex](22-compatibility-codex.md)). Nothing is copied into `arsy.json`; OMP declarations remain inspection-only;
6. the current session request, including CLI flags. `--config <PATH>` supplies
   one file at this layer. It is the operator speaking for this invocation, so
   it carries their own authority — it may name an endpoint or a credential
   handle the way a user file can — and no more: every ceiling merges by
   intersection, so it can narrow the run and never widen it.

Symlinks are resolved before scope checks. A nested file applies only below its parent directory. Repository and compatibility files are untrusted content: they may express intent or narrow authority, but cannot grant capabilities, expose credentials, weaken a ceiling, or redirect user storage.

## Merge and authority

Merge strategies are `replace` (highest applicable layer), `min`, `max`, `intersection`, `append-unique`, and `rules`. `rules` merges by stable rule ID; a later deny or narrower resource wins, while a grant from repository, compatibility, or session input is ignored with a diagnostic.

Authority classes are:

- **built-in**: fixed by this schema;
- **ceiling**: enterprise may cap it; lower layers may only narrow it;
- **user**: enterprise or user may set it; repository content cannot;
- **intent**: repository/nested/compatibility/session intent is accepted within ceilings;
- **session**: the session may select a value within resolved policy.

`compat.<source>.enabled` does not merge by `replace`: a `false` from any layer switches the source off and no later layer can switch it back on, so a repository cannot re-enable a tool the operator turned off.

`none` below means the key is absent, not an empty string. Defaults are fallbacks applied only when no layer sets a key; they are not operands in a multi-layer merge.

## Version 1 key schema

| Key | Type | Default | Merge | Authority |
|---|---|---|---|---|
| `schema_version` | integer, exactly `1` | optional; absent means `1` | replace | built-in |
| `provider.default` | string or `"auto"` | `"auto"` | replace | intent |
| `provider.allowed` | array of provider IDs | all configured | intersection | ceiling |
| `provider.residency` | array of region IDs | none | intersection | ceiling |
| `provider.credential` | secret-handle string | none | replace | user |
| `provider.endpoint.<id>.kind` | `"anthropic"` or `"openai"` | required | replace | user |
| `provider.endpoint.<id>.base_url` | http/https API root | the dialect's own API | replace | user |
| `provider.endpoint.<id>.credential` | secret-handle string | none | replace | user |
| `provider.endpoint.<id>.api_key_env` | environment variable name | none | replace | user |
| `provider.endpoint.<id>.model` | string | none | replace | user |
| `provider.endpoint.<id>.max_output_tokens` | positive integer | `8192` | replace | user |
| `provider.endpoint.<id>.pricing.<model>.input_micros_per_million` | non-negative integer | none | replace | user |
| `provider.endpoint.<id>.pricing.<model>.output_micros_per_million` | non-negative integer | none | replace | user |
| `provider.endpoint.<id>.oauth.authorize_url` | HTTPS URL | none | replace | user |
| `provider.endpoint.<id>.oauth.token_url` | HTTPS URL | none | replace | user |
| `provider.endpoint.<id>.oauth.device_authorization_url` | HTTPS URL | none | replace | user |
| `provider.endpoint.<id>.oauth.client_id` | string | none | replace | user |
| `provider.endpoint.<id>.oauth.client_secret` | string | none | replace | user |
| `provider.endpoint.<id>.oauth.scopes` | array of strings | `[]` | replace | user |
| `provider.endpoint.<id>.oauth.redirect_uri` | loopback URL with a port | free port on `/callback` | replace | user |
| `provider.endpoint.<id>.oauth.authorize_params` | table of string values | `{}` | replace | user |
| `model.default` | string or `"auto"` | `"auto"` | replace | intent |
| `model.allowed` | array of model IDs | all profiled | intersection | ceiling |
| `context.max_tokens` | positive integer | `65536` | min | ceiling |
| `context.instructions` | array of workspace-relative paths | `["AGENTS.md"]` | append-unique | intent |
| `context.allow_external_files` | boolean | `false` | intersection | user |
| `policy.rules` | array of rule tables with unique `id` | `[]` | rules | ceiling |
| `policy.default_effect` | `"deny"`, `"ask"`, or `"allow"` | `"ask"` | max | user |
| `sandbox.minimum_assurance` | `"none"`, `"process"`, `"workspace"`, or `"isolated"` | `"workspace"` | max | ceiling |
| `sandbox.network.allowed_hosts` | array of host/port patterns | `[]` | intersection | ceiling |
| `sandbox.fs.writable_roots` | array of canonical root aliases | `["workspace"]` | intersection | ceiling |
| `sandbox.process.allowed_programs` | array of executable IDs | `[]` | intersection | ceiling |
| `execution.timeout_seconds` | positive integer | `300` | min | ceiling |
| `execution.max_output_bytes` | positive integer | `1048576` | min | ceiling |
| `execution.max_parallel` | positive integer | `4` | min | ceiling |
| `storage.data_dir` | absolute path | platform data directory | replace | user |
| `storage.durability` | `"fast"`, `"balanced"`, or `"strict"` | `"balanced"` | max | user |
| `lsp.server.<name>.command` | array of strings, program first | none | replace | user |
| `lsp.server.<name>.extensions` | array of file extensions | `[]` | replace | user |
| `telemetry.sample_every` | positive integer | `1` | replace | intent |
| `telemetry.capacity` | positive integer | `256` | replace | intent |
| `telemetry.enabled` | boolean | `false` | replace | user |
| `telemetry.endpoint` | HTTPS URL | none | replace | user |
| `telemetry.include_content` | boolean | `false` | intersection | ceiling |
| `compat.claude.enabled` | boolean | `true` | `false` sticks | intent |
| `compat.codex.enabled` | boolean | `true` | `false` sticks | intent |
| `compat.omp.enabled` | boolean | `true` | `false` sticks | intent |
| `git.respect_ignore` | boolean | `true` | replace | intent |
| `ui.output` | `"human"`, `"json"`, or `"ci"` | TTY-derived | replace | session |
| `ui.color` | `"auto"`, `"always"`, or `"never"` | `"auto"` | replace | session |
| `theme.base` | `"dark"`, `"vivid"`, `"dracula"`, `"nord"`, `"ocean"`, `"sunset"`, or `"mono"` | `"dark"` | replace | user |
| `theme.<role>` | `#rrggbb` colour | the base theme's | replace | user |

For boolean `intersection`, every authoritative layer must permit `true`; an absent layer does not veto. Restriction order for `policy.default_effect` is `allow < ask < deny`; durability order is `fast < balanced < strict`. Empty allowlists deny the corresponding capability unless enterprise policy explicitly defines an unconstrained set.

## Provider endpoints

An endpoint names a wire dialect and an API root, so one adapter serves the vendor's own API, a
gateway such as LiteLLM or OpenRouter, and a local runtime such as Ollama or LM Studio:

```json
{
  "provider": {
    "default": "gateway",
    "endpoint": {
      "gateway": {
        "kind": "openai",
        "base_url": "https://gateway.internal/v1",
        "credential": "secret://os/gateway",
        "model": "qwen3-coder",
        "oauth": {
          "authorize_url": "https://issuer.internal/authorize",
          "token_url": "https://issuer.internal/token",
          "device_authorization_url": "https://issuer.internal/device",
          "client_id": "arsy",
          "scopes": ["offline_access"]
        }
      }
    }
  }
}
```

The `oauth` object is optional; `arsy auth login` uses it when it is there.

`provider.endpoint.*` keys carry **user** authority and are accepted from the enterprise and user
layers only. A `base_url` decides where prompts and a credential are sent, so a repository file
that set one would make cloning a repository enough to redirect the model call; such a table is
ignored with a diagnostic that `arsy config explain` prints. The transport refuses to send a
credential over plaintext `http` unless the host is loopback, which is how a local runtime is
reached without opening a cleartext path to the internet.

A credential is looked for in the order an operator would expect to override it: the variable
`api_key_env` names, then the keyring entry `credential` names, then the dialect's conventional
variable (`ANTHROPIC_API_KEY` or `OPENAI_API_KEY`). A source that is present but blank counts as
absent. `credential` may hold either an API key or the token set `arsy auth login` writes; the two
are told apart by shape, and an expired access token is refreshed and written back before use.

### Built-in login presets

`arsy auth login <id>` also accepts an `<id>` that names no configured endpoint
but is a built-in preset — a vendor ARSY ships an OAuth client for:

| Preset | Signs in with | Endpoint it writes |
|---|---|---|
| `codex-oauth` | a ChatGPT account | `kind = "openai_responses"`, the Codex backend |
| `antigravity` | a Google account | `kind = "google_code_assist"`, Cloud Code Assist |
| `claude-oauth` | a Claude.ai account (Pro/Max) | `kind = "anthropic"`, the Messages API |

Signing in to one runs its OAuth flow, stores the token, and appends a
`[provider.endpoint.<id>]` table pointed at it, so `/model` and a turn find it
like any hand-configured endpoint. These reuse another product's client
identifier; the Antigravity and Claude Pro/Max paths in particular may
violate that product's terms of service. An endpoint you configure yourself
with its own `[oauth]` table always takes precedence over a preset of the
same name.

Credential values are handles such as `secret://os/gateway`, never raw secrets.
The half after `secret://` names the store that answers, and a store ARSY does
not have is refused rather than resolved somewhere else. Two exist: `os` is the
platform credential store, and `file` is a file the operator owns —
`secret://file/gateway.key` beside the user configuration, or an absolute path.
A file credential must be readable by its owner alone; a mode with any group or
other bit set is refused with the `chmod` that fixes it. `file` is what a
headless host, a container, or a debug build whose code identity changes on
every rebuild — and so is asked to unlock the keychain again each time — should
use. `api_key_env` still takes precedence over both.

An endpoint names its default model with `model` and may list the others with
`models = ["a", "b"]`. One endpoint speaks to one host, and a host serves more
than one model, so the models belong to the endpoint rather than to a second
endpoint that would duplicate its URL and credential. The default always leads
the offered list, duplicates are dropped, and the order is otherwise kept. A
value that is not an array of non-empty names is refused when the file loads.

`credentials.store` chooses where the credential catalog — the list of handles,
provider names, and timestamps that `arsy auth list` prints — is kept: `file`
(the default) beside the user configuration, or `os` in the platform store. The
catalog holds no secret value, so the default costs no unlock prompt to read it;
`os` keeps everything in one place for an operator who prefers that. The names
are the same two the `secret://` handles use. Switching to `file` migrates an
existing catalog out of the platform store on first read, once — except when
`ARSY_CONFIG_HOME` is set, because a run pointed at a throwaway configuration
home asked for that home and not for the operator's own catalog copied into it.
A record the catalog names but the platform store will not open is skipped
rather than failing the turn: a handle that cannot be read has no value that
could reach the output, so there is nothing left unredacted. A store that is
neither is refused when the file loads, so a typo cannot quietly send
credentials somewhere else. Path and URL keys are canonicalized and validated before merge. Duplicate rule IDs in one file, type mismatches, invalid enum values, and out-of-scope nested paths reject that file.

## Theme

`[theme]` colours the interactive TUI. `base` picks one of the built-in themes
(`dark`, `vivid`, `dracula`, `nord`, `ocean`, `sunset`, `mono` — all designed for a dark terminal); any
other key is a role whose colour it replaces, given as `#rrggbb`. The roles are
`assistant`, `dim`, `accent`, `ok`, `err`, `run`, `model`, `cwd`, `border`,
`bullet`, and `input_bg` (a background).

```json
{
  "theme": {
    "base": "ocean",
    "accent": "#1e78b4",
    "err": "#c8283f"
  }
}
```

The `/theme` command in the TUI opens a picker that repaints in each theme as
you arrow onto it, so the choice is previewed before Enter takes it; the chosen
`base` is remembered beside the configuration. An explicit `theme.base` in the
file wins over the remembered one. An unrecognized role or a malformed colour is
reported and skipped, never applied. `--no-color` and `NO_COLOR` still suppress
all of it.

## Six-layer example

Assume resolution from a workspace root to `services/payments`:

| Layer | Relevant input |
|---|---|
| enterprise | allows providers `["anthropic", "openai"]`, caps context at `80000`, denies all network except provider endpoints |
| user | selects provider `"openai"`, caps context at `64000`, stores a credential handle |
| native repository | requests provider `"anthropic"`, context `48000`, and instruction `"PROJECT.md"` |
| nested native | requests model `"claude-sonnet"`, context `32000`, and instruction `"services/payments/AGENTS.md"` |
| compatibility import | requests provider `"local"`, tries to allow arbitrary network, and contributes `"CLAUDE.md"` |
| session | requests provider `"openai"`, context `50000`, JSON output, and a one-host network grant |

The result is provider `openai` because the session selection is enterprise-allowed; model `claude-sonnet` only if its profile belongs to that provider's allowed set; context `32000` by `min`; instructions `[AGENTS.md, PROJECT.md, services/payments/AGENTS.md, CLAUDE.md]` by `append-unique`; and JSON output by session replacement. Both attempted network grants remain denied because compatibility and session input cannot widen enterprise policy. The credential remains the user's opaque handle, and the rejected provider/network values appear in the explanation trace.
