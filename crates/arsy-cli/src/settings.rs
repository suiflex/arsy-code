//! Writing one registry setting to the user or the workspace `arsy.json`.
//!
//! The `/settings` dialog and `arsy config set|unset` both come through here,
//! so a value is checked against the same registry, lands in the same file,
//! and is refused the same way whichever surface the operator used.

use crate::config_load::replace_file;
use crate::{config_edit, mcp::Scope};
use std::io;
use std::path::{Path, PathBuf};

/// The object path a dotted key sits at, and the leaf that names it.
pub(crate) fn key_path(key: &str) -> Result<(Vec<&str>, &str), String> {
    let (path, leaf) = key
        .rsplit_once('.')
        .ok_or_else(|| format!("`{key}` is not a dotted key this build can write"))?;
    Ok((path.split('.').collect(), leaf))
}

fn registered(key: &str) -> Result<&'static arsy_kernel::config::Setting, String> {
    arsy_kernel::config::setting(key)
        .ok_or_else(|| format!("`{key}` is not a setting this build can write"))
}

/// Set `key` to `value` in the file `scope` names, and return that file.
pub(crate) fn set(root: &Path, scope: Scope, key: &str, value: &str) -> Result<PathBuf, String> {
    let setting = registered(key)?;
    setting.kind.check(value)?;
    let (path, leaf) = key_path(key)?;
    edit(root, scope, |config| {
        config_edit::set(config, &path, leaf, setting.kind.to_json(value))
    })
}

/// Remove `key` from the file `scope` names, so a lower layer or the built-in
/// default decides it. Only the key goes: its siblings under the same object
/// stay set.
pub(crate) fn unset(root: &Path, scope: Scope, key: &str) -> Result<PathBuf, String> {
    registered(key)?;
    let (mut path, leaf) = key_path(key)?;
    path.push(leaf);
    edit(root, scope, |config| config_edit::remove(config, &path))
}

/// Apply `change` to one layer's file. The result is checked by the loader
/// before the file is touched, so a refused edit never reaches the disk and a
/// stop part-way leaves the old file or the new one, never one ARSY refuses.
fn edit(
    root: &Path,
    scope: Scope,
    change: impl FnOnce(&str) -> Result<String, String>,
) -> Result<PathBuf, String> {
    let file = scope.path(root).map_err(|diagnostic| diagnostic.message)?;
    let original = read_existing(&file)?;
    let updated = change(original.as_deref().unwrap_or(""))?;
    validate(&file, scope, &updated)?;
    write(&file, &updated)?;
    Ok(file)
}

/// Whether the loader takes `updated` as this layer's file, judged from a
/// staged copy beside it that is removed again either way.
pub(crate) fn validate(file: &Path, scope: Scope, updated: &str) -> Result<(), String> {
    let name = file
        .file_name()
        .map_or_else(|| "arsy.json".into(), |name| name.to_string_lossy());
    let staged = file.with_file_name(format!(".{name}.check-{}", std::process::id()));
    if let Some(parent) = staged.parent() {
        // forgeguard: allow FG-SEC-007 -- the parent of arsy.json under the config home or the workspace
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{} could not be created: {error}", parent.display()))?;
    }
    // forgeguard: allow FG-SEC-007 -- a dotfile staged beside arsy.json under the config home or the workspace
    std::fs::write(&staged, updated)
        .map_err(|error| format!("{} could not be written: {error}", staged.display()))?;
    let checked = arsy_kernel::config::Config::load(&[(scope.layer(), staged.clone())]);
    let _ = std::fs::remove_file(&staged);
    checked.map(|_| ()).map_err(|error| {
        format!(
            "{} was left unchanged: the edit would not load: {}",
            file.display(),
            error.message
        )
    })
}

/// Rewrite one JSON file ARSY owns through `change`, creating it and its
/// directory when missing, and hand back what it held before (`None` when it
/// did not exist).
///
/// The write is atomic, so a reader sees the old file or the new one. The
/// guard writer uses it; settings go through `edit`, which also validates.
pub(crate) fn rewrite(
    file: &Path,
    change: impl FnOnce(&str) -> Result<String, String>,
) -> Result<Option<String>, String> {
    let original = read_existing(file)?;
    let updated = change(original.as_deref().unwrap_or(""))?;
    write(file, &updated)?;
    Ok(original)
}

fn read_existing(file: &Path) -> Result<Option<String>, String> {
    // forgeguard: allow FG-SEC-007 -- only ever arsy.json or guard.json under the config home or the workspace
    match std::fs::read_to_string(file) {
        Ok(original) => Ok(Some(original)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{} could not be read: {error}", file.display())),
    }
}

fn write(file: &Path, updated: &str) -> Result<(), String> {
    if let Some(parent) = file.parent() {
        // forgeguard: allow FG-SEC-007 -- the parent of arsy.json or guard.json under the config home or the workspace
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{} could not be created: {error}", parent.display()))?;
    }
    replace_file(file, updated.as_bytes())
        .map_err(|error| format!("{} could not be written: {error}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> serde_json::Value {
        // forgeguard: allow FG-SEC-007 -- test helper reading a file the test itself wrote in a tempdir
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_workspace_setting_lands_in_the_workspace_file() {
        let root = tempfile::tempdir().unwrap();

        let file = set(root.path(), Scope::Workspace, "ui.style", "classic").unwrap();

        assert_eq!(file, root.path().join(".arsy/arsy.json"));
        assert_eq!(read(&file)["ui"]["style"], "classic");
    }

    #[test]
    fn unset_removes_only_the_key() {
        let root = tempfile::tempdir().unwrap();
        set(root.path(), Scope::Workspace, "ui.style", "classic").unwrap();
        set(root.path(), Scope::Workspace, "ui.mcp_log", "full").unwrap();

        let file = unset(root.path(), Scope::Workspace, "ui.style").unwrap();

        let written = read(&file);
        assert!(written["ui"].get("style").is_none());
        assert_eq!(written["ui"]["mcp_log"], "full", "its sibling stays");
    }

    #[test]
    fn an_unknown_key_or_a_bad_value_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(".arsy/arsy.json");

        assert!(set(root.path(), Scope::Workspace, "ui.nonsense", "x").is_err());
        assert!(set(root.path(), Scope::Workspace, "ui.style", "loud").is_err());
        assert!(unset(root.path(), Scope::Workspace, "nope.nope").is_err());

        assert!(!file.exists());
    }

    #[test]
    fn an_edit_the_loader_refuses_never_reaches_the_file() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(".arsy/arsy.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        // Valid JSON the loader rejects: an unknown top-level key.
        let original = "{\"unheard_of\": 1}\n";
        std::fs::write(&file, original).unwrap();

        assert!(set(root.path(), Scope::Workspace, "ui.style", "classic").is_err());

        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        let left: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(left, ["arsy.json"], "the staged copy is cleaned up");
    }
}
