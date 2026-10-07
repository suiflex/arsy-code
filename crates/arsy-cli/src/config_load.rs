//! Layered configuration: reading every layer for a workspace, bootstrapping
//! the user file, and rewriting any of them atomically.

use crate::{compat_homes, Diagnostic, ARSY_CFG_1000, ARSY_PRV_1000};

fn config_home_overridden() -> bool {
    std::env::var_os(arsy_kernel::config::CONFIG_HOME_VAR).is_some_and(|home| !home.is_empty())
}

/// The operator's own configuration home, or none from a unit test that did
/// not point `ARSY_CONFIG_HOME` at a directory of its own: what a test
/// asserts must not depend on whoever runs it, and a test must never write
/// to their home.
pub(crate) fn operator_config_home() -> Option<std::path::PathBuf> {
    if cfg!(test) && !config_home_overridden() {
        return None;
    }
    arsy_kernel::config::config_home()
}

/// The operator's own `arsy.json`, under the same rule as
/// [`operator_config_home`].
pub(crate) fn operator_user_config() -> Option<std::path::PathBuf> {
    if cfg!(test) && !config_home_overridden() {
        return None;
    }
    arsy_kernel::config::user_config()
}

use arsy_kernel::config::Config;
use std::io::{self, Write};
use std::path::Path;
pub(crate) fn replace_file(path: &Path, body: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("a settings file needs a directory"))?;
    std::fs::create_dir_all(parent)?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staged = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("settings"),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(body)?;
        file.sync_all()
    })();
    if written.is_ok() {
        // The staged file is new, so it carries the umask rather than whatever
        // the destination was set to. An operator who tightened a settings
        // file must not have that undone by the next write that touches it.
        #[cfg(unix)]
        if let Ok(existing) = std::fs::metadata(path) {
            use std::os::unix::fs::PermissionsExt;
            let mode = existing.permissions().mode();
            let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode));
        }
        // Rename replaces on every target ARSY ships for, so nothing is left
        // half written even when the destination is already there.
        if let Err(error) = std::fs::rename(&staged, path) {
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        return Ok(());
    }
    let _ = std::fs::remove_file(&staged);
    written
}

/// Create `~/.arsy/arsy.json` when it is not there yet, carrying over the
/// `config.toml` an older ARSY kept in the platform configuration directory.
///
/// Run before every load rather than only at install time, because ARSY also
/// arrives through Homebrew, Scoop, and npm, and a person who deleted the file
/// should get a working one back rather than a diagnostic.
///
/// Every failure here is silent: a home directory that cannot be written is a
/// run without a user layer, which is exactly what it was before this existed.
/// Nothing is ever overwritten.
pub(crate) fn bootstrap_user_config() {
    let Some(path) = operator_user_config() else {
        return;
    };
    if path.exists() {
        return;
    }
    // What an older ARSY had, converted once. A file that no longer parses is
    // left where it is: reporting nothing beats replacing settings with an
    // empty file the operator did not ask for.
    //
    // A run pointed at a throwaway configuration home — a test, a container, a
    // second account — asked for that home and not for the operator's own
    // settings copied into it, exactly as the credential catalog treats it.
    let carried = (!config_home_overridden())
        .then(arsy_kernel::config::legacy_user_config)
        .flatten()
        .and_then(|legacy| std::fs::read_to_string(legacy).ok())
        .and_then(|raw| arsy_kernel::config::json_from_toml(&raw, &path).ok());
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let body = carried.unwrap_or_else(|| "{}".to_owned());
    // Staged beside the destination and linked into place, so a second ARSY
    // starting at the same time reads either nothing or the whole file. A
    // created-then-written file is visible while it is still empty, and an
    // empty `arsy.json` is a fatal parse error rather than a missing layer.
    //
    // `hard_link` rather than the rename `replace_file` uses: it fails when
    // the destination exists, so neither process truncates what the other
    // carried over.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staged = parent.join(format!(
        ".{}.{}.{}.tmp",
        arsy_kernel::config::CONFIG_FILE,
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let Ok(mut file) = std::fs::File::create(&staged) else {
        return;
    };
    let written = file
        .write_all(body.trim_end().as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all());
    drop(file);
    if written.is_ok() {
        let _ = std::fs::hard_link(&staged, &path);
    }
    let _ = std::fs::remove_file(&staged);
}

