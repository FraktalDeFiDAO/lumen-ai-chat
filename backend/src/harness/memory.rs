//! The bridge between the harness and `harness-memory`.
//!
//! The crate underneath stores rows and searches them; it has no opinion about
//! what a row *is*. This decides that: it walks a workspace, cuts each file into
//! records a coding agent would want back — a function, a type, a heading — and
//! keeps the index honest when a tool edits a file.
//!
//! Everything here is synchronous, matching the crate's design. Embedding and
//! graph insertion are CPU-bound, and `async fn` would invite callers to run
//! them on a reactor thread. The harness wraps these in `spawn_blocking`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use harness_memory::{
    embed::HashEmbedder,
    hnsw::HnswConfig,
    memory::{CodeSymbol, CodebaseMemory, MemoryError, Record, SearchHit},
};
use thiserror::Error;

use crate::harness::workspace::walk_files;

/// MiniLM-class width, so swapping `HashEmbedder` for a real model later does
/// not change the on-disk row format.
const EMBEDDING_DIMENSION: usize = 384;
/// A file larger than this is a data blob, not source a model will read.
const MAX_INDEXED_FILE_BYTES: usize = 512 * 1024;
/// Lines. A record longer than this stops being a retrieval unit.
const MAX_CHUNK_LINES: usize = 120;
/// Lines per window for files with no detectable definitions.
const PROSE_CHUNK_LINES: usize = 40;
const MAX_INDEXED_FILES: usize = 20_000;

