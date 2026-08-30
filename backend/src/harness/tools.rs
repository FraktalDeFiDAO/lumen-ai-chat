//! The tools the harness runs on the model's behalf.
//!
//! Every tool is provider-neutral: it declares a name, a description and a JSON
//! Schema, and the gateway translates that into whichever wire format the
//! selected provider speaks. Nothing here knows which provider is on the other
//! end, which is what makes one registry serve all of them.
//!
//! `run` is synchronous. Tools do file I/O and vector search, both blocking, so
//! the loop dispatches them through `spawn_blocking` rather than pretending
//! they are futures.

use std::{fmt::Write as _, path::Path, sync::Arc};

use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    ToolDef,
    harness::{
        memory::HarnessMemory,
        workspace::{MAX_FILE_BYTES, Workspace, WorkspaceError, walk_files},
    },
};

/// A tool result larger than this crowds out the conversation it informs.
const MAX_TOOL_OUTPUT_BYTES: usize = 32 * 1024;
const DEFAULT_READ_LINES: usize = 400;
const MAX_SEARCH_RESULTS: usize = 100;
const MAX_SEARCHED_FILES: usize = 5_000;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("{0}")]
    InvalidArguments(String),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("{path}: {message}")]
    Io { path: String, message: String },
    #[error("memory is not available for this workspace")]
    MemoryUnavailable,
    #[error("memory search failed: {0}")]
    Memory(String),
}

/// What a tool is allowed to touch.
pub struct ToolContext {
    pub workspace: Workspace,
    pub memory: Option<Arc<HarnessMemory>>,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    fn run(&self, arguments: &Value, context: &ToolContext) -> Result<String, ToolError>;

    /// Whether a successful run can have changed the workspace, and so needs
    /// the memory index refreshed.
    fn mutates(&self) -> bool {
        false
    }
}

/// The tools offered for one run.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// The built-in set, narrowed to what this context can actually serve.
    ///
    /// A read-only workspace does not advertise `write_file`, and a harness
    /// without an index does not advertise `search_memory`. Offering a tool
    /// that can only fail teaches the model to distrust the whole set.
    pub fn builtin(context: &ToolContext) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ReadFile),
            Arc::new(ListDirectory),
            Arc::new(SearchText),
        ];
        if context.memory.is_some() {
            tools.push(Arc::new(SearchMemory));
        }
        if context.workspace.allows_writes() {
            tools.push(Arc::new(WriteFile));
            tools.push(Arc::new(EditFile));
        }
        Self { tools }
    }

    pub fn definitions(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|tool| ToolDef {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(Arc::clone)
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Read a string argument, refusing the wrong type rather than coercing it.
fn required_str<'a>(arguments: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ToolError::InvalidArguments(format!("`{key}` is required and must be a string"))
        })
}

fn optional_usize(arguments: &Value, key: &str) -> Option<usize> {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

fn io_error(workspace: &Workspace, path: &Path, error: &std::io::Error) -> ToolError {
    ToolError::Io {
        path: workspace.display_path(path),
        message: error.to_string(),
    }
}

/// Cut a tool result to the budget, saying so rather than trailing off.
fn truncate(mut output: String) -> String {
    if output.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output;
    }
    let mut cut = MAX_TOOL_OUTPUT_BYTES;
    while cut > 0 && !output.is_char_boundary(cut) {
        cut -= 1;
    }
    output.truncate(cut);
    output.push_str("\n… output truncated; narrow the request to see the rest.");
    output
}

/// Bring the index back in line after a mutating tool succeeds.
///
/// Without this the store keeps serving the body a file had *before* the agent
/// edited it, which is worse than having no index at all: the agent reads its
/// own stale work and concludes the edit did not land.
///
/// Best-effort by design. A failed re-index degrades retrieval; failing the
/// tool call would discard an edit that already succeeded on disk.
pub fn reindex_after(context: &ToolContext, arguments: &Value) {
    let Some(memory) = &context.memory else {
        return;
    };
    let Some(path) = arguments.get("path").and_then(Value::as_str) else {
        return;
    };
    let Ok(absolute) = context.workspace.resolve(path) else {
        return;
    };
    if let Err(error) = memory.index_file(&absolute) {
        tracing::warn!(path = %path, %error, "failed to re-index an edited file");
    }
}

