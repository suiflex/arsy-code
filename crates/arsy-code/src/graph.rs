//! The repository knowledge graph, and what invalidates it.
//!
//! See `docs/17-memory-knowledge.md`. The graph exists to answer "what does
//! this touch" without re-reading the repository, so the property that matters
//! is incremental correctness: re-indexing must produce exactly the graph a
//! full rebuild would, while doing work only for the files whose content
//! actually changed.
//!
//! Identity is content-addressed where it can be. A file node is keyed by its
//! path and carries the digest of its bytes; a symbol node is keyed by the path
//! and the name it binds, which survives an edit elsewhere in the file. An
//! import that cannot be resolved to a file keeps a module node named by the
//! path as written — a revision-bound fallback rather than a guess.

use crate::{resource::Workspace, syntax::RustSyntax};
use arsy_kernel::domain::StateVersion;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

/// Largest file the graph reads. Beyond this a file is a node with no symbols:
/// a generated blob is not worth parsing and must not be able to stall an index.
pub const MAX_INDEXED_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Symbol,
    /// An import target that resolves to no file in this workspace.
    Module,
}

/// A node's stable name. `file:src/main.rs`, `symbol:src/main.rs#run`,
/// `module:serde::Serialize`.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct NodeId(String);

impl NodeId {
    pub fn file(path: &Path) -> Self {
        Self(format!("file:{}", slash(path)))
    }

    pub fn symbol(path: &Path, name: &str) -> Self {
        Self(format!("symbol:{}#{name}", slash(path)))
    }