#[derive(Debug, Error)]
pub enum HarnessMemoryError {
    #[error("memory store: {0}")]
    Store(#[from] MemoryError),
    #[error("memory directory {path} is unusable: {message}")]
    Directory { path: String, message: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct IndexStats {
    pub files: usize,
    pub records: usize,
    pub skipped: usize,
}

/// A codebase index the harness can query and keep current.
pub struct HarnessMemory {
    inner: CodebaseMemory,
    root: PathBuf,
    /// Where the index's own segments live. Their modification time is when the
    /// index was last written, which survives a restart -- an in-process stamp
    /// would not, and a service that had just come back would call every stale
    /// index fresh.
    store: PathBuf,
}

impl HarnessMemory {
    /// Open (or create) the index for `root`, storing its segments in `dir`.
    pub fn open(dir: &Path, root: &Path) -> Result<Self, HarnessMemoryError> {
        std::fs::create_dir_all(dir).map_err(|error| HarnessMemoryError::Directory {
            path: dir.display().to_string(),
            message: error.to_string(),
        })?;
        let embedder = Arc::new(HashEmbedder::new(EMBEDDING_DIMENSION));
        let inner = CodebaseMemory::open(dir, embedder, HnswConfig::default())?;
        Ok(Self {
            inner,
            root: root.to_path_buf(),
            store: dir.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the index still describes the tree.
    ///
    /// The harness reindexes what its own tools change, but a `git pull`, an
    /// editor, or anything else outside it is invisible -- and an index that has
    /// silently stopped matching answers with code that is no longer there.
    /// Comparing the newest modification time under the root against when the
    /// index was last written is cheap and catches every one of those.
    pub fn is_fresh(&self) -> bool {
        // Nothing indexed yet is empty rather than stale: saying stale would
        // send someone to rebuild what does not exist.
        if self.live_count() == 0 {
            return true;
        }
        let Some(written) = newest_mtime(&self.store, |_| true) else {
            return true;
        };
        newest_mtime(&self.root, indexable_extension).is_none_or(|changed| changed <= written)
    }

    pub fn live_count(&self) -> usize {
        self.inner.live_count()
    }

    /// Index every source file under the root, replacing what was there.
    pub fn index_workspace(&self) -> Result<IndexStats, HarnessMemoryError> {
        let files = walk_files(&self.root, MAX_INDEXED_FILES)
            .into_iter()
            .filter(|path| indexable_extension(path));

        let mut stats = IndexStats::default();
        for path in files {
            match self.index_file(&path)? {
                0 => stats.skipped += 1,
                records => {
                    stats.files += 1;
                    stats.records += records;
                }
            }
        }
        Ok(stats)
    }

    /// Re-index one file, tombstoning whatever it held before.
    ///
    /// Returns the number of records written; zero means the file was skipped
    /// (binary, oversized, or gone), and a deleted file leaves nothing behind.
    pub fn index_file(&self, absolute: &Path) -> Result<usize, HarnessMemoryError> {
        let key = self.key_for(absolute);
        let Some(source) = read_indexable(absolute) else {
            self.inner.invalidate_file(&key)?;
            return Ok(0);
        };
        let records = records_for(&key, &source);
        if records.is_empty() {
            self.inner.invalidate_file(&key)?;
            return Ok(0);
        }
        let written = records.len();
        self.inner.index_file(&key, records)?;
        Ok(written)
    }

    /// Drop everything indexed for a path, for a file the agent deleted.
    pub fn forget_file(&self, absolute: &Path) -> Result<usize, HarnessMemoryError> {
        Ok(self.inner.invalidate_file(self.key_for(absolute))?)
    }

    pub fn search(&self, query: &str, k: usize) -> Result<Vec<SearchHit>, HarnessMemoryError> {
        Ok(self.inner.query(query, k)?)
    }

    /// Rows are keyed by workspace-relative path, so the index survives the
    /// workspace being mounted somewhere else.
    fn key_for(&self, absolute: &Path) -> PathBuf {
        absolute
            .strip_prefix(&self.root)
            .unwrap_or(absolute)
            .to_path_buf()
    }
}

/// Read a file if it is text a model would benefit from reading.
fn read_indexable(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() as usize > MAX_INDEXED_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    // A NUL in the first block is the cheap, reliable binary test; UTF-8
    // validation then catches everything else.
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// The newest modification time under `root`, among files `wanted` accepts.
///
/// Over the workspace it uses the same traversal policy as indexing, so a file
/// the index ignores cannot make it look stale.
fn newest_mtime(root: &Path, wanted: impl Fn(&Path) -> bool) -> Option<SystemTime> {
    walk_files(root, MAX_INDEXED_FILES)
        .into_iter()
        .filter(|path| wanted(path))
        .filter_map(|path| std::fs::metadata(&path).ok()?.modified().ok())
        .max()
}

fn indexable_extension(path: &Path) -> bool {
    const INDEXABLE: [&str; 34] = [
        "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "vue", "svelte", "py", "go", "java", "kt",
        "rb", "php", "cs", "c", "h", "cc", "cpp", "hpp", "swift", "scala", "sh", "bash", "sql",
        "md", "mdx", "toml", "yaml", "yml", "json", "html", "css",
    ];
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| INDEXABLE.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Cut a file into records.
///
/// Definition-bearing files split on their definitions; everything else splits
/// into fixed windows. The fallback matters more than it looks: without it a
/// README or a schema is silently absent from the index, and the agent is told
/// the codebase does not mention something it documents on its front page.
pub fn records_for(path: &Path, source: &str) -> Vec<Record> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let definitions: Vec<(usize, Definition)> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| definition(line).map(|found| (index, found)))
        .collect();

    if definitions.is_empty() {
        return prose_records(path, &lines);
    }

    let mut records = Vec::with_capacity(definitions.len());
    for (position, (line_index, found)) in definitions.iter().enumerate() {
        // The record ends where the next definition at the same or an outer
        // indent begins; a nested one belongs to this body and also gets a
        // record of its own.
        let end = definitions[position + 1..]
            .iter()
            .find(|(_, next)| next.indent <= found.indent)
            .map(|(next_index, _)| *next_index)
            .unwrap_or(lines.len())
            .min(line_index + MAX_CHUNK_LINES);
        let start = doc_block_start(&lines, *line_index);
        let doc = (start < *line_index).then(|| lines[start..*line_index].join("\n"));

        records.push(Record {
            symbol: CodeSymbol {
                name: found.name.clone(),
                kind: found.kind.into(),
                signature: lines[*line_index].trim().to_string(),
                doc,
                file_path: path.to_path_buf(),
                start_line: start + 1,
                end_line: end.max(*line_index + 1),
            },
            content: lines[*line_index..end.max(*line_index + 1)].join("\n"),
        });
    }
    records
}

fn prose_records(path: &Path, lines: &[&str]) -> Vec<Record> {
    let label = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    lines
        .chunks(PROSE_CHUNK_LINES)
        .enumerate()
        .filter(|(_, chunk)| chunk.iter().any(|line| !line.trim().is_empty()))
        .map(|(window, chunk)| {
            let start = window * PROSE_CHUNK_LINES;
            Record {
                symbol: CodeSymbol {
                    // The path is part of the name so a query naming the file
                    // reaches the chunk through the symbol path too.
                    name: format!("{label}#{}", window + 1),
                    kind: "chunk".into(),
                    signature: chunk
                        .iter()
                        .find(|line| !line.trim().is_empty())
                        .map(|line| line.trim().to_string())
                        .unwrap_or_default(),
                    doc: None,
                    file_path: path.to_path_buf(),
                    start_line: start + 1,
                    end_line: start + chunk.len(),
                },
                content: chunk.join("\n"),
            }
        })
        .collect()
}

/// Walk back over the comment block and attributes attached to a definition.
fn doc_block_start(lines: &[&str], definition_line: usize) -> usize {
    let mut start = definition_line;
    while start > 0 {
        let candidate = lines[start - 1].trim();
        let attached = candidate.starts_with("///")
            || candidate.starts_with("//!")
            || candidate.starts_with("//")
            || candidate.starts_with("/*")
            || candidate.starts_with('*')
            || candidate.starts_with("*/")
            || candidate.starts_with('#')
            || candidate.starts_with('@')
            || candidate.starts_with("\"\"\"");
        if !attached {
            break;
        }
        start -= 1;
    }
    start
}

struct Definition {
    kind: &'static str,
    name: String,
    indent: usize,
}

/// Recognise a definition line across the languages this repo and its users
/// actually contain, without carrying a parser per language.
fn definition(line: &str) -> Option<Definition> {
    let indent = line.len() - line.trim_start().len();
    let mut rest = line.trim();
    if rest.is_empty() {
        return None;
    }

    // A Markdown heading is the definition of a section.
    if let Some(heading) = rest.strip_prefix('#')
        && (rest.starts_with("# ") || rest.starts_with("## ") || rest.starts_with("### "))
    {
        let name = heading.trim_start_matches('#').trim();
        return (!name.is_empty()).then(|| Definition {
            kind: "section",
            name: name.chars().take(80).collect(),
            indent,
        });
    }

    // Modifiers carry no meaning for retrieval; strip them and look again.
    loop {
        let stripped = [
            "pub(crate) ",
            "pub(super) ",
            "pub ",
            "export default ",
            "export ",
            "public ",
            "private ",
            "protected ",
            "internal ",
            "abstract ",
            "declare ",
            "async ",
            "unsafe ",
            "extern ",
            "static ",
            "final ",
            "override ",
            "open ",
            "suspend ",
            "default ",
        ]
        .into_iter()
        .find_map(|modifier| rest.strip_prefix(modifier));
        match stripped {
            Some(remainder) => rest = remainder.trim_start(),
            None => break,
        }
    }

    // The keyword is the leading run of identifier characters. Splitting on
    // whitespace instead would read `impl<'a> Reader<'a>` as a keyword of
    // `impl<'a>` and find nothing.
    let split = rest
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    let (keyword, tail) = rest.split_at(split);
    let kind = match keyword {
        "fn" | "func" | "def" | "function" | "sub" => "function",
        "struct" => "struct",
        "enum" => "enum",
        "trait" => "trait",
        "interface" => "interface",
        "class" => "class",
        "impl" => "impl",
        "type" => "type",
        "union" => "union",
        "mod" | "module" | "namespace" | "package" => "module",
        "record" => "record",
        // A binding is only a definition when it binds something callable;
        // `const MAX = 5` is noise, `const run = async () => {}` is a function.
        "const" | "let" | "var" => {
            if rest.contains("=>") || rest.contains("= function") || rest.contains("= class") {
                "function"
            } else {
                return None;
            }
        }
        _ => {
            // Method shorthand carries no keyword at all. Skipping it would
            // make every method of a TypeScript class or a Vue component
            // invisible to retrieval.
            return method_shorthand(rest).map(|name| Definition {
                kind: "function",
                name,
                indent,
            });
        }
    };

    let name = symbol_name(tail)?;
    Some(Definition { kind, name, indent })
}

/// Recognise `run(options) {` — a method with no keyword introducing it.
///
/// The risk here is call sites, which have the same shape. Two cheap tests
/// separate them: the parameter list must close on the same line, and it must
/// contain no string literal, which is what `describe('name', () => {` has and
/// a parameter list does not.
fn method_shorthand(line: &str) -> Option<String> {
    let trimmed = line.trim_end();
    if !trimmed.ends_with('{') {
        return None;
    }
    let open = trimmed.find('(')?;
    let close = trimmed.rfind(')')?;
    if close < open {
        return None;
    }
    let parameters = &trimmed[open + 1..close];
    if parameters.contains(['\'', '"', '`']) {
        return None;
    }

    let name = trimmed[..open].split_whitespace().last()?.to_string();
    const CONTROL: [&str; 10] = [
        "if", "for", "while", "switch", "catch", "return", "match", "with", "do", "else",
    ];
    let plausible = !name.is_empty()
        && !CONTROL.contains(&name.as_str())
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
        && name.chars().next().is_some_and(|c| !c.is_numeric());
    plausible.then_some(name)
}

/// Take the identifier a definition introduces.
///
/// Go puts a receiver between the keyword and the name (`func (s *Store) Get`),
/// and Rust's `impl` puts generics there (`impl<'a> Reader<'a>`), so both get
/// skipped before the name is read.
fn symbol_name(tail: &str) -> Option<String> {
    let mut rest = tail.trim_start();
    for (open, close) in [('(', ')'), ('<', '>')] {
        if rest.starts_with(open)
            && let Some(end) = rest.find(close)
        {
            rest = rest[end + 1..].trim_start();
        }
    }
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | ':' | '.'))
        .collect();
    let name = name.trim_end_matches(':').to_string();
    (!name.is_empty() && name.chars().next().is_some_and(|c| !c.is_numeric())).then_some(name)
}

#[cfg(test)]
mod staleness_tests {
    use super::*;

    fn memory(root: &Path, store: &Path) -> HarnessMemory {
        HarnessMemory::open(store, root).unwrap()
    }

    /// An index that has never been told a file changed will happily answer with
    /// code that is no longer there. The harness reindexes what its own tools
    /// touch, but a `git pull` or an editor is invisible to it -- and nothing
    /// said so.
    #[test]
    fn an_index_knows_when_the_tree_has_moved_on_without_it() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("notes.md"),
            "# Retrieval\nthe passphrase is hunter2\n",
        )
        .unwrap();

        let memory = memory(workspace.path(), store.path());
        memory.index_workspace().unwrap();
        assert!(memory.is_fresh(), "an index built just now is not stale");

        // Somebody else edits the tree: a pull, an editor, anything not a tool.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(
            workspace.path().join("notes.md"),
            "# Retrieval\nthe passphrase was rotated\n",
        )
        .unwrap();

        assert!(
            !memory.is_fresh(),
            "the index is answering from a file that has since changed"
        );

        memory.index_workspace().unwrap();
        assert!(memory.is_fresh(), "reindexing did not settle it");
    }

    #[test]
    fn a_new_file_makes_the_index_stale_too() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.md"), "# One\n").unwrap();

        let memory = memory(workspace.path(), store.path());
        memory.index_workspace().unwrap();
        assert!(memory.is_fresh());

        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(workspace.path().join("b.md"), "# Two\n").unwrap();
        assert!(
            !memory.is_fresh(),
            "a file nobody indexed left the index fresh"
        );
    }