struct ReadFile;

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from the workspace. Returns numbered lines. Use `offset` and `limit` to page through a long file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative path to the file."},
                "offset": {"type": "integer", "minimum": 1, "description": "First line to return, 1-based."},
                "limit": {"type": "integer", "minimum": 1, "description": "How many lines to return."},
            },
            "required": ["path"],
        })
    }

    fn run(&self, arguments: &Value, context: &ToolContext) -> Result<String, ToolError> {
        let workspace = &context.workspace;
        let path = workspace.resolve(required_str(arguments, "path")?)?;
        let metadata =
            std::fs::metadata(&path).map_err(|error| io_error(workspace, &path, &error))?;
        if metadata.is_dir() {
            return Err(ToolError::InvalidArguments(format!(
                "{} is a directory; use list_directory",
                workspace.display_path(&path)
            )));
        }
        if metadata.len() as usize > MAX_FILE_BYTES {
            return Err(ToolError::InvalidArguments(format!(
                "{} is {} bytes, over the {MAX_FILE_BYTES} byte limit; read it in pages with offset and limit",
                workspace.display_path(&path),
                metadata.len()
            )));
        }
        let source =
            std::fs::read_to_string(&path).map_err(|error| io_error(workspace, &path, &error))?;

        let offset = optional_usize(arguments, "offset").unwrap_or(1).max(1);
        let limit = optional_usize(arguments, "limit").unwrap_or(DEFAULT_READ_LINES);
        let lines: Vec<&str> = source.lines().collect();
        if offset > lines.len() {
            return Ok(format!(
                "{} has {} lines; offset {offset} is past the end.",
                workspace.display_path(&path),
                lines.len()
            ));
        }

        let end = offset.saturating_add(limit).min(lines.len() + 1);
        let mut output = String::new();
        for (number, line) in (offset..end).zip(&lines[offset - 1..end - 1]) {
            let _ = writeln!(output, "{number:>6}| {line}");
        }
        if end <= lines.len() {
            let _ = writeln!(
                output,
                "… {} more lines; continue at offset {end}.",
                lines.len() - (end - 1)
            );
        }
        Ok(truncate(output))
    }
}

struct ListDirectory;

impl Tool for ListDirectory {
    fn name(&self) -> &'static str {
        "list_directory"
    }

    fn description(&self) -> &'static str {
        "List the entries of a workspace directory. Omit `path` for the workspace root. Directories end in `/`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative directory. Defaults to the root."},
            },
        })
    }

    fn run(&self, arguments: &Value, context: &ToolContext) -> Result<String, ToolError> {
        let workspace = &context.workspace;
        let path = match arguments.get("path").and_then(Value::as_str) {
            Some(requested) if !requested.trim().is_empty() => workspace.resolve(requested)?,
            _ => workspace.root().to_path_buf(),
        };
        let entries =
            std::fs::read_dir(&path).map_err(|error| io_error(workspace, &path, &error))?;

        let mut rows: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                // The same exclusion policy the resolver enforces, so a listing
                // never advertises a path a read would refuse.
                if crate::coders::excluded_workspace_entry(&name) {
                    return None;
                }
                let metadata = entry.metadata().ok()?;
                Some(if metadata.is_dir() {
                    format!("{name}/")
                } else {
                    format!("{name} ({} bytes)", metadata.len())
                })
            })
            .collect();
        rows.sort();

        if rows.is_empty() {
            return Ok(format!("{} is empty.", workspace.display_path(&path)));
        }
        Ok(truncate(format!(
            "{}:\n{}",
            workspace.display_path(&path),
            rows.join("\n")
        )))
    }
}

struct SearchText;