    pub fn module(path: &str) -> Self {
        Self(format!("module:{path}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    /// The file this node lives in. A module node has none.
    pub path: Option<PathBuf>,
    pub name: String,
    /// What the declaration is: `function_item`, `struct_item`, and so on.
    pub declaration: Option<String>,
    /// Digest of the file the node came from, so a stale node is recognizable.
    pub revision: Option<StateVersion>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A file declares a symbol.
    Defines,
    /// A file imports a module or another file.
    Imports,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Edge {
    pub from: NodeId,
    pub kind: EdgeKind,
    pub to: NodeId,
}

/// What one index pass changed. This is the measurement that says the index was
/// incremental rather than a rebuild wearing its name.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct IndexDelta {
    pub added: Vec<PathBuf>,
    pub updated: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    /// Files whose digest matched, so nothing was reparsed for them.
    pub unchanged: usize,
    /// Files read but not parsed: too large, or a language with no grammar.
    ///
    /// Counted where the parse is attempted and nowhere else, so a file that
    /// is both too large and unchanged is not counted twice — or counted at
    /// all, since an unchanged file is never read for symbols.
    pub unparsed: usize,
}

impl IndexDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }

    /// Files whose nodes and edges were rebuilt by this pass.
    pub fn reindexed(&self) -> usize {
        self.added.len() + self.updated.len()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileEntry {
    digest: StateVersion,
    nodes: BTreeSet<NodeId>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum GraphError {
    Walk(String),
    Io(String),
}

impl fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Walk(detail) | Self::Io(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for GraphError {}

/// Serializable so a map can be kept between processes: re-walking a large
/// repository on every turn is the cost this whole structure exists to avoid,
/// and it is wasted if the answer dies with the process that computed it.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct KnowledgeGraph {
    files: BTreeMap<PathBuf, FileEntry>,
    nodes: BTreeMap<NodeId, Node>,
    edges: BTreeSet<Edge>,
}

impl KnowledgeGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.iter()
    }

    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Walk the workspace and bring the graph up to date.
    ///
    /// A file whose bytes hash to what the graph already recorded is skipped
    /// entirely — not reparsed, not re-linked — so the cost of a pass is the
    /// cost of what changed plus one digest per file.
    pub fn index(&mut self, workspace: &Workspace) -> Result<IndexDelta, GraphError> {
        let root = workspace.path();
        let mut delta = IndexDelta::default();
        let mut seen = BTreeSet::new();

        for entry in WalkBuilder::new(root)
            .standard_filters(true)
            .require_git(false)
            .build()
        {
            let entry = entry.map_err(|error| GraphError::Walk(error.to_string()))?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_path_buf();
            seen.insert(relative.clone());

            let metadata = entry
                .metadata()
                .map_err(|error| GraphError::Walk(error.to_string()))?;
            let bytes = match std::fs::read(entry.path()) {
                Ok(bytes) => bytes,
                // A file that vanished between the walk and the read is simply
                // not there; the next pass will record its removal.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(GraphError::Io(error.to_string())),
            };
            let digest = StateVersion::from_digest(Sha256::digest(&bytes).into());

            match self.files.get(&relative) {
                Some(existing) if existing.digest == digest => {
                    delta.unchanged += 1;
                    continue;
                }
                Some(_) => delta.updated.push(relative.clone()),
                None => delta.added.push(relative.clone()),
            }
            self.forget(&relative);
            let parsed = self.insert_file(&relative, &bytes, digest, metadata.len());
            if !parsed {
                delta.unparsed += 1;
            }
        }

        for path in self.files.keys().cloned().collect::<Vec<_>>() {
            if !seen.contains(&path) {
                self.forget(&path);
                delta.removed.push(path);
            }
        }
        // Module nodes exist only as import targets; one whose last importer is
        // gone would otherwise linger as a node nothing points at.
        self.collect_orphan_modules();
        Ok(delta)
    }

    /// Drop a file's nodes and every edge touching them.
    ///
    /// Invalidation is by file because a file is the unit that changes: a
    /// symbol cannot be edited without its file changing, and re-deriving one
    /// file's nodes is cheap enough that finer granularity would buy nothing.
    pub fn forget(&mut self, path: &Path) {
        let Some(entry) = self.files.remove(path) else {
            return;
        };
        let file = NodeId::file(path);
        for id in entry.nodes.iter().chain(std::iter::once(&file)) {
            self.nodes.remove(id);
        }
        self.edges
            .retain(|edge| !entry.nodes.contains(&edge.from) && !entry.nodes.contains(&edge.to));
    }

    /// Add one file's nodes and edges. Returns whether it was parsed for
    /// symbols, as opposed to recorded as a file and nothing more.
    fn insert_file(&mut self, path: &Path, bytes: &[u8], digest: StateVersion, size: u64) -> bool {
        let file = NodeId::file(path);
        let mut owned = BTreeSet::from([file.clone()]);
        self.nodes.insert(
            file.clone(),
            Node {
                id: file.clone(),
                kind: NodeKind::File,
                path: Some(path.to_owned()),
                name: slash(path),
                declaration: None,
                revision: Some(digest),
            },
        );

        // Rust is the one grammar this build pins, so it is the one language
        // that yields symbols. Every other file is still a node — the graph
        // knows it exists and what imports it — but has none.
        let parsed = size <= MAX_INDEXED_BYTES
            && path.extension().and_then(|value| value.to_str()) == Some("rs")
            && self.insert_rust(path, bytes, digest, &file, &mut owned);

        self.files.insert(
            path.to_owned(),
            FileEntry {
                digest,
                nodes: owned,
            },
        );
        parsed
    }

    fn insert_rust(
        &mut self,
        path: &Path,
        bytes: &[u8],
        digest: StateVersion,
        file: &NodeId,
        owned: &mut BTreeSet<NodeId>,
    ) -> bool {
        let Ok(syntax) = RustSyntax::new(bytes.to_vec()) else {
            return false;
        };
        for kind in DECLARATIONS {
            let Ok(declarations) = syntax.declarations(kind) else {
                continue;
            };
            for (node, name) in declarations {
                let id = NodeId::symbol(path, &name);
                self.nodes.insert(
                    id.clone(),
                    Node {
                        id: id.clone(),
                        kind: NodeKind::Symbol,
                        path: Some(path.to_owned()),
                        name,
                        declaration: Some(node.kind.clone()),
                        revision: Some(digest),
                    },
                );
                self.edges.insert(Edge {
                    from: file.clone(),
                    kind: EdgeKind::Defines,
                    to: id.clone(),
                });
                owned.insert(id);
            }
        }

        let Ok(imports) = syntax.imports() else {
            return true;
        };
        for import in imports {
            let Some(text) = syntax.source().get(import.bytes.clone()) else {
                continue;
            };
            for target in import_targets(&String::from_utf8_lossy(text)) {
                // An import is recorded as the path it names, never as the file
                // that path happens to resolve to right now. Resolving at parse
                // time would make the graph depend on the order files were
                // walked in — a file indexed before its target would keep a
                // different edge than one indexed after it — and an incremental
                // index would drift from a rebuild.
                let to = NodeId::module(&target);
                if !self.nodes.contains_key(&to) {
                    self.nodes.insert(
                        to.clone(),
                        Node {
                            id: to.clone(),
                            kind: NodeKind::Module,
                            path: None,
                            name: target.clone(),
                            declaration: None,
                            revision: None,
                        },
                    );
                }
                self.edges.insert(Edge {
                    from: file.clone(),
                    kind: EdgeKind::Imports,
                    to,
                });
            }
        }
        true
    }

    /// Remove module nodes nothing imports any more.
    fn collect_orphan_modules(&mut self) {
        let referenced: BTreeSet<&NodeId> = self.edges.iter().map(|edge| &edge.to).collect();
        let orphans: Vec<NodeId> = self
            .nodes
            .values()
            .filter(|node| node.kind == NodeKind::Module && !referenced.contains(&node.id))
            .map(|node| node.id.clone())
            .collect();
        for orphan in orphans {
            self.nodes.remove(&orphan);
        }
    }

    /// Symbols with this exact name, wherever they are declared.
    pub fn symbols(&self, name: &str) -> Vec<&Node> {
        self.nodes
            .values()
            .filter(|node| node.kind == NodeKind::Symbol && node.name == name)
            .collect()
    }

    /// What `from` points at, along edges of `kind`.
    pub fn neighbours(&self, from: &NodeId, kind: EdgeKind) -> Vec<&Node> {
        self.edges
            .iter()
            .filter(|edge| edge.from == *from && edge.kind == kind)
            .filter_map(|edge| self.nodes.get(&edge.to))
            .collect()
    }

    /// Files that import `path`, directly.
    ///
    /// This is the question invalidation exists to answer: when a file changes,
    /// what else might now be wrong. Resolution happens here rather than at
    /// index time, so it always reflects the current file set.
    pub fn importers_of(&self, path: &Path) -> Vec<&Node> {
        let Some(module) = module_path_of(path) else {
            return Vec::new();
        };
        let item_prefix = format!("{module}::");
        let mut importers: Vec<&Node> = self
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Imports)
            .filter(|edge| {
                self.nodes.get(&edge.to).is_some_and(|node| {
                    // The module itself, or an item inside it. `crate::a` must
                    // not match `crate::abc`, so the boundary is explicit.
                    node.name == module || node.name.starts_with(&item_prefix)
                })
            })
            .filter_map(|edge| self.nodes.get(&edge.from))
            .filter(|node| node.path.as_deref() != Some(path))
            .collect();
        importers.dedup_by(|left, right| left.id == right.id);
        importers
    }
}

/// Declarations worth a node. Deliberately the ones a person searches for.
/// The Rust declarations this index addresses. Public because a symbol the
/// graph named must be findable again by whoever it named it to.
pub const DECLARATIONS: &[&str] = &[
    "function_item",
    "struct_item",
    "enum_item",
    "trait_item",
    "type_item",
    "mod_item",
    "const_item",
    "static_item",
    "macro_definition",
];

/// The module paths a `use` declaration names.
///
/// A brace group expands to one target per branch, so `use a::{b, c};` names
/// `a::b` and `a::c`. Nested braces are flattened the same way, and a `*` or an
/// `as` alias contributes the path up to it. This is textual because the target
/// of an import is a path, not a tree the caller ever needs.
fn import_targets(declaration: &str) -> Vec<String> {
    let body = declaration
        .trim()
        .trim_start_matches("pub")
        .trim_start()
        .trim_start_matches("use")
        .trim()
        .trim_end_matches(';')
        .trim();
    let mut targets = Vec::new();
    expand(body, "", &mut targets);
    targets
}

fn expand(body: &str, prefix: &str, targets: &mut Vec<String>) {
    let Some(open) = body.find('{') else {
        let path = join(prefix, body.trim());
        if !path.is_empty() {
            targets.push(path);
        }
        return;
    };
    // A half-edited file can close a group before it opens one.
    let Some(close) = body.rfind('}').filter(|close| *close > open) else {
        return;
    };
    let head = join(prefix, body[..open].trim().trim_end_matches("::").trim());
    for branch in split_top_level(&body[open + 1..close]) {
        expand(branch.trim(), &head, targets);
    }
}

/// Split on commas that are not inside a nested brace group.
fn split_top_level(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (index, character) in body.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);
    parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect()
}

