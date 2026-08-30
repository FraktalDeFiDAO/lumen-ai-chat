//! Which files a request is allowed to touch, and whose they are.
//!
//! The harness confines its tools to one root. That was a complete answer while
//! there was one person: the root was the project, and the only person who could
//! reach it already had a shell on the same machine. With several people it is
//! not an answer at all. A shared root that anyone can write to is a channel
//! between them — one user's agent writes a file, another user's agent reads it,
//! and `search_memory` will happily retrieve it.
//!
//! So the root becomes a function of the user. `Shared` keeps today's behaviour,
//! which stays correct for a read-only workspace: one project, everyone reading
//! the same files, nobody able to leave anything for anybody. `PerUser` gives
//! each person a subtree of their own, and gives each person their own codebase
//! index, because an index built over one person's files is exactly the leak the
//! subtree was meant to prevent.
//!
//! The combination that cannot be made safe -- several people, one root, writes
//! enabled -- is refused at startup rather than served.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use thiserror::Error;

use crate::{
    harness::{
        memory::{HarnessMemory, HarnessMemoryError},
        workspace::{Workspace, WorkspaceError},
    },
    identity::User,
};

/// Per-user indexes are memory-mapped, so they are not free to hold open. This
/// bounds how many are kept; the least recently opened is dropped first, and
/// dropping one costs the next request that needs it a reopen, not its data.
const MAX_CACHED_USERS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMode {
    /// One root for everyone: the project, as it has always been.
    Shared,
    /// A subtree per user, created on demand.
    PerUser,
}

#[derive(Debug, Error)]
pub enum TenancyError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("the workspace for this user cannot be created: {0}")]
    Create(String),
    #[error(transparent)]
    Memory(#[from] HarnessMemoryError),
}

/// A resolved workspace and the index that goes with it.
#[derive(Clone)]
pub struct Tenancy {
    pub workspace: Workspace,
    pub memory: Option<Arc<HarnessMemory>>,
}

/// Resolves a user to the files they may touch.
pub struct Workspaces {
    mode: WorkspaceMode,
    base: PathBuf,
    allow_writes: bool,
    /// Where indexes live. `None` when retrieval is off entirely.
    memory_root: Option<PathBuf>,
    /// The shared answer, resolved once at startup so a misconfigured root fails
    /// then rather than on the first request.
    shared: Option<Tenancy>,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Tenancy>,
    order: Vec<String>,
}

impl std::fmt::Debug for Workspaces {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Workspaces")
            .field("mode", &self.mode)
            .field("base", &self.base)
            .field("allow_writes", &self.allow_writes)
            .finish()
    }
}

impl Workspaces {
    /// One root for everyone, with one index.
    pub fn shared(workspace: Workspace, memory: Option<Arc<HarnessMemory>>) -> Self {
        Self {
            mode: WorkspaceMode::Shared,
            base: workspace.root().to_path_buf(),
            allow_writes: workspace.allows_writes(),
            memory_root: None,
            shared: Some(Tenancy { workspace, memory }),
            cache: Mutex::new(Cache::default()),
        }
    }

    /// A subtree per user under `base`, and an index per user under
    /// `memory_root` when retrieval is on.
    pub fn per_user(
        base: &Path,
        allow_writes: bool,
        memory_root: Option<PathBuf>,
    ) -> Result<Self, TenancyError> {
        std::fs::create_dir_all(base)
            .map_err(|error| TenancyError::Create(format!("{}: {error}", base.display())))?;
        let base = std::fs::canonicalize(base)
            .map_err(|error| WorkspaceError::Root(format!("{}: {error}", base.display())))?;
        Ok(Self {
            mode: WorkspaceMode::PerUser,
            base,
            allow_writes,
            memory_root,
            shared: None,
            cache: Mutex::new(Cache::default()),
        })
    }

    pub fn mode(&self) -> WorkspaceMode {
        self.mode
    }

    /// The root everything hangs off. In per-user mode this is the parent of the
    /// subtrees, not a workspace anyone is served.
    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn allows_writes(&self) -> bool {
        self.allow_writes
    }

    /// Total records across every index currently open, for the metrics
    /// endpoint. A user whose index is not open contributes nothing, which is
    /// the honest answer: nothing is holding it.
    pub fn live_records(&self) -> usize {
        if let Some(shared) = &self.shared {
            return shared
                .memory
                .as_ref()
                .map_or(0, |memory| memory.live_count());
        }
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .values()
            .map(|tenancy| {
                tenancy
                    .memory
                    .as_ref()
                    .map_or(0, |memory| memory.live_count())
            })
            .sum()
    }

