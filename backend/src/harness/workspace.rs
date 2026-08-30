//! The filesystem boundary every harness tool runs behind.
//!
//! Tools take paths from a model, which is to say from whatever ended up in the
//! transcript. Nothing here trusts them: a path is normalised into the root,
//! checked component by component against the same secret-file policy the CLI
//! coders use, and then re-checked after resolving symlinks, because a link is
//! the one way a syntactically-clean path can still land outside.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::coders::excluded_workspace_entry;

/// A single file read has to fit in a message, and a message has a budget.
pub const MAX_FILE_BYTES: usize = 256 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WorkspaceError {
    #[error("path escapes the workspace: {0}")]
    Escape(String),
    #[error("path is not permitted: {0}")]
    Forbidden(String),
    #[error("workspace root is unavailable: {0}")]
    Root(String),
    #[error("the workspace is read-only; enable AI_CODER_ALLOW_WRITES to modify files")]
    ReadOnly,
}

/// A canonical root plus the write permission that applies inside it.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    allow_writes: bool,
}

impl Workspace {
    pub fn new(root: &Path, allow_writes: bool) -> Result<Self, WorkspaceError> {
        let root = std::fs::canonicalize(root)
            .map_err(|error| WorkspaceError::Root(format!("{}: {error}", root.display())))?;
        Ok(Self { root, allow_writes })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn allows_writes(&self) -> bool {
        self.allow_writes
    }

    /// Resolve a caller-supplied path to an absolute one inside the root.
    ///
    /// Accepts a workspace-relative path or an absolute path already inside the
    /// root; the second form exists because a model that has seen absolute
    /// paths in tool output will hand them back.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, WorkspaceError> {
        let trimmed = requested.trim();
        if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            return Err(WorkspaceError::Forbidden(display(requested)));
        }

        let candidate = Path::new(trimmed);
        let mut resolved = self.root.clone();
        let components: Vec<Component<'_>> = if candidate.is_absolute() {
            let relative = candidate
                .strip_prefix(&self.root)
                .map_err(|_| WorkspaceError::Escape(display(trimmed)))?;
            relative.components().collect()
        } else {
            candidate.components().collect()
        };

        for component in components {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    // A `..` that would climb past the root is an escape, not a
                    // no-op: silently clamping it would let `../../etc/passwd`
                    // read the root's own `etc/passwd` and look like it worked.
                    if !resolved.pop() || !resolved.starts_with(&self.root) {
                        return Err(WorkspaceError::Escape(display(trimmed)));
                    }
                }
                Component::Normal(name) => {
                    let name = name.to_string_lossy();
                    if excluded_workspace_entry(&name) {
                        return Err(WorkspaceError::Forbidden(display(trimmed)));
                    }
                    resolved.push(name.as_ref());
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(WorkspaceError::Escape(display(trimmed)));
                }
            }
        }

        self.verify_real_location(&resolved, trimmed)?;
        Ok(resolved)
    }

    /// Resolve a path that is about to be written, refusing on a read-only root.
    pub fn resolve_for_write(&self, requested: &str) -> Result<PathBuf, WorkspaceError> {
        if !self.allow_writes {
            return Err(WorkspaceError::ReadOnly);
        }
        self.resolve(requested)
    }

    /// The path as a tool should report it: relative to the root.
    pub fn display_path(&self, absolute: &Path) -> String {
        absolute
            .strip_prefix(&self.root)
            .unwrap_or(absolute)
            .display()
            .to_string()
    }

    /// Re-check the path after the filesystem has had its say about symlinks.
    ///
    /// The component walk above proves the *spelling* stays inside the root. It
    /// cannot prove the same of a symlink, and the file may not exist yet, so
    /// the deepest existing ancestor is what gets canonicalised.
    fn verify_real_location(&self, resolved: &Path, requested: &str) -> Result<(), WorkspaceError> {
        let mut existing = resolved;
        loop {
            match std::fs::canonicalize(existing) {
                Ok(real) => {
                    return real
                        .starts_with(&self.root)
                        .then_some(())
                        .ok_or_else(|| WorkspaceError::Escape(display(requested)));
                }
                Err(_) => match existing.parent() {
                    Some(parent) if parent.starts_with(&self.root) => existing = parent,
                    // Walked out of the root without finding anything real: the
                    // root itself was checked at construction.
                    _ => return Ok(()),
                },
            }
        }
    }
}

