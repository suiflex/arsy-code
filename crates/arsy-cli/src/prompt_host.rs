//! Assemble the model-visible prompt from workspace context and operator settings.

use crate::{compat_homes, memory};
use std::path::{Path, PathBuf};

/// The system prompt for one turn: the harness's own instructions, then the
/// project's, discovered by walking from the workspace root to the working
/// directory.
///
/// Compilation failure is not a reason to lose the turn — a prompt over budget
/// or an unredactable secret is a degradation, not a fault — so the harness
/// instructions alone are the floor.
/// What a recalled memory may take out of the prompt.
///
/// Small on purpose. Memory competes with the task and the repository's own
/// instructions for the same window, and a workspace that remembers a page of
/// facts is one whose next turn has less room to read the code.
const MAX_RECALLED_MEMORY_BYTES: usize = 4 * 1024;

/// The plugins this workspace has installed and approved, for the prompt.
///
/// Read fresh each turn rather than cached: `arsy plugin install` and
/// `arsy plugin refresh` take effect at a turn boundary, and a listing held
/// from session start would tell the model about a set that no longer exists.
///
/// Only loadable plugins are listed. One whose manifest now asks for more than
/// was approved cannot run, and offering it would produce a refusal the model
/// could do nothing about.
#[cfg(feature = "wasm")]
fn installed_extensions(root: &Path) -> Vec<arsy_code::agent::instructions::ExtensionTool> {
    let (installed, _unreadable) = arsy_code::plugin::Registry::open(root)
        .list()
        .unwrap_or_default();
    installed
        .into_iter()
        .filter(arsy_code::plugin::Installed::loadable)
        .map(|plugin| arsy_code::agent::instructions::ExtensionTool {
            id: plugin.manifest.id,
            version: plugin.manifest.version,
            capabilities: plugin
                .manifest
                .capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
        })
        .collect()
}

pub(crate) fn system_prompt(
    root: &Path,
    config: &arsy_kernel::config::Config,
    provider: &str,
    model: &str,
    mode: arsy_code::agent::ExecutionMode,
) -> Option<String> {
    let workspace = arsy_code::resource::Workspace::open(root).ok()?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.to_path_buf());
    let instructions = instructions_for(&workspace, config, &working);
    let family = arsy_code::agent::instructions::family_for(provider, model);
    let compiled = arsy_code::agent::instructions::system_prompt(
        family,
        &instructions,
        &installed_extensions(root),
        &prompt_skills(root, config),
        config.additional_directories(),
        memory::recalled(root, MAX_RECALLED_MEMORY_BYTES).as_deref(),
        mode,
        &arsy_kernel::secret::Redactor::new(),
        arsy_kernel::prompt::MAX_PROMPT_BYTES as u32,
    )
    .ok()?;
    let rendered = arsy_code::agent::instructions::render(&compiled);
    Some(format!(
        "{rendered}\n\n{}",
        environment_note(std::env::var_os("PATH").as_deref())
    ))
}

/// Commands a model commonly reaches for, checked so it is told which exist
/// rather than guessing: `python` where only `python3` is installed cost a
/// failed round on every task that ran a test.
const PROBED_COMMANDS: &[&str] = &[
    "python3", "python", "pip3", "pip", "uv", "node", "npm", "pnpm", "bun", "cargo", "go", "git",
    "make", "rtk",
];

/// What the model should know about where its commands run: the platform,
/// the shell `bash` uses, and which common commands are on `PATH`. Found by
/// looking at `PATH`, never by running anything.
pub(crate) fn environment_note(path: Option<&std::ffi::OsStr>) -> String {
    let directories: Vec<PathBuf> = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .collect();
    let on_path = |command: &str| {
        directories.iter().any(|directory| {
            let candidate = directory.join(command);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                candidate.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            }
            #[cfg(not(unix))]
            {
                candidate.is_file() || candidate.with_extension("exe").is_file()
            }
        })
    };
    let (present, missing): (Vec<&str>, Vec<&str>) = PROBED_COMMANDS
        .iter()
        .copied()
        .partition(|command| on_path(command));
    let list = |commands: &[&str]| {
        if commands.is_empty() {
            "none of the common ones".to_owned()
        } else {
            commands.join(", ")
        }
    };
    format!(
        "<environment>\nPlatform: {} ({}). `bash` runs commands with `sh`.\n\
         On PATH: {}.\nNot on PATH: {} — use a command listed as on PATH instead of guessing.\n\
         </environment>",
        std::env::consts::OS,
        std::env::consts::ARCH,
        list(&present),
        list(&missing)
    )
}