/// Read every configuration layer for this workspace.
///
/// An invalid file is fatal rather than skipped: continuing with a partly
/// applied policy would silently run under something the operator never wrote.
/// Every discovered configuration layer, plus the one `--config` named.
///
/// The extra file is applied last, so it wins a conflicting value — and only
/// that: `provider.allowed`, `model.allowed`, and the policy rules all merge
/// by intersection, so a session file can narrow the run but never widen it.
pub(crate) fn load_config(
    workspace: &Path,
    working: &Path,
    extra: Option<&Path>,
) -> Result<arsy_kernel::config::Config, Diagnostic> {
    bootstrap_user_config();
    let mut layers = arsy_kernel::config::layers(workspace, working);
    // A unit test reads no operator configuration: what it asserts must not
    // depend on whoever runs it. One that points `ARSY_CONFIG_HOME` at a
    // directory of its own still gets that user layer.
    if cfg!(test) {
        layers.retain(|(layer, _)| match layer {
            arsy_kernel::config::Layer::Enterprise => false,
            arsy_kernel::config::Layer::User => config_home_overridden(),
            _ => true,
        });
    }
    if let Some(path) = extra {
        // Unlike a discovered layer, a path the operator typed is theirs to
        // get right: a missing one is a mistake, not an absent optional file.
        if !path.exists() {
            return Err(Diagnostic::error(
                ARSY_CFG_1000,
                format!("--config names `{}`, which does not exist", path.display()),
                "pass the path to an existing arsy.json, or drop --config",
            ));
        }
        layers.push((arsy_kernel::config::Layer::Session, path.to_path_buf()));
    }
    let unusable = |error: arsy_kernel::config::ConfigError| {
        Diagnostic::error(
            ARSY_CFG_1000,
            format!("configuration is unusable: {error}"),
            "fix the reported file, then run `arsy config explain`",
        )
    };
    // Read twice: the first pass says which tools are switched on and whether
    // this checkout is trusted, and the second places what those tools declare
    // below every layer.
    let config = arsy_kernel::config::Config::load(&layers).map_err(unusable)?;
    let seeds = compat_seeds(workspace, &config);
    let contributes = |seed: &arsy_kernel::config::CompatSeed| {
        !(seed.mcp_servers.is_empty()
            && seed.policy_rules.is_empty()
            && seed.models.is_empty()
            && seed.notes.is_empty())
    };
    if !seeds.iter().any(contributes) {
        return Ok(config);
    }
    arsy_kernel::config::Config::load_with(&layers, &seeds).map_err(unusable)
}

/// [`load_config`] plus the `--add-dir` directories, for a command that builds
/// a tool runtime.
///
/// Only those commands take it: an added directory is authority for the
/// agent, and a command that never runs an operation has no use for one.
pub(crate) fn load_session_config(
    workspace: &Path,
    working: &Path,
    invocation: &crate::Invocation,
) -> Result<arsy_kernel::config::Config, Diagnostic> {
    let mut config = load_config(workspace, working, invocation.config.as_deref())?;
    for directory in &invocation.additional_directories {
        config.add_directory(directory).map_err(|error| {
            Diagnostic::error(
                ARSY_CFG_1000,
                format!("--add-dir names `{}`: {error}", directory.display()),
                "pass an existing directory to --add-dir",
            )
        })?;
    }
    Ok(config)
}

/// What Claude Code and Codex declare for this workspace, read live.
pub(crate) fn compat_seeds(
    workspace: &Path,
    config: &arsy_kernel::config::Config,
) -> Vec<arsy_kernel::config::CompatSeed> {
    let homes = compat_homes();
    arsy_compat::seeds(&arsy_compat::Context {
        homes: &homes,
        root: workspace,
        trusted: config.trusts(workspace),
        claude: config.compat_enabled("claude"),
        codex: config.compat_enabled("codex"),
        env: &|name| std::env::var(name).ok(),
    })
}

/// The model a turn asks for: `--model` when it was given, otherwise whatever
/// configuration resolved.
///
/// A named model is checked against `model.allowed` before it is used. The
/// ceiling is the point of the flag being an override and not an escape: an
/// operator may choose between the models policy permits, and naming one it
/// does not is refused rather than silently ignored or silently obeyed.
pub(crate) fn selected_model(
    config: &Config,
    endpoint: &arsy_kernel::config::Endpoint,
    requested: Option<&str>,
) -> Result<String, Diagnostic> {
    if let Some(model) = requested {
        if !config.model_is_allowed(model) {
            return Err(Diagnostic::error(
                ARSY_PRV_1000,
                format!("--model `{model}` is excluded by the model.allowed ceiling"),
                "run `arsy model list` for the models this configuration permits",
            ));
        }
        return Ok(model.to_owned());
    }
    // A fallback the endpoint does not list was never offered by it: the Codex
    // backend, for one, refuses a ChatGPT login any model outside its own
    // list. Where the endpoint lists its models, a fallback has to be one of
    // them — or the base of an effort family it lists — and the first listed
    // model stands in for one that is not. The endpoint's own `model` and
    // `--model` are the operator's choice and are taken as given.
    let listed = |model: &&str| {
        endpoint.models.is_empty()
            || endpoint.models.iter().any(|listed| listed == model)
            || !crate::tui::variant_levels(&endpoint.models, model).is_empty()
    };
    endpoint
        .model
        .clone()
        .or_else(|| config.model_default().filter(listed).map(str::to_owned))
        // What Claude Code or Codex is set to use, only when arsy.json is silent.
        .or_else(|| {
            config
                .compat_model(endpoint)
                .filter(listed)
                .map(str::to_owned)
        })
        .or_else(|| {
            endpoint
                .models
                .iter()
                .find(|model| config.model_is_allowed(model))
                .cloned()
        })
        .ok_or_else(|| {
            Diagnostic::error(
                ARSY_PRV_1000,
                format!("provider `{}` does not say which model to use", endpoint.id),
                "set `model` on the provider endpoint, or `model.default`, in arsy.json, or \
                 pass --model",
            )
        })
}