    /// The workspace and index this user is served.
    ///
    /// Creating the subtree on demand is what makes a new user work without an
    /// administrator: their first request makes their directory. The user id is
    /// validated on the way in precisely so it can be one path component here.
    pub fn for_user(&self, user: &User) -> Result<Tenancy, TenancyError> {
        if let Some(shared) = &self.shared {
            return Ok(shared.clone());
        }

        {
            let mut cache = self.lock();
            if let Some(tenancy) = cache.entries.get(&user.id).cloned() {
                cache.touch(&user.id);
                return Ok(tenancy);
            }
        }

        let root = self.base.join(&user.id);
        std::fs::create_dir_all(&root)
            .map_err(|error| TenancyError::Create(format!("{}: {error}", root.display())))?;
        let workspace = Workspace::new(&root, self.allow_writes)?;

        // A separate index per user, because one index over everyone's files
        // would answer `search_memory` with somebody else's code.
        let memory = match &self.memory_root {
            Some(directory) => Some(Arc::new(HarnessMemory::open(
                &directory.join(&user.id),
                workspace.root(),
            )?)),
            None => None,
        };

        let tenancy = Tenancy { workspace, memory };
        let mut cache = self.lock();
        cache.insert(user.id.clone(), tenancy.clone());
        Ok(tenancy)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Cache {
    fn touch(&mut self, id: &str) {
        if let Some(position) = self.order.iter().position(|item| item == id) {
            let id = self.order.remove(position);
            self.order.push(id);
        }
    }

    fn insert(&mut self, id: String, tenancy: Tenancy) {
        if self.entries.insert(id.clone(), tenancy).is_none() {
            self.order.push(id);
        } else {
            self.touch(&id);
        }
        while self.order.len() > MAX_CACHED_USERS {
            let evicted = self.order.remove(0);
            self.entries.remove(&evicted);
        }
    }
}

/// The one arrangement that cannot be served.
///
/// Several people, one root, writes enabled: whatever one person's agent writes,
/// another person's agent reads, and the index will retrieve it for them. There
/// is no runtime check that fixes this, so it is refused before the service is
/// listening.
pub fn validate(
    mode: WorkspaceMode,
    multi_user: bool,
    allow_writes: bool,
) -> Result<(), &'static str> {
    if multi_user && allow_writes && mode == WorkspaceMode::Shared {
        return Err(
            "a shared writable workspace cannot serve several users: set \
             AI_WORKSPACE_MODE=per_user, or leave AI_CODER_ALLOW_WRITES unset",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str) -> User {
        User {
            id: id.into(),
            display_name: None,
            email: None,
        }
    }

    #[test]
    fn a_shared_workspace_answers_everyone_with_the_same_root() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(directory.path(), false).unwrap();
        let workspaces = Workspaces::shared(workspace, None);

        let riley = workspaces.for_user(&user("riley")).unwrap();
        let sam = workspaces.for_user(&user("sam")).unwrap();

        assert_eq!(riley.workspace.root(), sam.workspace.root());
        assert_eq!(workspaces.mode(), WorkspaceMode::Shared);
    }

    #[test]
    fn per_user_mode_gives_each_person_their_own_subtree() {
        let directory = tempfile::tempdir().unwrap();
        let workspaces = Workspaces::per_user(directory.path(), true, None).unwrap();

        let riley = workspaces.for_user(&user("riley")).unwrap();
        let sam = workspaces.for_user(&user("sam")).unwrap();

        assert_ne!(riley.workspace.root(), sam.workspace.root());
        assert!(riley.workspace.root().ends_with("riley"));
        assert!(sam.workspace.root().ends_with("sam"));
        assert!(
            riley.workspace.root().is_dir(),
            "the subtree was not created"
        );
    }

    /// The point of the subtree: a path is confined to it, so one user cannot
    /// name their way into another's files even knowing the layout.
    #[test]
    fn one_users_files_are_unreachable_from_anothers_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let workspaces = Workspaces::per_user(directory.path(), true, None).unwrap();

        let riley = workspaces.for_user(&user("riley")).unwrap();
        std::fs::write(riley.workspace.root().join("secret.txt"), "private").unwrap();

        let sam = workspaces.for_user(&user("sam")).unwrap();
        assert!(sam.workspace.resolve("../riley/secret.txt").is_err());
        assert!(
            sam.workspace
                .resolve(
                    &riley
                        .workspace
                        .root()
                        .join("secret.txt")
                        .display()
                        .to_string()
                )
                .is_err()
        );
    }