impl Tool for SearchText {
    fn name(&self) -> &'static str {
        "search_text"
    }

    fn description(&self) -> &'static str {
        "Find a literal, case-insensitive string across workspace files. Returns `path:line: text`. Use this when you know the exact string; use search_memory when you know roughly what the code is called but not where it is."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Literal text to find. Not a regular expression."},
                "path": {"type": "string", "description": "Workspace-relative directory to search. Defaults to the root."},
                "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_RESULTS},
            },
            "required": ["pattern"],
        })
    }

    fn run(&self, arguments: &Value, context: &ToolContext) -> Result<String, ToolError> {
        let workspace = &context.workspace;
        let pattern = required_str(arguments, "pattern")?.to_lowercase();
        let root = match arguments.get("path").and_then(Value::as_str) {
            Some(requested) if !requested.trim().is_empty() => workspace.resolve(requested)?,
            _ => workspace.root().to_path_buf(),
        };
        let limit = optional_usize(arguments, "max_results")
            .unwrap_or(50)
            .clamp(1, MAX_SEARCH_RESULTS);

        let mut hits = Vec::new();
        let mut scanned = 0usize;
        for file in walk_files(&root, MAX_SEARCHED_FILES) {
            if hits.len() >= limit {
                break;
            }
            let Ok(source) = std::fs::read_to_string(&file) else {
                continue; // Binary, or unreadable: neither is a search result.
            };
            scanned += 1;
            for (number, line) in source.lines().enumerate() {
                if hits.len() >= limit {
                    break;
                }
                if line.to_lowercase().contains(&pattern) {
                    hits.push(format!(
                        "{}:{}: {}",
                        workspace.display_path(&file),
                        number + 1,
                        line.trim().chars().take(200).collect::<String>()
                    ));
                }
            }
        }

        if hits.is_empty() {
            return Ok(format!(
                "No match for `{}` in {} files under {}.",
                required_str(arguments, "pattern")?,
                scanned,
                workspace.display_path(&root)
            ));
        }
        let capped = hits.len() >= limit;
        let mut output = hits.join("\n");
        if capped {
            output.push_str(
                "\n… stopped at the result limit; narrow the pattern or raise max_results.",
            );
        }
        Ok(truncate(output))
    }
}

struct SearchMemory;

impl Tool for SearchMemory {
    fn name(&self) -> &'static str {
        "search_memory"
    }

    fn description(&self) -> &'static str {
        "Search the indexed codebase for functions, types and sections. Ranking combines word overlap with exact symbol-name matches, so use words that appear in the code itself -- identifiers, type names, the project's own vocabulary -- rather than a paraphrase of what the code does. Returns each match with its file and line range."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What you are looking for, in words. Identifiers also work."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 25, "description": "How many results to return. Defaults to 8."},
            },
            "required": ["query"],
        })
    }

    fn run(&self, arguments: &Value, context: &ToolContext) -> Result<String, ToolError> {
        let memory = context
            .memory
            .as_ref()
            .ok_or(ToolError::MemoryUnavailable)?;
        let query = required_str(arguments, "query")?;
        let limit = optional_usize(arguments, "limit").unwrap_or(8).clamp(1, 25);

        let hits = memory
            .search(query, limit)
            .map_err(|error| ToolError::Memory(error.to_string()))?;
        if hits.is_empty() {
            return Ok(format!(
                "Nothing indexed matches `{query}`. The index holds {} records; try search_text for an exact identifier.",
                memory.live_count()
            ));
        }
        Ok(truncate(render_hits(&hits)))
    }
}

/// Render search results so the model can act on them without another call:
/// where it is, what it is, and enough of the body to judge relevance.
fn render_hits(hits: &[harness_memory::memory::SearchHit]) -> String {
    let mut output = String::new();
    for hit in hits {
        let symbol = &hit.record.symbol;
        let _ = writeln!(
            output,
            "{}:{}-{}  {} {}",
            symbol.file_path.display(),
            symbol.start_line,
            symbol.end_line,
            symbol.kind,
            symbol.name
        );
        if let Some(doc) = &symbol.doc {
            for line in doc.lines().take(3) {
                let _ = writeln!(output, "    {}", line.trim());
            }
        }
        for line in hit.record.content.lines().take(12) {
            let _ = writeln!(output, "    {line}");
        }
        output.push('\n');
    }
    output
}