/// Echo a rejected path back without letting it carry control characters or
/// unbounded length into a log line or an error message.
fn display(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    if cleaned.is_empty() {
        "<empty>".into()
    } else {
        cleaned
    }
}

/// Every file under `root`, skipping excluded names, in a deterministic order.
///
/// One traversal policy for the whole harness: the memory indexer and the text
/// search both come through here, so a directory that is off limits to one is
/// off limits to the other.
pub fn walk_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_into(root, 0, limit, &mut out);
    out
}

/// Deep enough for any real tree; a bound at all is what stops a cycle.
const MAX_WALK_DEPTH: usize = 24;

fn walk_into(directory: &Path, depth: usize, limit: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_WALK_DEPTH || out.len() >= limit {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| !excluded_workspace_entry(&name.to_string_lossy()))
        })
        .collect();
    // Sorted, so two runs over the same tree agree on row order and a diff of
    // the index reads as a diff of the code.
    paths.sort();

    for path in paths {
        if out.len() >= limit {
            return;
        }
        // `symlink_metadata`, not `metadata`: a symlinked directory can point
        // back up the tree, and following it walks forever.
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            walk_into(&path, depth + 1, limit, out);
        } else if metadata.is_file() {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
        let workspace = Workspace::new(dir.path(), true).unwrap();
        (dir, workspace)
    }

    #[test]
    fn resolves_a_relative_path_inside_the_root() {
        let (dir, workspace) = workspace();
        let resolved = workspace.resolve("src/main.rs").unwrap();
        assert_eq!(resolved, workspace.root().join("src/main.rs"));
        assert_eq!(workspace.display_path(&resolved), "src/main.rs");
        drop(dir);
    }

    #[test]
    fn accepts_an_absolute_path_already_inside_the_root() {
        let (_dir, workspace) = workspace();
        let absolute = workspace.root().join("src/main.rs");
        assert_eq!(
            workspace.resolve(&absolute.display().to_string()).unwrap(),
            absolute
        );
    }

    #[test]
    fn refuses_to_climb_out_of_the_root() {
        let (_dir, workspace) = workspace();
        for path in ["../outside", "src/../../outside", "/etc/passwd"] {
            assert!(
                matches!(workspace.resolve(path), Err(WorkspaceError::Escape(_))),
                "{path} must not resolve"
            );
        }
    }

    #[test]
    fn refuses_secrets_and_vendored_trees_by_name() {
        let (_dir, workspace) = workspace();
        for path in [".env", "src/../.env", ".git/config", "node_modules/x.js"] {
            assert!(
                matches!(workspace.resolve(path), Err(WorkspaceError::Forbidden(_))),
                "{path} must not resolve"
            );
        }
    }

    /// The spelling of this path never leaves the root. Only the filesystem
    /// knows it does.
    #[cfg(unix)]
    #[test]
    fn refuses_a_symlink_that_points_outside() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "leaked").unwrap();
        let (_dir, workspace) = workspace();
        std::os::unix::fs::symlink(outside.path(), workspace.root().join("escape")).unwrap();

        assert!(matches!(
            workspace.resolve("escape/secret.txt"),
            Err(WorkspaceError::Escape(_))
        ));
    }

    #[test]
    fn resolves_a_path_that_does_not_exist_yet() {
        let (_dir, workspace) = workspace();
        let resolved = workspace.resolve_for_write("src/new/module.rs").unwrap();
        assert!(resolved.starts_with(workspace.root()));
    }

    #[test]
    fn a_read_only_workspace_refuses_writes_before_touching_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path(), false).unwrap();
        assert_eq!(
            workspace.resolve_for_write("notes.md"),
            Err(WorkspaceError::ReadOnly)
        );
    }
}