fn join(prefix: &str, tail: &str) -> String {
    // `self` in a brace group means the prefix itself, and an alias or glob
    // contributes only the path in front of it.
    let tail = tail
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches("::*");
    match (prefix.is_empty(), tail.is_empty() || tail == "self") {
        (_, true) => prefix.to_owned(),
        (true, false) => tail.to_owned(),
        (false, false) => format!("{prefix}::{tail}"),
    }
}

/// The `crate::`-rooted module path a file provides, or `None` for a file that
/// is not a Rust module at all.
///
/// Both layouts collapse to the same path: `a/mod.rs` and `a.rs` are both
/// `crate::a`. Everything before a `src` component belongs to whichever crate
/// owns the file and is dropped, so one workspace of many crates does not need
/// a different rule from a single-crate one.
fn module_path_of(path: &Path) -> Option<String> {
    if path.extension().and_then(|value| value.to_str()) != Some("rs") {
        return None;
    }
    let mut segments: Vec<String> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::to_owned)
        .collect();
    if let Some(source) = segments.iter().rposition(|segment| segment == "src") {
        segments.drain(..=source);
    }
    let last = segments.pop()?;
    let stem = last.trim_end_matches(".rs");
    // `mod.rs` and the crate roots name the directory they are in, not a
    // module of their own.
    if !matches!(stem, "mod" | "lib" | "main") {
        segments.push(stem.to_owned());
    }
    Some(if segments.is_empty() {
        "crate".to_owned()
    } else {
        format!("crate::{}", segments.join("::"))
    })
}