struct WriteFile;

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create a workspace file or replace its entire contents. Parent directories are created. To change part of a file, prefer edit_file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative path to write."},
                "content": {"type": "string", "description": "The complete new contents of the file."},
            },
            "required": ["path", "content"],
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn run(&self, arguments: &Value, context: &ToolContext) -> Result<String, ToolError> {
        let workspace = &context.workspace;
        let path = workspace.resolve_for_write(required_str(arguments, "path")?)?;
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidArguments("`content` is required and must be a string".into())
            })?;
        if content.len() > MAX_FILE_BYTES {
            return Err(ToolError::InvalidArguments(format!(
                "content is {} bytes, over the {MAX_FILE_BYTES} byte limit",
                content.len()
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| io_error(workspace, parent, &error))?;
        }
        std::fs::write(&path, content).map_err(|error| io_error(workspace, &path, &error))?;
        Ok(format!(
            "Wrote {} lines to {}.",
            content.lines().count(),
            workspace.display_path(&path)
        ))
    }
}

struct EditFile;

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace an exact string in a workspace file. `old_text` must appear exactly once unless `replace_all` is true, so include enough surrounding context to be unambiguous."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative path to edit."},
                "old_text": {"type": "string", "description": "Exact text to replace, including indentation."},
                "new_text": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring exactly one."},
            },
            "required": ["path", "old_text", "new_text"],
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn run(&self, arguments: &Value, context: &ToolContext) -> Result<String, ToolError> {
        let workspace = &context.workspace;
        let path = workspace.resolve_for_write(required_str(arguments, "path")?)?;
        let old_text = required_str(arguments, "old_text")?;
        let new_text = arguments
            .get("new_text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidArguments("`new_text` is required and must be a string".into())
            })?;
        let replace_all = arguments
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let source =
            std::fs::read_to_string(&path).map_err(|error| io_error(workspace, &path, &error))?;
        let occurrences = source.matches(old_text).count();
        // An ambiguous edit is the failure mode that silently corrupts a file,
        // so it is refused rather than resolved by guessing which one was meant.
        let updated = match (occurrences, replace_all) {
            (0, _) => {
                return Err(ToolError::InvalidArguments(format!(
                    "`old_text` does not appear in {}",
                    workspace.display_path(&path)
                )));
            }
            (count, false) if count > 1 => {
                return Err(ToolError::InvalidArguments(format!(
                    "`old_text` appears {count} times in {}; add surrounding context or set replace_all",
                    workspace.display_path(&path)
                )));
            }
            (_, true) => source.replace(old_text, new_text),
            (_, false) => source.replacen(old_text, new_text, 1),
        };

        std::fs::write(&path, &updated).map_err(|error| io_error(workspace, &path, &error))?;
        Ok(format!(
            "Replaced {} occurrence{} in {}.",
            if replace_all { occurrences } else { 1 },
            if replace_all && occurrences != 1 {
                "s"
            } else {
                ""
            },
            workspace.display_path(&path)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(allow_writes: bool) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn route() {\n    resolve();\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
        let context = ToolContext {
            workspace: Workspace::new(dir.path(), allow_writes).unwrap(),
            memory: None,
        };
        (dir, context)
    }

    fn run(tool: &dyn Tool, arguments: Value, context: &ToolContext) -> Result<String, ToolError> {
        tool.run(&arguments, context)
    }

    #[test]
    fn read_file_numbers_its_lines() {
        let (_dir, context) = context(false);
        let output = run(&ReadFile, json!({"path": "src/lib.rs"}), &context).unwrap();
        assert!(output.starts_with("     1| pub fn route() {"), "{output}");
    }

    #[test]
    fn read_file_pages_a_long_file() {
        let (dir, context) = context(false);
        let body: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("long.txt"), body).unwrap();

        let output = run(
            &ReadFile,
            json!({"path": "long.txt", "offset": 10, "limit": 2}),
            &context,
        )
        .unwrap();
        assert!(output.contains("    10| line 10"), "{output}");
        assert!(output.contains("    11| line 11"), "{output}");
        assert!(!output.contains("line 12"), "{output}");
        assert!(output.contains("continue at offset 12"), "{output}");
    }

    #[test]
    fn tools_refuse_paths_the_workspace_refuses() {
        let (_dir, context) = context(true);
        assert!(matches!(
            run(&ReadFile, json!({"path": ".env"}), &context),
            Err(ToolError::Workspace(WorkspaceError::Forbidden(_)))
        ));
        assert!(matches!(
            run(
                &WriteFile,
                json!({"path": "../escape", "content": "x"}),
                &context
            ),
            Err(ToolError::Workspace(WorkspaceError::Escape(_)))
        ));
    }

    #[test]
    fn a_read_only_registry_does_not_advertise_writing() {
        let (_dir, read_only) = context(false);
        let names = ToolRegistry::builtin(&read_only).names();
        assert!(!names.contains(&"write_file"), "{names:?}");
        assert!(!names.contains(&"edit_file"), "{names:?}");
        assert!(names.contains(&"read_file"));
        // No index was opened, so nothing promises to search one.
        assert!(!names.contains(&"search_memory"), "{names:?}");

        let (_dir, writable) = context(true);
        let names = ToolRegistry::builtin(&writable).names();
        assert!(names.contains(&"write_file"), "{names:?}");
        assert!(names.contains(&"edit_file"), "{names:?}");
    }

    #[test]
    fn writing_is_refused_on_a_read_only_workspace() {
        let (_dir, context) = context(false);
        assert!(matches!(
            run(
                &WriteFile,
                json!({"path": "new.txt", "content": "x"}),
                &context
            ),
            Err(ToolError::Workspace(WorkspaceError::ReadOnly))
        ));
    }

    #[test]
    fn write_file_creates_parents() {
        let (dir, context) = context(true);
        run(
            &WriteFile,
            json!({"path": "a/b/c.txt", "content": "hello\n"}),
            &context,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "hello\n"
        );
    }

    /// The edit that silently corrupts a file is the ambiguous one.
    #[test]
    fn edit_file_refuses_an_ambiguous_match_and_applies_a_unique_one() {
        let (dir, context) = context(true);
        std::fs::write(dir.path().join("dup.txt"), "a\na\n").unwrap();
        assert!(matches!(
            run(
                &EditFile,
                json!({"path": "dup.txt", "old_text": "a", "new_text": "b"}),
                &context
            ),
            Err(ToolError::InvalidArguments(_))
        ));

        run(
            &EditFile,
            json!({"path": "dup.txt", "old_text": "a", "new_text": "b", "replace_all": true}),
            &context,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dup.txt")).unwrap(),
            "b\nb\n"
        );

        assert!(matches!(
            run(
                &EditFile,
                json!({"path": "dup.txt", "old_text": "missing", "new_text": "x"}),
                &context
            ),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[test]
    fn search_text_finds_a_literal_and_reports_a_miss() {
        let (_dir, context) = context(false);
        let output = run(&SearchText, json!({"pattern": "resolve"}), &context).unwrap();
        assert!(output.contains("src/lib.rs:2: resolve();"), "{output}");

        let output = run(&SearchText, json!({"pattern": "nowhere_at_all"}), &context).unwrap();
        assert!(output.starts_with("No match"), "{output}");
    }

    /// A listing that named a secret would be an invitation to read it.
    #[test]
    fn list_directory_hides_what_the_resolver_would_refuse() {
        let (_dir, context) = context(false);
        let output = run(&ListDirectory, json!({}), &context).unwrap();
        assert!(output.contains("src/"), "{output}");
        assert!(!output.contains(".env"), "{output}");
    }

    #[test]
    fn search_memory_says_so_when_there_is_no_index() {
        let (_dir, context) = context(false);
        assert!(matches!(
            run(&SearchMemory, json!({"query": "routing"}), &context),
            Err(ToolError::MemoryUnavailable)
        ));
    }

    #[test]
    fn missing_arguments_are_reported_rather_than_guessed() {
        let (_dir, context) = context(true);
        for (tool, arguments) in [
            (&ReadFile as &dyn Tool, json!({})),
            (&SearchText as &dyn Tool, json!({"pattern": ""})),
            (&WriteFile as &dyn Tool, json!({"path": "x.txt"})),
        ] {
            assert!(matches!(
                tool.run(&arguments, &context),
                Err(ToolError::InvalidArguments(_))
            ));
        }
    }
}