    /// The reason freshness is read off the index files rather than remembered:
    /// a service that has just restarted has no memory of when it last indexed,
    /// and must not therefore call every stale index fresh.
    #[test]
    fn a_restarted_service_still_knows_the_index_is_stale() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.md"), "# One\n").unwrap();

        memory(workspace.path(), store.path())
            .index_workspace()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(workspace.path().join("a.md"), "# One, revised\n").unwrap();

        // A completely new handle, as a restart would build.
        let reopened = memory(workspace.path(), store.path());
        assert!(
            !reopened.is_fresh(),
            "a reopened index forgot that the tree had moved on"
        );
    }

    /// An index nobody has built yet is not stale, it is empty, and saying
    /// "stale" would send a user to rebuild something that does not exist.
    #[test]
    fn an_index_that_was_never_built_is_not_called_stale() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.md"), "# One\n").unwrap();

        assert!(memory(workspace.path(), store.path()).is_fresh());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> Vec<(String, String)> {
        records_for(Path::new("sample.rs"), source)
            .into_iter()
            .map(|record| (record.symbol.kind, record.symbol.name))
            .collect()
    }

    #[test]
    fn recognises_definitions_across_languages() {
        for (source, kind, name) in [
            (
                "pub async fn open_upstream(state: &AppState) {",
                "function",
                "open_upstream",
            ),
            (
                "func (s *Store) Append(v []byte) error {",
                "function",
                "Append",
            ),
            ("    def index_file(self, path):", "function", "index_file"),
            ("export default class ChatStore {", "class", "ChatStore"),
            ("export const useChat = () => {", "function", "useChat"),
            ("interface ToolDef {", "interface", "ToolDef"),
            ("impl<'a> Reader<'a> {", "impl", "Reader"),
            ("  get(key) {", "function", "get"),
            ("  async handleSubmit(event) {", "function", "handleSubmit"),
            ("## Provider routing", "section", "Provider routing"),
        ] {
            let found = definition(source).unwrap_or_else(|| panic!("no definition in {source}"));
            assert_eq!((found.kind, found.name.as_str()), (kind, name), "{source}");
        }
    }

    /// Method shorthand and a call site have the same shape, and only one of
    /// them is a definition.
    #[test]
    fn a_call_site_is_not_mistaken_for_a_method() {
        for line in [
            "describe('routes tool calls', () => {",
            "  if (response.ok) {",
            "  } else if (retries > 0) {",
            "  } catch (error) {",
            "  for (const message of messages) {",
            "  const merged = Object.assign(target, {",
        ] {
            assert!(definition(line).is_none(), "{line} is not a definition");
        }
    }

    /// A constant is not a definition worth its own row; a bound closure is.
    #[test]
    fn ignores_bindings_that_are_not_callable() {
        assert!(definition("const MAX_MESSAGES = 100;").is_none());
        assert!(definition("let total = 0;").is_none());
        assert!(definition("const run = async () => {").is_some());
    }

    #[test]
    fn a_record_carries_its_doc_comment_and_body() {
        let source = "/// Opens the store.\n/// Twice.\npub fn open() {\n    body();\n}\n\npub fn close() {}\n";
        let records = records_for(Path::new("store.rs"), source);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].symbol.name, "open");
        assert_eq!(
            records[0].symbol.doc.as_deref(),
            Some("/// Opens the store.\n/// Twice.")
        );
        assert!(records[0].content.contains("body();"));
        // The doc block is where the record starts, not the signature line.
        assert_eq!(records[0].symbol.start_line, 1);
    }

    #[test]
    fn a_nested_definition_gets_its_own_record_without_truncating_the_outer_one() {
        let source = "class Store {\n  get(key) {\n    return 1;\n  }\n}\nclass Other {}\n";
        assert_eq!(
            names(source),
            vec![
                ("class".to_string(), "Store".to_string()),
                ("function".to_string(), "get".to_string()),
                ("class".to_string(), "Other".to_string()),
            ]
        );
        let records = records_for(Path::new("store.js"), source);
        assert!(
            records[0].content.contains("return 1;"),
            "outer keeps its body"
        );
    }

    /// Without this a README is invisible to retrieval, and the agent concludes
    /// the codebase never mentions what its front page documents.
    #[test]
    fn a_file_with_no_definitions_still_produces_records() {
        let source =
            "Lumen routes chat through native coder CLIs.\n\nIt speaks four provider protocols.\n";
        let records = records_for(Path::new("NOTES.txt"), source);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].symbol.kind, "chunk");
        assert_eq!(records[0].symbol.name, "NOTES.txt#1");
        assert!(records[0].content.contains("four provider protocols"));
    }

    #[test]
    fn an_empty_or_blank_file_produces_nothing() {
        assert!(records_for(Path::new("empty.md"), "").is_empty());
        assert!(records_for(Path::new("blank.md"), "\n\n  \n").is_empty());
    }
}
