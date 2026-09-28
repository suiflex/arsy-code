//! Compact task-oriented code intelligence over an LSP transport.

use crate::{
    lsp::{LspError, LspHost, LspRequest, LspTransport, MAX_LSP_BATCH},
    resource::Workspace,
    syntax::RustSyntax,
};
use arsy_kernel::domain::StateVersion;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt,
    ops::Range,
    path::{Path, PathBuf},
};

pub const MAX_SEMANTIC_RESULTS: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceProvider {
    Lsp,
    Syntax,
    Text,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SymbolId(String);

impl SymbolId {
    pub fn new(value: impl Into<String>) -> Result<Self, IntelligenceError> {
        let value = value.into();
        if value.is_empty() || value.len() > 1024 {
            return Err(IntelligenceError::InvalidSymbol);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolQuery {
    pub name: String,
    pub max_results: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceLocation {
    pub uri: String,
    pub bytes: Range<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SymbolHit {
    pub id: SymbolId,
    pub name: String,
    pub location: SourceLocation,
    pub source_revision: StateVersion,
    pub provider: EvidenceProvider,
    pub confidence_basis_points: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SymbolEvidence {
    pub symbol: SymbolId,
    pub summary: String,
    pub citations: Vec<SourceLocation>,
    pub source_revision: StateVersion,
    pub provider: EvidenceProvider,
    pub confidence_basis_points: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReferenceGraph {
    pub callers: Vec<SymbolHit>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CodeDiagnostic {
    pub location: SourceLocation,
    pub severity: u8,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiagnosticSet {
    pub diagnostics: Vec<CodeDiagnostic>,
    pub source_revision: StateVersion,
    pub provider: EvidenceProvider,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceTextEdit {
    pub uri: String,
    pub bytes: Range<usize>,
    pub new_text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEditPlan {
    pub server: String,
    pub revision: StateVersion,
    pub symbol: SymbolId,
    pub new_name: String,
    pub edits: Vec<WorkspaceTextEdit>,
}

pub trait CodeIntelligence {
    fn find_symbol(&mut self, query: &SymbolQuery) -> Result<Vec<SymbolHit>, IntelligenceError>;
    fn explain_symbol(&mut self, id: &SymbolId) -> Result<SymbolEvidence, IntelligenceError>;
    fn find_callers(&mut self, id: &SymbolId) -> Result<ReferenceGraph, IntelligenceError>;
    fn diagnostics(&mut self, scope: &str) -> Result<DiagnosticSet, IntelligenceError>;
    fn plan_rename(
        &mut self,
        id: &SymbolId,
        name: &str,
    ) -> Result<WorkspaceEditPlan, IntelligenceError>;
}

/// The top tier: a language server, asked the questions LSP actually defines.
///
/// # Positions, and why they are converted here
///
/// LSP addresses code by line and character, and — unless the server agreed to
/// UTF-8 — a character is a UTF-16 code unit. Everything inside ARSY addresses
/// code by byte range, because that is what an edit needs and what an artifact
/// can cite. Converting at this boundary means no layer above ever holds a
/// position whose meaning depends on an encoding negotiated at startup.
///
/// # Why the file is opened first
///
/// A server answers a positional request about documents it has been told
/// about. `textDocument/didOpen` is that telling, and it carries the text, so
/// the server answers about the bytes this process read rather than whatever
/// is on disk at the moment it looks.
pub struct LspCodeIntelligence<'a, T> {
    server: String,
    revision: StateVersion,
    host: LspHost<T>,
    workspace: &'a Workspace,
    /// Documents already announced to the server, and the text announced.
    opened: BTreeMap<String, String>,
    /// The position encoding the server agreed to.
    encoding: PositionEncoding,
}

/// How a server counts a `character` in a position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PositionEncoding {
    Utf8,
    Utf16,
}

impl<'a, T: LspTransport> LspCodeIntelligence<'a, T> {
    pub fn new(
        server: impl Into<String>,
        revision: StateVersion,
        host: LspHost<T>,
        workspace: &'a Workspace,
    ) -> Self {
        Self {
            server: server.into(),
            revision,
            host,
            workspace,
            opened: BTreeMap::new(),
            encoding: PositionEncoding::Utf16,
        }
    }

    fn one(&mut self, method: &str, params: Value) -> Result<Value, IntelligenceError> {
        self.host
            .request_batch(vec![LspRequest {
                method: method.into(),
                params,
            }])?
            .pop()
            .ok_or_else(|| IntelligenceError::Protocol("missing LSP result".into()))
    }

    pub fn request_many(
        &mut self,
        requests: Vec<LspRequest>,
    ) -> Result<Vec<Value>, IntelligenceError> {
        if requests.len() > MAX_LSP_BATCH {
            return Err(IntelligenceError::FanoutExceeded);
        }
        self.host.request_batch(requests).map_err(Into::into)
    }

    /// Start the server if it is not running, and read the encoding it chose.
    fn ready(&mut self) -> Result<(), IntelligenceError> {
        if self.host.state() == crate::lsp::ServerState::Stopped {
            self.host.start()?;
        }
        if self
            .host
            .capabilities()
            .get("positionEncoding")
            .and_then(Value::as_str)
            == Some("utf-8")
        {
            self.encoding = PositionEncoding::Utf8;
        }
        Ok(())
    }

    /// Tell the server about a file, once, with the text this process read.
    fn open(&mut self, path: &Path) -> Result<String, IntelligenceError> {
        self.ready()?;
        let uri = crate::lsp::file_uri(&self.workspace.path().join(path));
        if let Some(text) = self.opened.get(&uri) {
            return Ok(text.clone());
        }
        let content = self.workspace.read(path, MAX_DOCUMENT_BYTES)?;
        let text = String::from_utf8(content.bytes)
            .map_err(|_| IntelligenceError::Protocol(format!("{} is not UTF-8", path.display())))?;
        self.host.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri,
                "languageId": language_of(path),
                "version": 1,
                "text": text,
            }}),
        )?;
        self.opened.insert(uri, text.clone());
        Ok(text)
    }

    /// The text of a URI, from the open set or from disk.
    fn text_of(&mut self, uri: &str) -> Option<String> {
        if let Some(text) = self.opened.get(uri) {
            return Some(text.clone());
        }
        let path = crate::lsp::uri_path(uri);
        let relative = path.strip_prefix(self.workspace.path()).unwrap_or(&path);
        let content = self.workspace.read(relative, MAX_DOCUMENT_BYTES).ok()?;
        String::from_utf8(content.bytes).ok()
    }

    /// One LSP range as a byte range in `text`.
    fn bytes_of(&self, text: &str, range: &Value) -> Range<usize> {
        let offset = |key: &str| {
            range
                .get(key)
                .map(|position| {
                    byte_offset(
                        text,
                        position.get("line").and_then(Value::as_u64).unwrap_or(0),
                        position
                            .get("character")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        self.encoding,
                    )
                })
                .unwrap_or(0)
        };
        let start = offset("start");
        start..offset("end").max(start)
    }

    /// A location as this crate's own, with the byte range resolved.
    fn location_of(&mut self, location: &Value) -> Option<(String, Range<usize>)> {
        let uri = location.get("uri").and_then(Value::as_str)?.to_owned();
        let range = location.get("range").cloned().unwrap_or(Value::Null);
        let bytes = self
            .text_of(&uri)
            .map_or(0..0, |text| self.bytes_of(&text, &range));
        Some((uri, bytes))
    }

    /// The position a symbol id names, and the file it is in.
    fn address(&self, id: &SymbolId) -> Result<(PathBuf, u64, u64), IntelligenceError> {
        let (uri, position) = id
            .as_str()
            .strip_prefix("lsp:")
            .and_then(|rest| rest.rsplit_once('#'))
            .ok_or_else(|| {
                IntelligenceError::Protocol(format!(
                    "`{}` is not a symbol id from this provider; find one with code.symbol",
                    id.as_str()
                ))
            })?;
        let (line, character) = position
            .split_once(':')
            .ok_or_else(|| IntelligenceError::Protocol("a symbol id carries a position".into()))?;
        let path = crate::lsp::uri_path(uri);
        let relative = path
            .strip_prefix(self.workspace.path())
            .unwrap_or(&path)
            .to_path_buf();
        Ok((
            relative,
            line.parse().unwrap_or(0),
            character.parse().unwrap_or(0),
        ))
    }

    /// The request shape every positional method shares.
    fn at(&mut self, id: &SymbolId) -> Result<(String, Value, String), IntelligenceError> {
        let (path, line, character) = self.address(id)?;
        let text = self.open(&path)?;
        let uri = crate::lsp::file_uri(&self.workspace.path().join(&path));
        Ok((
            uri.clone(),
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character},
            }),
            text,
        ))
    }

    fn hit(&mut self, name: String, location: &Value, confidence: u16) -> Option<SymbolHit> {
        let (uri, bytes) = self.location_of(location)?;
        let position = location.get("range").and_then(|range| range.get("start"));
        let line = position
            .and_then(|start| start.get("line"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let character = position
            .and_then(|start| start.get("character"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Some(SymbolHit {
            id: SymbolId::new(format!("lsp:{uri}#{line}:{character}")).ok()?,
            name,
            location: SourceLocation {
                uri: uri.clone(),
                bytes,
            },
            source_revision: self.revision,
            provider: EvidenceProvider::Lsp,
            confidence_basis_points: confidence,
        })
    }
}

/// Largest file this tier will hand a server or convert positions in.
pub const MAX_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;

/// A digest of the documents an edit plan was computed against.
///
/// The planner computes this over the text it read; whatever applies the plan
/// computes it again over the same URIs as they are on disk. Equal means the
/// bytes the offsets were computed against are still the bytes that will be
/// edited — which is the only thing that makes byte offsets from a language
/// server safe to apply at all.
///
/// Order-independent, because the two sides walk the same files in whatever
/// order their own structures give them.
pub fn document_revision<'a>(
    documents: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> StateVersion {
    let ordered: BTreeMap<&str, &str> = documents.into_iter().collect();
    let mut hasher = Sha256::new();
    for (uri, text) in ordered {
        hasher.update(uri.as_bytes());
        hasher.update([0]);
        hasher.update(Sha256::digest(text.as_bytes()));
    }
    StateVersion::from_digest(hasher.finalize().into())
}

/// The `languageId` a server expects for a file, by extension.
///
/// Wrong-but-present is better than absent: a server that does not recognize
/// the id ignores the document, where one given no id at all may reject the
/// notification outright.
fn language_of(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => "rust",
        Some("ts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("js") => "javascript",
        Some("py") => "python",
        Some("go") => "go",
        Some("c" | "h") => "c",
        Some("cc" | "cpp" | "hpp") => "cpp",
        Some("java") => "java",
        Some("rb") => "ruby",
        Some("json") => "json",
        Some("toml") => "toml",
        _ => "plaintext",
    }
}

/// A line/character position as a byte offset into `text`.
///
/// The character count is in UTF-16 code units unless the server agreed to
/// UTF-8, which is why the encoding is carried rather than assumed: getting
/// this wrong puts an edit in the middle of a character on any line with an
/// emoji or an accent in it.
fn byte_offset(text: &str, line: u64, character: u64, encoding: PositionEncoding) -> usize {
    let mut offset = 0;
    for (index, current) in text.split_inclusive('\n').enumerate() {
        if index as u64 != line {
            offset += current.len();
            continue;
        }
        let mut counted = 0u64;
        for (byte, glyph) in current.char_indices() {
            if counted >= character {
                return offset + byte;
            }
            counted += match encoding {
                PositionEncoding::Utf8 => glyph.len_utf8() as u64,
                PositionEncoding::Utf16 => glyph.len_utf16() as u64,
            };
        }
        // Past the end of the line is the end of the line, not an error: an
        // exclusive range end is written that way.
        return offset + current.trim_end_matches(['\r', '\n']).len();
    }
    text.len()
}

impl<T: LspTransport> CodeIntelligence for LspCodeIntelligence<'_, T> {
    fn find_symbol(&mut self, query: &SymbolQuery) -> Result<Vec<SymbolHit>, IntelligenceError> {
        if query.name.is_empty() || query.max_results == 0 {
            return Err(IntelligenceError::InvalidQuery);
        }
        self.ready()?;
        let limit = query.max_results.min(MAX_SEMANTIC_RESULTS);
        let found = self.one("workspace/symbol", json!({"query": query.name}))?;
        let symbols: Vec<Value> = found.as_array().cloned().unwrap_or_default();
        Ok(symbols
            .into_iter()
            .filter(|symbol| symbol.get("name").and_then(Value::as_str) == Some(&query.name))
            .take(limit)
            .filter_map(|symbol| {
                let name = symbol.get("name")?.as_str()?.to_owned();
                let location = symbol.get("location")?.clone();
                self.hit(name, &location, 9_000)
            })
            .collect())
    }

    /// What the server says the symbol is, from `textDocument/hover`.
    fn explain_symbol(&mut self, id: &SymbolId) -> Result<SymbolEvidence, IntelligenceError> {
        let (uri, position, text) = self.at(id)?;
        let hover = self.one("textDocument/hover", position)?;
        if hover.is_null() {
            return Err(IntelligenceError::Protocol(
                "the server knows nothing about that position".into(),
            ));
        }
        let summary = hover_text(hover.get("contents").unwrap_or(&Value::Null));
        if summary.len() > MAX_SUMMARY_BYTES {
            return Err(IntelligenceError::ResultTooLarge);
        }
        let bytes = hover
            .get("range")
            .map_or(0..0, |range| self.bytes_of(&text, range));
        Ok(SymbolEvidence {
            symbol: id.clone(),
            summary,
            citations: vec![SourceLocation { uri, bytes }],
            source_revision: self.revision,
            provider: EvidenceProvider::Lsp,
            confidence_basis_points: 9_000,
        })
    }

    /// Every reference the server can prove, which is what a rename would
    /// touch and what a change might break.
    fn find_callers(&mut self, id: &SymbolId) -> Result<ReferenceGraph, IntelligenceError> {
        let (_, mut position, _) = self.at(id)?;
        position["context"] = json!({"includeDeclaration": false});
        let found = self.one("textDocument/references", position)?;
        let locations: Vec<Value> = found.as_array().cloned().unwrap_or_default();
        let truncated = locations.len() > MAX_SEMANTIC_RESULTS;
        let callers = locations
            .into_iter()
            .take(MAX_SEMANTIC_RESULTS)
            .filter_map(|location| {
                let name = crate::lsp::uri_path(location.get("uri")?.as_str()?)
                    .file_name()?
                    .to_string_lossy()
                    .into_owned();
                self.hit(name, &location, 9_000)
            })
            .collect();
        Ok(ReferenceGraph { callers, truncated })
    }

    /// The server's own diagnostics for one file, pulled rather than waited for.
    ///
    /// `textDocument/diagnostic` is the request form; the push form arrives as
    /// a notification whenever the server feels like it, which a request/reply
    /// transport cannot wait for without blocking on something that may never
    /// come.
    fn diagnostics(&mut self, scope: &str) -> Result<DiagnosticSet, IntelligenceError> {
        let path = PathBuf::from(scope);
        let text = self.open(&path)?;
        let uri = crate::lsp::file_uri(&self.workspace.path().join(&path));
        let report = self.one(
            "textDocument/diagnostic",
            json!({"textDocument": {"uri": uri.clone()}}),
        )?;
        let items: Vec<Value> = report
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let truncated = items.len() > MAX_SEMANTIC_RESULTS;
        let diagnostics = items
            .into_iter()
            .take(MAX_SEMANTIC_RESULTS)
            .map(|item| CodeDiagnostic {
                location: SourceLocation {
                    uri: uri.clone(),
                    bytes: self.bytes_of(&text, item.get("range").unwrap_or(&Value::Null)),
                },
                // LSP severity: 1 error, 2 warning, 3 information, 4 hint. An
                // absent severity means the server did not say, which is not
                // the same as "hint".
                severity: item
                    .get("severity")
                    .and_then(Value::as_u64)
                    .and_then(|severity| u8::try_from(severity).ok())
                    .unwrap_or(0),
                message: item
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
            .collect();
        Ok(DiagnosticSet {
            diagnostics,
            source_revision: self.revision,
            provider: EvidenceProvider::Lsp,
            truncated,
        })
    }

    /// The edits a rename would make, everywhere, as the server computes them.
    ///
    /// Nothing is applied here: the plan is bound to the revision it was
    /// computed against, and applying it is a transaction the edit engine
    /// owns.
    fn plan_rename(
        &mut self,
        id: &SymbolId,
        name: &str,
    ) -> Result<WorkspaceEditPlan, IntelligenceError> {
        if name.is_empty() {
            return Err(IntelligenceError::InvalidQuery);
        }
        let (_, mut position, _) = self.at(id)?;
        position["newName"] = json!(name);
        let workspace_edit = self.one("textDocument/rename", position)?;
        let changes = workspace_edit
            .get("changes")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| {
                IntelligenceError::Protocol(
                    "the server returned no edits for that rename; it may not be renameable".into(),
                )
            })?;

        let mut edits = Vec::new();
        for (uri, per_file) in changes {
            let Some(text) = self.text_of(&uri) else {
                return Err(IntelligenceError::Protocol(format!(
                    "the rename touches {uri}, which this workspace cannot read"
                )));
            };
            for edit in per_file.as_array().cloned().unwrap_or_default() {
                edits.push(WorkspaceTextEdit {
                    uri: uri.clone(),
                    bytes: self.bytes_of(&text, edit.get("range").unwrap_or(&Value::Null)),
                    new_text: edit
                        .get("newText")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
        }
        if edits.len() > MAX_SEMANTIC_RESULTS {
            return Err(IntelligenceError::ResultTooLarge);
        }
        if edits.is_empty() {
            return Err(IntelligenceError::Protocol(
                "the server planned no edits for that rename".into(),
            ));
        }
        // Sorted by file and then by position, descending within a file, so an
        // applier can walk them without an earlier edit moving a later one.
        edits.sort_by(|left, right| {
            left.uri
                .cmp(&right.uri)
                .then(right.bytes.start.cmp(&left.bytes.start))
        });
        // Bound to the text the offsets were computed against, not to the
        // server's own notion of a version: the applier re-reads these files
        // and refuses the plan if any of them moved underneath it.
        let touched: BTreeMap<String, String> = edits
            .iter()
            .filter_map(|edit| self.text_of(&edit.uri).map(|text| (edit.uri.clone(), text)))
            .collect();
        let revision = document_revision(
            touched
                .iter()
                .map(|(uri, text)| (uri.as_str(), text.as_str())),
        );
        Ok(WorkspaceEditPlan {
            server: self.server.clone(),
            revision,
            symbol: id.clone(),
            new_name: name.into(),
            edits,
        })
    }
}

/// Longest hover text kept. A server that answers with a page of documentation
/// is answering a different question.
const MAX_SUMMARY_BYTES: usize = 16 * 1024;

/// Hover contents as text, in any of the three shapes LSP allows.
fn hover_text(contents: &Value) -> String {
    match contents {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Value::Array(parts) => parts
            .iter()
            .map(hover_text)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned(),
        _ => String::new(),
    }
}

/// Level-2 fallback: tree-sitter declarations, indexed into the repository
/// graph.
///
/// Between the text tier, which can only find a string, and a language server,
/// which can prove what a name binds to. It knows a `fn foo` from a comment
/// mentioning `foo`, and it knows which files import the one a symbol lives
/// in — but an import is not a call and a declaration is not a definition site
/// proof, so it says so in its confidence and refuses the operations that
/// would need more than it has.
pub struct GraphCodeIntelligence<'a> {
    workspace: &'a Workspace,
    graph: crate::graph::KnowledgeGraph,
}

/// A symbol the graph tier found, addressed the way the graph addresses it.
///
/// The LSP tier passes an opaque server id through; this tier's ids are
/// `symbol:<path>#<name>`, so a follow-up call can find the declaration again
/// without the caller holding the graph.
struct GraphSymbol {
    path: PathBuf,
    name: String,
}

impl GraphSymbol {
    fn parse(id: &SymbolId) -> Result<Self, IntelligenceError> {
        let (path, name) = id
            .as_str()
            .strip_prefix("symbol:")
            .and_then(|rest| rest.rsplit_once('#'))
            .ok_or_else(|| {
                // Named rather than rejected as malformed: the caller almost
                // always passed the symbol's *name*, and the fix is to look it
                // up first rather than to spell the id differently.
                IntelligenceError::Protocol(format!(
                    "`{}` is not a symbol id; find one with code.symbol first, such as \
                     `symbol:src/lib.rs#run`",
                    id.as_str()
                ))
            })?;
        Ok(Self {
            path: PathBuf::from(path),
            name: name.to_owned(),
        })
    }
}

/// How much of a declaration `explain_symbol` quotes back.
const MAX_DECLARATION_BYTES: usize = 4 * 1024;

impl<'a> GraphCodeIntelligence<'a> {
    /// Index the workspace now. The graph itself is incremental, but a fresh
    /// process has nothing to be incremental against.
    pub fn index(workspace: &'a Workspace) -> Result<Self, IntelligenceError> {
        let mut graph = crate::graph::KnowledgeGraph::new();
        graph
            .index(workspace)
            .map_err(|error| IntelligenceError::Protocol(error.to_string()))?;
        Ok(Self { workspace, graph })
    }

    /// The declaration's byte range, found by re-parsing the file the graph
    /// says it is in. The graph stores identity, not offsets: an offset goes
    /// stale on the next edit, and the parse that would refresh it is the same
    /// parse that answers this question.
    fn locate(
        &self,
        path: &Path,
        name: &str,
    ) -> Result<(SourceLocation, StateVersion), IntelligenceError> {
        let content = self.workspace.read(path, crate::graph::MAX_INDEXED_BYTES)?;
        let syntax = RustSyntax::new(content.bytes.clone())
            .map_err(|error| IntelligenceError::Protocol(error.to_string()))?;
        for kind in crate::graph::DECLARATIONS {
            let Ok(declarations) = syntax.declarations(kind) else {
                continue;
            };
            if let Some((node, _)) = declarations.into_iter().find(|(_, found)| found == name) {
                return Ok((
                    SourceLocation {
                        uri: format!("file:{}", slash(path)),
                        bytes: node.bytes,
                    },
                    content.digest,
                ));
            }
        }
        Err(IntelligenceError::Protocol(format!(
            "{name} is indexed in {} but no longer declared there",
            path.display()
        )))
    }
}

impl CodeIntelligence for GraphCodeIntelligence<'_> {
    fn find_symbol(&mut self, query: &SymbolQuery) -> Result<Vec<SymbolHit>, IntelligenceError> {
        if query.name.is_empty() || query.max_results == 0 {
            return Err(IntelligenceError::InvalidQuery);
        }
        let found: Vec<(PathBuf, String)> = self
            .graph
            .symbols(&query.name)
            .into_iter()
            .filter_map(|node| Some((node.path.clone()?, node.name.clone())))
            .take(query.max_results.min(MAX_SEMANTIC_RESULTS))
            .collect();
        found
            .into_iter()
            .map(|(path, name)| {
                let (location, revision) = self.locate(&path, &name)?;
                Ok(SymbolHit {
                    id: SymbolId::new(crate::graph::NodeId::symbol(&path, &name).to_string())?,
                    name,
                    location,
                    source_revision: revision,
                    provider: EvidenceProvider::Syntax,
                    // A grammar proves this is a declaration of that name; it
                    // does not prove it is the one the caller meant.
                    confidence_basis_points: 6_000,
                })
            })
            .collect()
    }

    fn explain_symbol(&mut self, id: &SymbolId) -> Result<SymbolEvidence, IntelligenceError> {
        let symbol = GraphSymbol::parse(id)?;
        let node = self
            .graph
            .node(&crate::graph::NodeId::symbol(&symbol.path, &symbol.name))
            .ok_or(IntelligenceError::InvalidSymbol)?;
        let declaration = node.declaration.clone().unwrap_or_default();
        let (location, revision) = self.locate(&symbol.path, &symbol.name)?;
        let content = self
            .workspace
            .read(&symbol.path, crate::graph::MAX_INDEXED_BYTES)?;
        let text = content
            .bytes
            .get(location.bytes.clone())
            .map(|slice| String::from_utf8_lossy(slice).into_owned())
            .unwrap_or_default();
        let mut summary = format!("{declaration} {}\n", symbol.name);
        summary.push_str(&text[..text.floor_char_boundary(MAX_DECLARATION_BYTES)]);
        Ok(SymbolEvidence {
            symbol: id.clone(),
            summary,
            citations: vec![location],
            source_revision: revision,
            provider: EvidenceProvider::Syntax,
            confidence_basis_points: 6_000,
        })
    }

    /// The files that import this symbol's module.
    ///
    /// Not callers: an import proves a file can reach the symbol, not that it
    /// uses it. Reported at low confidence rather than withheld, because "these
    /// eight files could be affected" is the answer a change needs and the text
    /// tier cannot give it at all.
    fn find_callers(&mut self, id: &SymbolId) -> Result<ReferenceGraph, IntelligenceError> {
        let symbol = GraphSymbol::parse(id)?;
        let importers: Vec<(PathBuf, String)> = self
            .graph
            .importers_of(&symbol.path)
            .into_iter()
            .filter_map(|node| Some((node.path.clone()?, node.name.clone())))
            .collect();
        let truncated = importers.len() > MAX_SEMANTIC_RESULTS;
        let callers = importers
            .into_iter()
            .take(MAX_SEMANTIC_RESULTS)
            .map(|(path, name)| {
                let content = self
                    .workspace
                    .read(&path, crate::graph::MAX_INDEXED_BYTES)?;
                Ok(SymbolHit {
                    id: SymbolId::new(crate::graph::NodeId::file(&path).to_string())?,
                    name,
                    location: SourceLocation {
                        uri: format!("file:{}", slash(&path)),
                        bytes: 0..0,
                    },
                    source_revision: content.digest,
                    provider: EvidenceProvider::Syntax,
                    confidence_basis_points: 3_000,
                })
            })
            .collect::<Result<_, IntelligenceError>>()?;
        Ok(ReferenceGraph { callers, truncated })
    }

    fn diagnostics(&mut self, _scope: &str) -> Result<DiagnosticSet, IntelligenceError> {
        Err(IntelligenceError::Unsupported("diagnostics"))
    }

    /// Refused rather than approximated.
    ///
    /// A grammar can find every declaration of a name; it cannot tell which
    /// uses of that name bind to this declaration, and a rename that is wrong
    /// about that silently breaks the build somewhere the caller is not
    /// looking. A language server answers this question or nobody does.
    fn plan_rename(
        &mut self,
        _id: &SymbolId,
        _name: &str,
    ) -> Result<WorkspaceEditPlan, IntelligenceError> {
        Err(IntelligenceError::Unsupported("plan_rename"))
    }
}

fn slash(path: &Path) -> String {
    path.components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// Level-1 fallback used when no language server is configured.
pub struct TextCodeIntelligence<'a> {
    workspace: &'a Workspace,
}

impl<'a> TextCodeIntelligence<'a> {
    pub const fn new(workspace: &'a Workspace) -> Self {
        Self { workspace }
    }
}

impl CodeIntelligence for TextCodeIntelligence<'_> {
    fn find_symbol(&mut self, query: &SymbolQuery) -> Result<Vec<SymbolHit>, IntelligenceError> {
        if query.name.is_empty() || query.max_results == 0 {
            return Err(IntelligenceError::InvalidQuery);
        }
        self.workspace
            .search(&query.name, query.max_results, 100_000, 16 * 1024 * 1024)?
            .hits
            .into_iter()
            .map(|found| {
                let path = found.resource.value();
                let content = self.workspace.resolve_file(path)?.read(16 * 1024 * 1024)?;
                let start = found.text.find(&query.name).unwrap_or_default();
                let raw = RawSymbol {
                    name: query.name.clone(),
                    uri: format!("file:{path}"),
                    start,
                    end: start + query.name.len(),
                };
                hit(raw, content.digest, EvidenceProvider::Text, 4_000)
            })
            .collect()
    }

    fn explain_symbol(&mut self, _id: &SymbolId) -> Result<SymbolEvidence, IntelligenceError> {
        Err(IntelligenceError::Unsupported("explain_symbol"))
    }

    fn find_callers(&mut self, _id: &SymbolId) -> Result<ReferenceGraph, IntelligenceError> {
        Err(IntelligenceError::Unsupported("find_callers"))
    }

    fn diagnostics(&mut self, _scope: &str) -> Result<DiagnosticSet, IntelligenceError> {
        Err(IntelligenceError::Unsupported("diagnostics"))
    }

    fn plan_rename(
        &mut self,
        _id: &SymbolId,
        _name: &str,
    ) -> Result<WorkspaceEditPlan, IntelligenceError> {
        Err(IntelligenceError::Unsupported("plan_rename"))
    }
}

/// A symbol location before it becomes a [`SymbolHit`]: the text tier's own
/// shape, since it finds a string rather than being told about a symbol.
struct RawSymbol {
    name: String,
    uri: String,
    start: usize,
    end: usize,
}

fn hit(
    raw: RawSymbol,
    revision: StateVersion,
    provider: EvidenceProvider,
    confidence: u16,
) -> Result<SymbolHit, IntelligenceError> {
    if raw.name.is_empty() || raw.uri.is_empty() || raw.start > raw.end {
        return Err(IntelligenceError::Protocol(
            "invalid symbol location".into(),
        ));
    }
    let identity =
        Sha256::digest(format!("{}\0{}\0{}\0{}", raw.name, raw.uri, raw.start, raw.end).as_bytes());
    Ok(SymbolHit {
        id: SymbolId::new(StateVersion::from_digest(identity.into()).to_string())?,
        name: raw.name,
        location: SourceLocation {
            uri: raw.uri,
            bytes: raw.start..raw.end,
        },
        source_revision: revision,
        provider,
        confidence_basis_points: confidence,
    })
}

#[derive(Debug)]
pub enum IntelligenceError {
    InvalidQuery,
    InvalidSymbol,
    FanoutExceeded,
    ResultTooLarge,
    Unsupported(&'static str),
    Protocol(String),
    Lsp(LspError),
    Search(crate::search::SearchError),
    Resolve(crate::resource::ResolveError),
    Io(std::io::Error),
}

impl fmt::Display for IntelligenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQuery => formatter.write_str("semantic query is empty or unbounded"),
            Self::InvalidSymbol => formatter.write_str("symbol id is empty or too large"),
            Self::FanoutExceeded => formatter.write_str("semantic request fan-out exceeds its cap"),
            Self::ResultTooLarge => formatter.write_str("semantic result exceeds its cap"),
            Self::Unsupported(operation) => write!(formatter, "{operation} needs an LSP provider"),
            Self::Protocol(error) => write!(formatter, "semantic protocol error: {error}"),
            Self::Lsp(error) => error.fmt(formatter),
            Self::Search(error) => error.fmt(formatter),
            Self::Resolve(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for IntelligenceError {}

impl From<LspError> for IntelligenceError {
    fn from(value: LspError) -> Self {
        Self::Lsp(value)
    }
}

impl From<crate::search::SearchError> for IntelligenceError {
    fn from(value: crate::search::SearchError) -> Self {
        Self::Search(value)
    }
}

impl From<crate::resource::ResolveError> for IntelligenceError {
    fn from(value: crate::resource::ResolveError) -> Self {
        Self::Resolve(value)
    }
}

impl From<std::io::Error> for IntelligenceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{CommandOrigin, RestartPolicy, ServerCommand};

    /// A server that answers from a script and records what it was asked, so a
    /// test can assert on the requests as well as on what came back.
    #[derive(Default)]
    struct Fake {
        answers: Vec<Value>,
        asked: std::sync::Arc<std::sync::Mutex<Vec<LspRequest>>>,
        notified: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        capabilities: Value,
    }

    impl LspTransport for Fake {
        fn start(&mut self, _command: &ServerCommand) -> Result<(), LspError> {
            Ok(())
        }

        fn stop(&mut self) {}

        fn request_batch(&mut self, requests: &[LspRequest]) -> Result<Vec<Value>, LspError> {
            assert!(requests.len() <= MAX_LSP_BATCH);
            self.asked.lock().unwrap().extend(requests.iter().cloned());
            Ok(self.answers.drain(..requests.len()).collect())
        }

        fn notify(&mut self, method: &str, _params: Value) -> Result<(), LspError> {
            self.notified.lock().unwrap().push(method.to_owned());
            Ok(())
        }

        fn capabilities(&self) -> Value {
            self.capabilities.clone()
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        workspace: Workspace,
        asked: std::sync::Arc<std::sync::Mutex<Vec<LspRequest>>>,
        notified: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    const SOURCE: &str = "struct Thing;\nfn café(x: Thing) -> Thing {\n    x\n}\n";

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        std::fs::write(directory.path().join("src/a.rs"), SOURCE).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        Fixture {
            _directory: directory,
            workspace,
            asked: std::sync::Arc::default(),
            notified: std::sync::Arc::default(),
        }
    }

    fn client<'a>(
        fixture: &'a Fixture,
        answers: Vec<Value>,
        capabilities: Value,
    ) -> LspCodeIntelligence<'a, Fake> {
        let host = LspHost::new(
            ServerCommand {
                argv: vec!["fake".into()],
                origin: CommandOrigin::Installed,
                policy_authorized: false,
            },
            Fake {
                answers,
                asked: std::sync::Arc::clone(&fixture.asked),
                notified: std::sync::Arc::clone(&fixture.notified),
                capabilities,
            },
            RestartPolicy { delays: vec![] },
        );
        LspCodeIntelligence::new(
            "rust-analyzer",
            StateVersion::from_digest([7; 32]),
            host,
            &fixture.workspace,
        )
    }

    fn uri(fixture: &Fixture) -> String {
        crate::lsp::file_uri(&fixture.workspace.path().join("src/a.rs"))
    }

    /// `{"line": l, "character": c}` twice, as LSP writes a range.
    fn range(start: (u64, u64), end: (u64, u64)) -> Value {
        json!({
            "start": {"line": start.0, "character": start.1},
            "end": {"line": end.0, "character": end.1},
        })
    }

    #[test]
    fn a_symbol_is_addressed_by_position_and_reported_in_bytes() {
        let fixture = fixture();
        let uri = uri(&fixture);
        let mut client = client(
            &fixture,
            vec![json!([
                {"name": "Thing", "kind": 23, "location": {"uri": uri, "range": range((0, 7), (0, 12))}},
                // A server may answer a query with near matches; only the name
                // that was asked for is a hit.
                {"name": "Thingy", "kind": 23, "location": {"uri": uri, "range": range((9, 0), (9, 6))}}
            ])],
            Value::Null,
        );

        let hits = client
            .find_symbol(&SymbolQuery {
                name: "Thing".into(),
                max_results: 8,
            })
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, EvidenceProvider::Lsp);
        assert_eq!(hits[0].source_revision, StateVersion::from_digest([7; 32]));
        // `struct Thing;` — the name starts at byte 7 and ends at 12.
        assert_eq!(hits[0].location.bytes, 7..12);
        // The id carries the position, so a follow-up call can address it.
        assert_eq!(hits[0].id.as_str(), format!("lsp:{uri}#0:7"));
    }

    #[test]
    fn a_character_offset_is_utf16_unless_the_server_agreed_otherwise() {
        let fixture = fixture();
        let uri = uri(&fixture);
        // Line 1 is `fn café(x: Thing) -> Thing {`. `é` is two bytes and one
        // UTF-16 unit, so the byte offset of `(` differs by one between the
        // encodings — which is exactly the bug this conversion exists to avoid.
        let utf16 = client(
            &fixture,
            vec![
                json!([{"name": "café", "location": {"uri": uri, "range": range((1, 3), (1, 7))}}]),
            ],
            Value::Null,
        )
        .find_symbol(&SymbolQuery {
            name: "café".into(),
            max_results: 1,
        })
        .unwrap();
        assert_eq!(&SOURCE[utf16[0].location.bytes.clone()], "café");

        let utf8 = client(
            &fixture,
            vec![
                json!([{"name": "café", "location": {"uri": uri, "range": range((1, 3), (1, 8))}}]),
            ],
            json!({"positionEncoding": "utf-8"}),
        )
        .find_symbol(&SymbolQuery {
            name: "café".into(),
            max_results: 1,
        })
        .unwrap();
        assert_eq!(&SOURCE[utf8[0].location.bytes.clone()], "café");
    }

    #[test]
    fn a_file_is_announced_once_before_it_is_asked_about() {
        let fixture = fixture();
        let uri = uri(&fixture);
        let mut client = client(
            &fixture,
            vec![
                json!({"contents": {"kind": "markdown", "value": "struct Thing"}, "range": range((0, 7), (0, 12))}),
                json!([{"uri": uri, "range": range((1, 11), (1, 16))}]),
            ],
            Value::Null,
        );
        let id = SymbolId::new(format!("lsp:{uri}#0:7")).unwrap();

        let evidence = client.explain_symbol(&id).unwrap();
        assert_eq!(evidence.summary, "struct Thing");
        assert_eq!(evidence.citations[0].bytes, 7..12);

        let references = client.find_callers(&id).unwrap();
        assert_eq!(references.callers.len(), 1);
        assert_eq!(references.callers[0].name, "a.rs");

        // Opened once, however many questions were asked about it.
        let notified = fixture.notified.lock().unwrap().clone();
        assert_eq!(
            notified
                .iter()
                .filter(|method| *method == "textDocument/didOpen")
                .count(),
            1,
            "{notified:?}"
        );
        // References exclude the declaration: a rename needs them, but "what
        // would break" does not include the definition itself.
        let asked = fixture.asked.lock().unwrap().clone();
        let references = asked
            .iter()
            .find(|request| request.method == "textDocument/references")
            .expect("references were requested");
        assert_eq!(references.params["context"]["includeDeclaration"], false);
    }

    #[test]
    fn a_rename_plan_is_bound_to_its_revision_and_ordered_for_applying() {
        let fixture = fixture();
        let uri = uri(&fixture);
        let mut client = client(
            &fixture,
            vec![json!({"changes": {
                uri.clone(): [
                    {"range": range((0, 7), (0, 12)), "newText": "Widget"},
                    {"range": range((1, 11), (1, 16)), "newText": "Widget"},
                ]
            }})],
            Value::Null,
        );

        let plan = client
            .plan_rename(&SymbolId::new(format!("lsp:{uri}#0:7")).unwrap(), "Widget")
            .unwrap();

        assert_eq!(plan.server, "rust-analyzer");
        // The revision binds the plan to the text its offsets were computed
        // against, not to the tier's own version stamp: that is what lets an
        // applier tell a fresh plan from one whose file has since moved.
        assert_eq!(
            plan.revision,
            document_revision([(uri.as_str(), SOURCE)]),
            "the plan is bound to the document it was planned against"
        );
        assert_ne!(plan.revision, StateVersion::from_digest([7; 32]));
        assert_eq!(plan.edits.len(), 2);
        // Descending within a file, so applying one does not move the next.
        assert!(plan.edits[0].bytes.start > plan.edits[1].bytes.start);
        assert_eq!(&SOURCE[plan.edits[1].bytes.clone()], "Thing");
    }

    #[test]
    fn a_rename_the_server_will_not_do_is_refused_rather_than_half_planned() {
        let fixture = fixture();
        let uri = uri(&fixture);
        let mut client = client(&fixture, vec![json!({})], Value::Null);

        let error = client
            .plan_rename(&SymbolId::new(format!("lsp:{uri}#0:7")).unwrap(), "Widget")
            .expect_err("no changes is not a plan");

        assert!(format!("{error}").contains("no edits"), "{error}");
    }

    #[test]
    fn an_id_from_another_tier_says_which_call_produces_a_usable_one() {
        let fixture = fixture();
        let mut client = client(&fixture, Vec::new(), Value::Null);

        let error = client
            .explain_symbol(&SymbolId::new("symbol:src/a.rs#Thing").unwrap())
            .expect_err("a graph-tier id is not an LSP address");

        assert!(format!("{error}").contains("code.symbol"), "{error}");
    }

    #[test]
    fn diagnostics_are_pulled_for_one_file_and_carry_their_severity() {
        let fixture = fixture();
        let mut client = client(
            &fixture,
            vec![json!({"kind": "full", "items": [
                {"range": range((2, 4), (2, 5)), "severity": 1, "message": "mismatched types"},
                {"range": range((1, 3), (1, 7)), "message": "unused"}
            ]})],
            Value::Null,
        );

        let found = client.diagnostics("src/a.rs").unwrap();

        assert_eq!(found.diagnostics.len(), 2);
        assert_eq!(found.diagnostics[0].severity, 1);
        assert_eq!(found.diagnostics[0].message, "mismatched types");
        assert_eq!(&SOURCE[found.diagnostics[0].location.bytes.clone()], "x");
        // A server that did not say is not reported as having said "hint".
        assert_eq!(found.diagnostics[1].severity, 0);
        assert!(!found.truncated);
    }

    #[test]
    fn a_path_survives_the_round_trip_through_a_file_uri() {
        for path in [
            "/repo/src/a.rs",
            "/repo/a file/b.rs",
            "/repo/100%/c.rs",
            "/repo/a#b/d.rs",
            "/repo/café/été.rs",
        ] {
            let uri = crate::lsp::file_uri(Path::new(path));
            assert!(uri.starts_with("file:///"), "{uri}");
            assert_eq!(crate::lsp::uri_path(&uri), PathBuf::from(path), "{uri}");
        }
    }

    /// Every URI decoded here came from a language server, and servers encode
    /// non-ASCII even where `file_uri` does not. Decoding an escape per `char`
    /// read those bytes as Latin-1, so a rename touching an accented path
    /// planned edits for a file it could then not read.
    #[test]
    fn a_server_encoded_path_decodes_to_the_bytes_it_named() {
        assert_eq!(
            crate::lsp::uri_path("file:///repo/caf%C3%A9/a.rs"),
            PathBuf::from("/repo/café/a.rs")
        );
        assert_eq!(
            crate::lsp::uri_path("file:///repo/%E6%97%A5%E6%9C%AC/b.rs"),
            PathBuf::from("/repo/日本/b.rs")
        );
        // A stray percent is not an escape and is kept as written.
        assert_eq!(
            crate::lsp::uri_path("file:///repo/100%/c.rs"),
            PathBuf::from("/repo/100%/c.rs")
        );
        assert_eq!(
            crate::lsp::uri_path("file:///repo/a%zz/d.rs"),
            PathBuf::from("/repo/a%zz/d.rs")
        );
    }

    /// An explanation quotes at most a few KiB of the declaration. The cut is
    /// in bytes, and a source with multi-byte text used to be cut through a
    /// character; each pad moves the cut to a different byte of one.
    #[test]
    fn a_long_declaration_is_quoted_up_to_a_character_boundary() {
        for pad in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(directory.path().join("src")).unwrap();
            std::fs::write(
                directory.path().join("src/big.rs"),
                format!(
                    "fn big() {{\n    // {}{}\n}}\n",
                    "x".repeat(pad),
                    "日".repeat(3_000)
                ),
            )
            .unwrap();
            let workspace = Workspace::open(directory.path()).unwrap();
            let evidence = GraphCodeIntelligence::index(&workspace)
                .unwrap()
                .explain_symbol(&SymbolId::new("symbol:src/big.rs#big").unwrap())
                .unwrap();
            assert!(evidence.summary.contains("日"), "{pad}");
        }
    }
}