fn slash(path: &Path) -> String {
    path.components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let directory = tempfile::tempdir().unwrap();
        for (path, body) in files {
            let path = directory.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let workspace = Workspace::open(directory.path()).unwrap();
        (directory, workspace)
    }

    #[test]
    fn import_paths_expand_through_braces_aliases_and_globs() {
        assert_eq!(
            import_targets("use serde::Serialize;"),
            ["serde::Serialize"]
        );
        assert_eq!(
            import_targets("pub use std::collections::{BTreeMap, BTreeSet};"),
            ["std::collections::BTreeMap", "std::collections::BTreeSet"]
        );
        assert_eq!(
            import_targets("use crate::a::{b::{c, d}, e};"),
            ["crate::a::b::c", "crate::a::b::d", "crate::a::e"]
        );
        assert_eq!(
            import_targets("use std::io::Write as _;"),
            ["std::io::Write"]
        );
        assert_eq!(import_targets("use std::fmt::*;"), ["std::fmt"]);
        assert_eq!(
            import_targets("use crate::graph::{self, NodeId};"),
            ["crate::graph", "crate::graph::NodeId"]
        );
    }

    #[test]
    fn a_file_maps_to_one_module_path_whichever_layout_it_uses() {
        for (path, expected) in [
            ("src/a.rs", Some("crate::a")),
            ("src/a/mod.rs", Some("crate::a")),
            ("src/a/b.rs", Some("crate::a::b")),
            ("src/lib.rs", Some("crate")),
            ("src/main.rs", Some("crate")),
            ("crates/arsy-code/src/graph.rs", Some("crate::graph")),
            ("README.md", None),
        ] {
            assert_eq!(
                module_path_of(Path::new(path)).as_deref(),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn indexing_records_files_symbols_and_resolved_imports() {
        let (_directory, workspace) = workspace(&[
            (
                "src/lib.rs",
                "pub mod util;\nuse crate::util::helper;\npub fn run() { helper(); }\n",
            ),
            ("src/util.rs", "pub fn helper() {}\npub struct Config;\n"),
            ("README.md", "# not rust\n"),
        ]);
        let mut graph = KnowledgeGraph::new();
        let delta = graph.index(&workspace).unwrap();

        assert_eq!(delta.added.len(), 3);
        assert_eq!(delta.unchanged, 0);
        assert!(delta.removed.is_empty());
        assert_eq!(
            delta.unparsed, 1,
            "the markdown file is a node, not symbols"
        );

        // A second pass reparses nothing, so it reports nothing unparsed:
        // `unparsed` counts work skipped, not files that would be skipped.
        let again = graph.index(&workspace).unwrap();
        assert_eq!(again.unchanged, 3);
        assert_eq!(again.unparsed, 0, "an unchanged file was not parsed again");
        assert_eq!(graph.file_count(), 3);

        assert_eq!(graph.symbols("helper").len(), 1);
        assert_eq!(
            graph.symbols("Config")[0].declaration.as_deref(),
            Some("struct_item")
        );
        assert!(graph.symbols("nothing_of_the_sort").is_empty());

        // `use crate::util::helper` resolves to the file that defines it, so
        // the graph can answer "what depends on util.rs".
        let importers = graph.importers_of(Path::new("src/util.rs"));
        assert_eq!(importers.len(), 1);
        assert_eq!(importers[0].name, "src/lib.rs");

        // `crate::a` must not be matched by a module whose name merely starts
        // with those letters.
        assert!(graph.importers_of(Path::new("src/uti.rs")).is_empty());

        // An import of something outside the workspace stays a module node.
        let defined = graph.neighbours(&NodeId::file(Path::new("src/lib.rs")), EdgeKind::Defines);
        assert!(defined.iter().any(|node| node.name == "run"));
        assert!(defined.iter().any(|node| node.name == "util"));
    }

    #[test]
    fn a_second_pass_reparses_only_what_changed() {
        let (directory, workspace) = workspace(&[
            ("src/lib.rs", "pub fn run() {}\n"),
            ("src/util.rs", "pub fn helper() {}\n"),
        ]);
        let mut graph = KnowledgeGraph::new();
        graph.index(&workspace).unwrap();

        let delta = graph.index(&workspace).unwrap();
        assert!(delta.is_empty(), "nothing changed, so nothing was indexed");
        assert_eq!(delta.unchanged, 2);
        assert_eq!(delta.reindexed(), 0);

        std::fs::write(
            directory.path().join("src/util.rs"),
            "pub fn helper() {}\npub fn added() {}\n",
        )
        .unwrap();
        let delta = graph.index(&workspace).unwrap();
        assert_eq!(delta.updated, vec![PathBuf::from("src/util.rs")]);
        assert_eq!(delta.unchanged, 1, "lib.rs was not reparsed");
        assert_eq!(graph.symbols("added").len(), 1);

        // A removed file takes its symbols with it.
        std::fs::remove_file(directory.path().join("src/util.rs")).unwrap();
        let delta = graph.index(&workspace).unwrap();
        assert_eq!(delta.removed, vec![PathBuf::from("src/util.rs")]);
        assert!(graph.symbols("helper").is_empty());
        assert!(graph.symbols("added").is_empty());
        assert!(graph
            .node(&NodeId::file(Path::new("src/util.rs")))
            .is_none());
        assert_eq!(graph.file_count(), 1);
    }

    #[test]
    fn an_incremental_index_equals_a_full_rebuild() {
        let (directory, workspace) = workspace(&[
            ("src/lib.rs", "use crate::a::one;\npub fn run() {}\n"),
            ("src/a.rs", "pub fn one() {}\n"),
            ("src/b.rs", "use serde::Serialize;\npub struct B;\n"),
        ]);
        let mut incremental = KnowledgeGraph::new();
        incremental.index(&workspace).unwrap();

        // Edit one file, add another, delete a third.
        std::fs::write(
            directory.path().join("src/a.rs"),
            "pub fn one() {}\npub fn two() {}\n",
        )
        .unwrap();
        std::fs::write(directory.path().join("src/c.rs"), "pub enum C { X }\n").unwrap();
        std::fs::remove_file(directory.path().join("src/b.rs")).unwrap();
        let delta = incremental.index(&workspace).unwrap();
        assert_eq!(delta.updated, vec![PathBuf::from("src/a.rs")]);
        assert_eq!(delta.added, vec![PathBuf::from("src/c.rs")]);
        assert_eq!(delta.removed, vec![PathBuf::from("src/b.rs")]);

        let mut rebuilt = KnowledgeGraph::new();
        rebuilt.index(&workspace).unwrap();
        assert_eq!(
            incremental.nodes().collect::<Vec<_>>(),
            rebuilt.nodes().collect::<Vec<_>>(),
            "an incremental index must not drift from a rebuild"
        );
        assert_eq!(
            incremental.edges().collect::<Vec<_>>(),
            rebuilt.edges().collect::<Vec<_>>()
        );
        // The dependency of the deleted file is gone with it: no module node
        // survives with nothing pointing at it.
        assert!(incremental
            .nodes()
            .all(|node| node.name != "serde::Serialize"));
    }

    /// A half-edited `use` can close a brace group before opening one; the
    /// slice between them used to run backwards and panic.
    #[test]
    fn a_brace_that_closes_before_it_opens_is_skipped() {
        for declaration in ["use a}::{b;", "use }{;", "use a::{b}}::{c;"] {
            let _ = import_targets(declaration);
        }
    }
}
