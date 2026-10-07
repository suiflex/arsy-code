# Native configuration

## Format and discovery

ARSY native configuration is UTF-8 JSON named `arsy.json`. `schema_version` is optional and, when written, must be `1`: a settings file someone just created is `{}`. An optional `"$schema"` key points an editor at [`schemas/arsy.schema.json`](../schemas/arsy.schema.json) for completion and inline errors; ARSY ignores it.

How a file is judged, strictest first:

- **The file stops loading** (`ARSY-CFG-1000`, fatal for every command that reads configuration) when it is not valid JSON, when a value has the wrong type, is out of range, or is not one of the listed values, or when it has an unknown key at the top level or under `provider`, `model`, `credentials`, `telemetry`, `lsp`, `mcp`, `remote`, `project`, `policy`, `compat`, `theme` (a colour), `storage`, `hook`, or `skill`.
- **The value is dropped with a warning** (`ARSY-CFG-1002`) when it is an unknown key under `ui` or `execution` — with a "did you mean" when one is close — when it sits in a reserved section (`context`, `git`, `sandbox`), or when a workspace file sets a key only a trusted layer may set. These are printed once when the interactive session starts, and by `arsy config validate`.

See [Validating a file](#validating-a-file).

The user layer lives in `~/.arsy/`, the directory ARSY and ARSY CODE share for
settings, credentials, and the rest of the ecosystem's global state. It is one
path on every platform, because two separate products have to agree on it. The
directory can be replaced with `ARSY_CONFIG_HOME`, which points a run at a
throwaway configuration without editing the operator's own file.

Files a person edits sit at the top of `~/.arsy/` — `arsy.json` and
`guard.json`. Everything ARSY writes on its own goes one level down:
`secrets/` (owner-only, `0700`) holds the credential catalog and file
credentials, `cache/` holds what is rebuilt on demand, and `state/` holds the
terminal's remembered model, effort, and theme. A workspace follows the same
rule: `.arsy/arsy.json`, `.arsy/guard.json`, `.arsy/AGENTS.md`, and
`.arsy/plugins/` are the project's to commit, and `.arsy/state/` — sessions,
artifacts, the repository map, subagent views, eval trees — ignores itself.
See [Where ARSY keeps files](#where-arsy-keeps-files) and
[ADR-0015](ADR/0015-arsy-directory-layout.md).

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

This is every key the loader in this build reads, grouped by section. The
same tree is published as [`schemas/arsy.schema.json`](../schemas/arsy.schema.json),
and `crates/arsy-kernel/tests/config_schema.rs` fails when the two disagree.

**Authority:**
- **trusted** keys are applied only from the enterprise layer, the user layer, or `--config`. A workspace or nested file that sets one is reported and ignored.
- **any** keys apply from every layer.

`<id>` and `<name>` are names you choose.

### Top level

| Key | Type | Default | Merge | Authority |
|---|---|---|---|---|
| `$schema` | string, ignored | none | — | any |
| `schema_version` | integer, exactly `1` | optional; absent means `1` | — | any |

### `provider` and `model`

| Key | Type | Default | Merge | Authority |
|---|---|---|---|---|
| `provider.default` | endpoint id, or `"auto"` | `"auto"` (unset) | replace | any |
| `provider.allowed` | array of endpoint ids, no repeats | all configured | intersection | any |
| `provider.endpoint.<id>.kind` | `"anthropic"`, `"openai"`, `"openai_responses"`, `"google_code_assist"`, or `"replay"` | required the first time `<id>` is declared | replace | trusted |
| `provider.endpoint.<id>.base_url` | `http(s)://` API root; a file path for `replay` | the dialect's own API | replace | trusted |
| `provider.endpoint.<id>.credential` | secret handle (`secret://file/...`); a raw key is refused | none | replace | trusted |
| `provider.endpoint.<id>.api_key_env` | environment variable name | none | replace | trusted |
| `provider.endpoint.<id>.model` | string | none | replace | trusted |
| `provider.endpoint.<id>.models` | array of non-empty strings | `[]` | replace | trusted |
| `provider.endpoint.<id>.max_output_tokens` | positive integer, optional endpoint-wide cap | unset; request fallback `8192` | replace | trusted |
| `provider.endpoint.<id>.context_windows.<model>` | positive integer, total input and output tokens | unknown | replace | trusted |
| `provider.endpoint.<id>.input_limits.<model>` | positive integer, input-only tokens for the exact model ID | provider metadata, or unknown | replace | trusted |
| `provider.endpoint.<id>.output_limits.<model>` | positive integer, requested response cap for the exact model ID | provider metadata, or unset | replace | trusted |
| `provider.endpoint.<id>.sanitize_tool_names` | boolean | `false` | replace | trusted |
| `provider.endpoint.<id>.efforts.<model>` | array of `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; `[]` for none; `"*"` key for every other model | provider metadata or built-in table | replace | trusted |
| `provider.endpoint.<id>.pricing.<model>.input_micros_per_million` | non-negative integer, required with the next | none | replace | trusted |
| `provider.endpoint.<id>.pricing.<model>.output_micros_per_million` | non-negative integer, required with the previous | none | replace | trusted |
| `provider.endpoint.<id>.oauth.authorize_url` | URL, required in `oauth` | none | replace | trusted |
| `provider.endpoint.<id>.oauth.token_url` | URL, required in `oauth` | none | replace | trusted |
| `provider.endpoint.<id>.oauth.client_id` | string, required in `oauth` | none | replace | trusted |
| `provider.endpoint.<id>.oauth.device_authorization_url` | URL | none | replace | trusted |
| `provider.endpoint.<id>.oauth.client_secret` | string | none | replace | trusted |
| `provider.endpoint.<id>.oauth.scopes` | array of strings | `[]` | replace | trusted |
| `provider.endpoint.<id>.oauth.redirect_uri` | loopback URL with a port | free port on `/callback` | replace | trusted |
| `provider.endpoint.<id>.oauth.authorize_params` | object of string values | `{}` | replace | trusted |
| `model.default` | string | none | replace | any |
| `model.effort` | `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"` or `"max"` | none (no effort sent) | replace | any |
| `model.allowed` | array of model ids | all | intersection | any |
| `credentials.store` | `"file"` (`"os"` is refused with the reason) | `"file"` | replace | any |

### `execution`

| Key | Type | Default | Merge | Authority |
|---|---|---|---|---|
| `execution.max_parallel` | integer, 1 to 16 | `4` | min | any |
| `execution.max_tool_rounds` | integer, 1 to 200 | `100` | min | any |
| `execution.allow_commands` | array of command prefixes | `[]` | union | trusted |
| `execution.additional_directories` | array of absolute or `~/` directory paths; a missing one is reported | `[]` | union | trusted |

### `ui` and `theme`

| Key | Type | Default | Merge | Authority |
|---|---|---|---|---|
| `ui.style` | `"modern"` or `"classic"` | `"modern"` | replace | any |
| `ui.tool_output` | `"collapsed"`, `"preview"`, or `"expanded"` | `"preview"` | replace | any |
| `ui.session_title` | `"model"`, `"prompt"`, or `"off"` | `"model"` | replace | any |
| `ui.mcp_log` | `"hidden"`, `"summary"`, or `"full"` | `"summary"` | replace | any |
| `theme.base` | `"dark"`, `"vivid"`, `"dracula"`, `"nord"`, `"ocean"`, `"sunset"`, or `"mono"` | `"dark"` | replace | any |
| `theme.<role>` | `#rrggbb` colour; anything else stops the file loading | the base theme's | replace | any |

### Integrations

| Key | Type | Default | Merge | Authority |
|---|---|---|---|---|
| `mcp.server.<name>.transport` | `"stdio"` or `"http"` | required for a new server | replace | any |
| `mcp.server.<name>.command` | string; required for `stdio` | none | replace | any |
| `mcp.server.<name>.args` | array of strings | `[]` | replace | any |
| `mcp.server.<name>.url` | URL; required for `http` | none | replace | any |
| `mcp.server.<name>.enabled` | boolean | `true` | replace | any |
| `mcp.server.<name>.timeout_ms` | positive integer | `30000` | replace | any |
| `mcp.server.<name>.max_body_bytes` | positive integer | `1048576` | replace | any |
| `lsp.server.<name>.command` | array of strings, program first; required | none | replace | trusted |
| `lsp.server.<name>.extensions` | array of file extensions | `[]` | replace | trusted |
| `remote.target.<name>.kind` | `"ssh"` or `"container"` | required | replace | trusted |
| `remote.target.<name>.host`, `.user`, `.identity`, `.container` | string | none | replace | trusted |
| `remote.target.<name>.port` | integer | none | replace | trusted |
| `remote.target.<name>.engine` | `"docker"` or `"podman"` | none | replace | trusted |
| `compat.claude.enabled` | boolean | `true` | `false` sticks | any |
| `compat.codex.enabled` | boolean | `true` | `false` sticks | any |
| `compat.omp.enabled` | boolean | `true` | `false` sticks | any |
| `hook.disabled.<declaration>` | boolean | `false` | replace | any |
| `skill.disabled."<ecosystem>/<name>"` | boolean | `false` | replace | any |

An `mcp.server.<name>` entry that sets only `enabled`, `timeout_ms`, or
`max_body_bytes` amends a server a lower layer declared rather than replacing
it; one that names a server nothing declares is reported and ignored.

### Policy, trust, telemetry, storage

| Key | Type | Default | Merge | Authority |
|---|---|---|---|---|
| `policy.default_effect` | `"deny"`, `"ask"`, or `"allow"` | `"ask"` | strictest | any |
| `policy.rules[]` | array of rules, below | `[]` | rules | any |
| `project."<path>".trust_level` | `"trusted"` or `"untrusted"` | none | replace | trusted |
| `telemetry.sample_every` | positive integer | `1` | replace | any |
| `telemetry.capacity` | positive integer | `256` | replace | any |
| `telemetry.enabled` | boolean | `false` | replace | trusted |
| `telemetry.endpoint` | HTTPS URL | none | replace | trusted |
| `telemetry.include_content` | boolean | `false` | every layer must agree | trusted |
| `storage.state_gitignore` | boolean | `true` | replace | any |
| `storage.artifact_retention_days` | integer, 1 to 3650 | `7` | replace | any |

A policy rule is an object with:
- **required:** `id` (unique within the file), `effect` (`allow`/`ask`/`deny`), `action`, and `resource`;
- **optional:** `actor` (`"*"`/`"any"` by default, `"system"`, or `"user:<name>"`), `expires_at_ms`, `delegation_depth`, and `minimum_assurance` (`none`, `process`, `filesystem`, `full`).

`action` is one of:
- `fs.read`, `fs.write`, `fs.delete`
- `process.exec`, `process.signal`
- `network.connect`
- `git.read`, `git.write`
- `credential.use`
- `browser.control`
- `debug.launch`, `debug.attach`
- `remote.exec`
- `system.modify`
- `plugin.invoke`, `mcp.invoke`

`resource` is `<scheme>:<glob>`, for example `fs:src/**`.

### Reserved, no effect in this build

The design keeps these names, and the loader accepts them so a file written for
a later build still loads. Setting one changes nothing:

- `context.*`, `git.*`, and `sandbox.*`: whole sections, reported as reserved;
- `provider.residency`, `provider.credential`, `storage.data_dir`, and `storage.durability`: individual keys, accepted silently.

Restriction order for `policy.default_effect` is `allow < ask < deny`. For boolean `intersection`, every authoritative layer must permit `true`; an absent layer does not veto. Empty allowlists deny the corresponding capability.

## Provider endpoints

When a native turn selects a model, ARSY reads its limit from provider model
metadata if the endpoint has no limit for that model. It accepts numeric
`context_window`, `max_input_tokens`, or `inputTokenLimit` values; Antigravity
also reports a total window as `maxTokens`. Output limits are read per model
from `max_output_tokens`, `maxOutputTokens`, or `outputTokenLimit` metadata;
Antigravity uses `maxOutputTokens`. ARSY requests the model's full reported
output limit by default. An explicit endpoint-wide `max_output_tokens` caps
that request, and exact `input_limits.<model>` and `output_limits.<model>`
settings take precedence over discovery. Without a per-model output limit, the
request uses the configured endpoint value or 8,192 tokens when none was
configured.

ARSY reserves the requested output from a total context window. When both a
total window and an input-only limit are available, the smaller input budget
applies. Request instructions and tool schemas are reserved before fitting
conversation history. The selected model may change between turns or rounds;
each selection uses its own reported limit. A provider that exposes no numeric
limit cannot generally be inferred from its model ID. For advertised
`gemini-3.8-flash-low`, `-medium`, and `-high` variants only, ARSY uses
[Google's documented 1,048,576-token input and 65,536-token output limits for Gemini 3.8 Flash](https://ai.google.dev/gemini-api/docs/models/gemini-3.8-flash)
when Antigravity omits a numeric limit. [Google lists these effort variants](https://codelabs.developers.google.com/antigravity-cli-hands-on).
The documented output limit also applies when the operator configured an exact
context window for one of those variants.
Other missing limits are reported instead of assumed.
An explicit verified override remains available as
`"context_windows": {"qwen3-coder": 128000}`.

Each model offers its own reasoning efforts. ARSY takes them from the first
source that knows the model: `efforts.<model>` (or `efforts."*"`) on its
endpoint; a family the endpoint lists once per effort (`gemini-3.8-flash-low`,
`-medium`, `-high`), which always reasons and offers no `off`; then a built-in
table of model families (`crates/arsy-kernel/src/effort.rs`). A model none of
them knows — a gateway's router alias such as `vikey/plan` — takes no effort:
no request to it carries one, the status line shows `effort n/a`, and `/effort`
and Ctrl+T leave the chosen effort alone for the next model that takes one. A
requested level the model does not offer is clamped down to the nearest one it
does, never up. A model the provider refuses an effort for is retried once
without it and treated as taking none for the rest of the session, without
being written to the configuration.

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
        "credential": "secret://file/gateway.key",
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
`api_key_env` names, then the credential file `credential` names, then the dialect's conventional
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

Credential values are handles such as `secret://file/gateway.key`, never raw
secrets. The half after `secret://` names the store that answers, and a store
ARSY does not have is refused rather than resolved somewhere else. One exists:
`file`, a file the operator owns — `secret://file/gateway.key` beside the user
configuration, or an absolute path. A file credential must be readable by its
owner alone; a mode with any group or other bit set is refused with the `chmod`
that fixes it. `api_key_env` still takes precedence over it.

`os` — the platform keyring — was withdrawn. It cost an unlock prompt on every
turn for a debug build, whose code identity changes on every rebuild, and a
prompt that arrives while the interactive session is painting is worse than the
theft it guards against on a machine the operator already controls. The name is
still recognised, so a `secret://os/...` handle written by an older build is
refused by the store it names rather than as an unknown one: re-run `arsy auth
login <provider>` for a login or `arsy auth set <provider>` for an API key, and
the credential lands in `secret://file/<provider>.key`. Signing in again also
re-points an endpoint still configured for the keyring. What is already in the
platform keyring stays there; ARSY cannot read it to move it, and cannot delete
it either, so `arsy auth remove` on such a handle drops the catalog record and
says nothing about the keyring entry.

An endpoint names its default model with `model` and may list the others with
`models = ["a", "b"]`. One endpoint speaks to one host, and a host serves more
than one model, so the models belong to the endpoint rather than to a second
endpoint that would duplicate its URL and credential. The default always leads
the offered list, duplicates are dropped, and the order is otherwise kept. A
value that is not an array of non-empty names is refused when the file loads.

The credential catalog — the list of handles, provider names, and timestamps
that `arsy auth list` prints — is kept beside the user configuration, in a file
readable by its owner alone. It holds no secret value. `credentials.store` takes
only `file`, the one store left; `credentials.store = "os"` is refused when the
file loads, naming the keyring and saying to remove the key, because an operator
who set it deliberately is owed the reason rather than a list of one.

An MCP server's own log lines are held and shown at a turn boundary rather than
written as they arrive, because a write from a forwarding thread lands in the
middle of whatever the interactive session is painting. `ui.mcp_log` says how
much of it to show: `hidden` none, `summary` (the default) one line per server
saying how much there was, `full` every line. A server that fails to connect is
reported at every level — that is a diagnostic about ARSY, not a server's
logging.

The boundary is the turn, so a line a server writes while a turn is running is
shown when the next one starts rather than as it arrives; at most 512 unshown
lines are held, and a server that outruns that loses its oldest. A scripted
`arsy run` still writes them to stderr as they arrive, now prefixed with the
name of the server that wrote them. A record
the catalog names but nothing can open is skipped rather than failing the turn:
a handle that cannot be read has no value that could reach the output, so there
is nothing left unredacted. Duplicate rule IDs in one file, type mismatches, and invalid enum values reject that file.

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
file wins over the remembered one. A malformed colour stops the file loading;
a role name the TUI does not know is reported when the session starts
(`ARSY-UIX-1002`) and skipped. `--no-color` and `NO_COLOR` still suppress all
of it.

## Interactive style

`[ui].style` selects the transcript projection: `modern` is the mockup-oriented
default, while `classic` keeps the historical renderer and its byte-locked
golden output.

```json
{
  "ui": {
    "style": "modern",
    "tool_output": "preview",
    "mcp_log": "summary"
  }
}
```

Only `modern` and `classic` are accepted. The setting applies when the
interactive session starts; it does not alter scripted `arsy run` output.

`[ui].tool_output` sets how much of a tool call's output its card shows:

- `collapsed`: one line saying how many lines there were. A running card still shows its newest line.
- `preview` (the default): the last 10 lines.
- `expanded`: all of it.

Ctrl+O toggles the last card between expanded and its resting size, and `e` or Ctrl+O does the same on a running command. `/settings` changes the value live.

`[ui].session_title` decides how a new session gets a title once its first
turn has answered. The title is what `/resume`, `/session`, the session
footer, and `arsy session list` show first, with the id behind it.

- `model` (the default) writes the first line of the first prompt at once.
  It then asks the session's own model for a title of at most six words and
  uses that, unless the session was renamed in the meantime. This costs one
  small extra request per session. When the request fails, or the turn ran
  through an installed provider CLI rather than an endpoint, the prompt's
  line stays.
- `prompt` uses only the first line of the first prompt, cut to 60 columns.
  This makes no extra request.
- `off` leaves new sessions untitled.

`/rename <TITLE>` and `arsy session rename` replace a title at any time.

## Validating a file

Edit `arsy.json` by hand freely, then check it:

```sh
arsy config validate                    # every layer this workspace loads
arsy config validate path/to/arsy.json  # one file, judged on its own
arsy config validate --strict           # also exit 1 on a warning, for CI or a pre-commit hook
```

- **The file does not load:** exit 2 and the same `ARSY-CFG-1000` message a session would stop on.
- **It loads but something was dropped:** exit 0 and each dropped value listed with its file and key — an unknown key (with a "did you mean"), a reserved section, or a trusted key set by a workspace file. `--strict` makes this exit 1.
- **Notes on Claude, Codex, or OMP files read as lower layers:** listed separately. They never fail `--strict`, because they are fixed in those files.

The interactive session prints the same warnings once, before the first
prompt. `arsy config explain [KEY]` shows the value each key resolved to and
the layer that decided it.

`/settings`, `arsy config set`, `/provider`, `/skill`, and `/hooks` check an
edit with the loader before writing. An edit that would stop a working file
loading is refused and the file is left as it was.

For completion and inline errors in an editor, start the file with:

```json
{
  "$schema": "https://raw.githubusercontent.com/suiflex/arsy-code/main/schemas/arsy.schema.json"
}
```

## A complete example

A user `~/.arsy/arsy.json` that uses most sections. Every key is optional.

```json
{
  "$schema": "https://raw.githubusercontent.com/suiflex/arsy-code/main/schemas/arsy.schema.json",
  "schema_version": 1,
  "provider": {
    "default": "gateway",
    "endpoint": {
      "gateway": {
        "kind": "openai",
        "base_url": "https://gateway.internal/v1",
        "credential": "secret://file/gateway.key",
        "model": "qwen3-coder",
        "models": ["qwen3-coder", "glm-4.6"],
        "context_windows": { "qwen3-coder": 128000 }
      },
      "claude": {
        "kind": "anthropic",
        "api_key_env": "ANTHROPIC_API_KEY",
        "model": "claude-sonnet-5-5"
      }
    }
  },
  "execution": {
    "max_parallel": 4,
    "max_tool_rounds": 100,
    "allow_commands": ["cargo test", "git status"]
  },
  "mcp": {
    "server": {
      "docs": { "transport": "http", "url": "https://mcp.internal/docs" },
      "local": { "transport": "stdio", "command": "my-mcp", "args": ["--quiet"] }
    }
  },
  "policy": {
    "default_effect": "ask",
    "rules": [
      { "id": "read-src", "effect": "allow", "action": "fs.read", "resource": "fs:src/**" },
      { "id": "no-push", "effect": "deny", "action": "git.write", "resource": "git:*" }
    ]
  },
  "ui": { "style": "modern", "tool_output": "preview", "mcp_log": "summary" },
  "theme": { "base": "ocean", "accent": "#1e78b4" },
  "storage": { "artifact_retention_days": 14 }
}
```

A workspace `.arsy/arsy.json` is the same shape, but the keys marked
**trusted** above are ignored there with a warning. Endpoints, command
allowlists, extra directories, LSP servers, remote targets, and telemetry
export stay the operator's to set.

## Where ARSY keeps files

```
~/.arsy/                       <workspace>/.arsy/
├── arsy.json                  ├── arsy.json
├── guard.json                 ├── guard.json
├── secrets/        (0700)     ├── AGENTS.md
│   ├── credentials.json       ├── plugins/
│   └── <handle>    (0600)     └── state/              (ignores itself)
├── cache/                         ├── sessions.sqlite3
│   └── mcp-tools.json             ├── repo-map.json
└── state/                         ├── artifacts/
    └── model, effort, theme       ├── views/
                                   └── eval/
```

`state/` and `cache/` may be deleted at any time; ARSY rebuilds what it needs,
and a deleted `state/` in a workspace loses that workspace's session history
and nothing else. `secrets/` is written only through the credential commands.

A layout an earlier release left behind is moved on first use, by rename and
never over a file already at the destination: workspace state the first time
any command resolves that workspace, and each credential, cache, or remembered
choice the first time it is read. Subagent views and eval trees are git
worktrees and stay where they were; `arsy storage clean views` removes them.

Nothing here needs a hand-edited file:

- `/settings` edits the user file, and after Tab the project's;
  `arsy config set <KEY> <VALUE> [--scope user|workspace]` and
  `arsy config unset <KEY>` do the same from a shell. A write the loader
  would refuse is put back.
- `/storage` and `arsy storage` list every location with its size, and clean
  the cache, the repository map, idle subagent views, and unreferenced
  artifacts. `arsy storage reset-history --confirm <WORKSPACE NAME>` deletes
  the session store once nothing is writing to it.
- `/hooks` and `arsy hook add|remove [--scope user|workspace]` add a
  `command` hook to, or remove one from, ARSY's own `guard.json`. Claude's
  and Codex's files are only ever switched off through `hook.disabled`.

## Six-layer example

This shows the merge model the design aims at. It uses `context.*` keys, which
are reserved and have no effect in this build.

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