/// Every declared skill the operator has not switched off, with the
/// description its front matter declares.
///
/// Off beats discovered: a skill in `skill.disabled` is still listed by
/// `arsy skill list`, which is where an operator goes to find out what they
/// switched off, but the model is not told about it at all.
pub(crate) fn prompt_skills(
    root: &Path,
    config: &arsy_kernel::config::Config,
) -> Vec<arsy_code::agent::instructions::Skill> {
    use arsy_code::agent::instructions::Skill;
    use arsy_code::compat::Ecosystem;
    let importer = arsy_code::compat::CompatibilityImporter::new(root);
    // The operator's own home offers skills as well as the workspace does:
    // `~/.claude/skills` and `~/.codex/skills`, the same directories the
    // ecosystems themselves read. A switch that is off, or a skill switched
    // off by name, contributes nothing — one rule for every source of skills.
    let user_skills = {
        let homes = compat_homes();
        arsy_compat::skills::all(
            &homes,
            config.compat_enabled("claude"),
            config.compat_enabled("codex"),
        )
    };
    // The workspace is listed first and claims its names, so a repository that
    // ships a skill by the same name as one of the operator's own is the one
    // that skill means: project overrides home, the precedence the ecosystems
    // use.
    let mut listed: Vec<Skill> = Vec::new();
    let mut taken = std::collections::HashSet::new();
    for ecosystem in [Ecosystem::Claude, Ecosystem::Codex, Ecosystem::Omp] {
        workspace_skills(&mut listed, &mut taken, ecosystem, config, root, &importer);
    }
    listed.extend(
        user_skills
            .into_iter()
            .filter(|skill| {
                !config
                    .skill_disabled()
                    .contains(&format!("{}/{}", skill.ecosystem, skill.name))
                    && !taken.contains(&skill.name)
            })
            .map(|skill| Skill {
                name: skill.name,
                ecosystem: skill.ecosystem.to_owned(),
                description: skill_description(root, &skill.path.display().to_string()),
                path: skill.path.display().to_string(),
            }),
    );
    listed
}

/// The workspace skills of one ecosystem, appended to `listed` under their
/// names.
///
/// `taken` collects the names as they are listed, and the workspace is listed
/// before the operator's home: a name claimed here is the one the home copy
/// then yields to.
fn workspace_skills(
    listed: &mut Vec<arsy_code::agent::instructions::Skill>,
    taken: &mut std::collections::HashSet<String>,
    ecosystem: arsy_code::compat::Ecosystem,
    config: &arsy_kernel::config::Config,
    root: &Path,
    importer: &arsy_code::compat::CompatibilityImporter,
) {
    // A source the operator switched off contributes nothing, the same as its
    // hooks and its instructions: `compat.<source>.enabled` governs everything
    // that source's files carry.
    if !config.compat_enabled(ecosystem.as_str()) {
        return;
    }
    let Ok(skills) = importer.skill_declarations(ecosystem) else {
        return;
    };
    for skill in skills {
        let Some(name) = skill["name"].as_str().map(str::to_owned) else {
            continue;
        };
        let key = format!("{}/{}", ecosystem.as_str(), name);
        if config.skill_disabled().contains(&key) || taken.contains(&name) {
            continue;
        }
        let Some(path) = skill["source"].as_str().map(str::to_owned) else {
            continue;
        };
        taken.insert(name.clone());
        listed.push(arsy_code::agent::instructions::Skill {
            name,
            ecosystem: ecosystem.as_str().to_owned(),
            description: skill_description(root, &path),
            path,
        });
    }
}

/// The one line a `SKILL.md` front matter offers as its description.
///
/// `fs.read` can carry the whole file to the model, so a body that fails to
/// read costs the skill its description and nothing more: the name and the
/// path are still enough for the model to find it when it wants to.
pub(crate) fn skill_description(root: &Path, relative: &str) -> Option<String> {
    let path = Path::new(relative);
    let text = if path.is_absolute() {
        // User skills come from the operator's home and use absolute paths.
        std::fs::read_to_string(path).ok()?
    } else {
        // Workspace declarations are relative. Resolve through the workspace
        // so a skill symlink cannot read a file outside it.
        let content = arsy_code::resource::Workspace::open(root)
            .ok()?
            .resolve_file(path)
            .ok()?
            .read(4 * 1024 * 1024)
            .ok()?;
        String::from_utf8(content.bytes).ok()?
    };
    let body = text.strip_prefix("---")?;
    let front = body.split("---").next()?;
    front.lines().find_map(|line| {
        let value = line.strip_prefix("description:")?;
        let description = value.trim().trim_matches('"').trim_matches('\'');
        (!description.is_empty()).then(|| description.to_owned())
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::skill_description;

    #[test]
    fn workspace_skill_description_stays_inside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let skill = workspace.path().join(".claude/skills/demo/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "---\ndescription: in workspace\n---\n").unwrap();
        assert_eq!(
            skill_description(workspace.path(), ".claude/skills/demo/SKILL.md"),
            Some("in workspace".to_owned())
        );

        let external = outside.path().join("SKILL.md");
        std::fs::write(&external, "---\ndescription: outside workspace\n---\n").unwrap();
        std::fs::remove_file(&skill).unwrap();
        std::os::unix::fs::symlink(&external, &skill).unwrap();
        assert_eq!(
            skill_description(workspace.path(), ".claude/skills/demo/SKILL.md"),
            None
        );
    }
}

/// The operator's own Claude Code and Codex instructions, then the
/// repository's, root first.
///
/// The switches come from the invocation's resolved config, `--config`
/// included, so the prompt follows the same compat policy as everything else.
fn instructions_for(
    workspace: &arsy_code::resource::Workspace,
    config: &arsy_kernel::config::Config,
    working: &Path,
) -> Vec<arsy_code::agent::instructions::Instruction> {
    use arsy_code::agent::instructions::{self, Instruction, MAX_INSTRUCTION_BYTES};
    let enabled = |source: &str| config.compat_enabled(source);
    arsy_compat::instructions::user_instructions(
        &compat_homes(),
        enabled("claude"),
        enabled("codex"),
        MAX_INSTRUCTION_BYTES,
    )
    .into_iter()
    .map(|found| Instruction {
        path: found.path.display().to_string(),
        text: found.text,
        truncated: found.truncated,
        operator: true,
    })
    .chain(instructions::discover_with(
        workspace,
        working,
        enabled("claude"),
    ))
    .collect()
}