    #[test]
    fn a_returning_user_gets_the_same_workspace_back() {
        let directory = tempfile::tempdir().unwrap();
        let workspaces = Workspaces::per_user(directory.path(), true, None).unwrap();

        let first = workspaces.for_user(&user("riley")).unwrap();
        std::fs::write(first.workspace.root().join("kept.txt"), "still here").unwrap();
        let second = workspaces.for_user(&user("riley")).unwrap();

        assert_eq!(first.workspace.root(), second.workspace.root());
        assert!(second.workspace.root().join("kept.txt").is_file());
    }

    #[test]
    fn each_user_gets_an_index_of_their_own_files_only() {
        let workspace_root = tempfile::tempdir().unwrap();
        let index_root = tempfile::tempdir().unwrap();
        let workspaces = Workspaces::per_user(
            workspace_root.path(),
            true,
            Some(index_root.path().to_path_buf()),
        )
        .unwrap();

        let riley = workspaces.for_user(&user("riley")).unwrap();
        std::fs::write(
            riley.workspace.root().join("notes.md"),
            "# Riley's private plan\nthe passphrase is hunter2\n",
        )
        .unwrap();
        riley.memory.as_ref().unwrap().index_workspace().unwrap();
        assert!(riley.memory.as_ref().unwrap().live_count() > 0);

        let sam = workspaces.for_user(&user("sam")).unwrap();
        sam.memory.as_ref().unwrap().index_workspace().unwrap();

        let found = sam
            .memory
            .as_ref()
            .unwrap()
            .search("passphrase", 5)
            .unwrap();
        assert!(
            found.is_empty(),
            "another user's file was retrievable: {found:?}"
        );
    }

    #[test]
    fn holding_many_users_open_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let workspaces = Workspaces::per_user(directory.path(), false, None).unwrap();

        for index in 0..MAX_CACHED_USERS + 10 {
            workspaces.for_user(&user(&format!("user{index}"))).unwrap();
        }

        let cache = workspaces.lock();
        assert_eq!(cache.entries.len(), MAX_CACHED_USERS);
        assert_eq!(cache.order.len(), MAX_CACHED_USERS);
        assert!(
            cache
                .entries
                .contains_key(&format!("user{}", MAX_CACHED_USERS + 9)),
            "the most recent user was evicted"
        );
        assert!(
            !cache.entries.contains_key("user0"),
            "the oldest user was kept"
        );
    }

    /// Evicting a cached workspace must not lose anything: the directory and its
    /// index are on disk, and reopening finds them.
    #[test]
    fn an_evicted_user_comes_back_to_the_same_files() {
        let directory = tempfile::tempdir().unwrap();
        let workspaces = Workspaces::per_user(directory.path(), true, None).unwrap();

        let first = workspaces.for_user(&user("riley")).unwrap();
        std::fs::write(first.workspace.root().join("kept.txt"), "still here").unwrap();
        for index in 0..MAX_CACHED_USERS + 1 {
            workspaces
                .for_user(&user(&format!("filler{index}")))
                .unwrap();
        }

        let again = workspaces.for_user(&user("riley")).unwrap();
        assert_eq!(again.workspace.root(), first.workspace.root());
        assert!(again.workspace.root().join("kept.txt").is_file());
    }

    #[test]
    fn a_shared_writable_workspace_cannot_serve_several_users() {
        assert!(validate(WorkspaceMode::Shared, true, true).is_err());

        // Every other combination is fine, and each for its own reason.
        assert!(
            validate(WorkspaceMode::Shared, true, false).is_ok(),
            "read-only sharing is a project"
        );
        assert!(
            validate(WorkspaceMode::Shared, false, true).is_ok(),
            "one person writing their own files"
        );
        assert!(
            validate(WorkspaceMode::PerUser, true, true).is_ok(),
            "everyone writing their own subtree"
        );
        assert!(validate(WorkspaceMode::PerUser, false, false).is_ok());
    }
}
