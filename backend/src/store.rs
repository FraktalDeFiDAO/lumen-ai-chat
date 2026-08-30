//! Where conversations live.
//!
//! They lived in one browser's `localStorage` until now, which is why clearing
//! site data destroyed them, why they never followed anyone to a second device,
//! and why the server had no idea they existed. Serving more than one person
//! means the server has to hold them, and holding them for several people means
//! every read and every write is scoped to exactly one of them.
//!
//! That scoping is the whole security property of this module, so it is not
//! left to callers: there is no method that can reach a row without naming the
//! user it belongs to, and no method that takes an id without also taking that
//! user. A handler cannot forget to filter, because there is nothing to forget.
//!
//! SQLite, driven synchronously. It matches how the rest of this codebase
//! treats blocking work -- the memory index and the file tools are synchronous
//! too, and the callers wrap them in `spawn_blocking` rather than pretending
//! the work is async. One file, one volume, no service to operate.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A conversation is a chat transcript; nothing here is unbounded, because a
/// client that misbehaves must not be able to fill the volume.
const MAX_TITLE_CHARS: usize = 200;
const MAX_MESSAGES_PER_CONVERSATION: usize = 2_000;
const MAX_CONTENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("the conversation store is unusable: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("the store directory {path} cannot be created: {message}")]
    Directory { path: String, message: String },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StoredRole {
    User,
    Assistant,
}

impl StoredRole {
    fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            other => Err(StoreError::Invalid(format!(
                "unknown message role `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredMessage {
    pub id: String,
    pub role: StoredRole,
    pub content: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
    /// Everything the client keeps on a message that the server has no opinion
    /// about: tool activity, retrieval counts, whether a run was cut short. It
    /// is stored verbatim so a newer client can add a field without a migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredConversation {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub messages: Vec<StoredMessage>,
}

/// A conversation without its transcript, for the history list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: i64,
}

/// The conversation store.
///
/// A single connection behind a mutex: SQLite serialises writers anyway, the
/// transcripts are small, and one lock is easier to reason about than a pool
/// whose connections each need the same pragmas.
#[derive(Clone)]
pub struct ConversationStore {
    connection: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for ConversationStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ConversationStore")
    }
}

impl ConversationStore {
    /// Open, or create, the store at `path`, applying the schema.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| StoreError::Directory {
                path: parent.display().to_string(),
                message: error.to_string(),
            })?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// An in-memory store, for tests and for a deployment that deliberately
    /// keeps nothing.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        // Write-ahead logging so a read never waits on a write, and foreign keys
        // so deleting a conversation actually takes its messages with it --
        // SQLite leaves them off by default and would otherwise orphan them.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Every conversation belonging to one user, most recently touched first.
    pub fn list(&self, user: &str) -> Result<Vec<ConversationSummary>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT c.id, c.title, c.created_at, c.updated_at,
                    (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id)
             FROM conversations c
             WHERE c.user_id = ?1
             ORDER BY c.updated_at DESC, c.id",
        )?;
        let rows = statement.query_map(params![user], |row| {
            Ok(ConversationSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                message_count: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// One conversation with its transcript, or `None` -- which is also the
    /// answer when it belongs to somebody else. A caller cannot tell the
    /// difference, and must not be able to.
    pub fn get(&self, user: &str, id: &str) -> Result<Option<StoredConversation>, StoreError> {
        let connection = self.lock();
        let header = connection
            .query_row(
                "SELECT id, title, created_at, updated_at
                 FROM conversations WHERE user_id = ?1 AND id = ?2",
                params![user, id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        let Some((id, title, created_at, updated_at)) = header else {
            return Ok(None);
        };

        let mut statement = connection.prepare(
            "SELECT id, role, content, created_at, feedback, extra
             FROM messages WHERE conversation_id = ?1 ORDER BY position",
        )?;
        let rows = statement.query_map(params![&id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (id, role, content, created_at, feedback, extra) = row?;
            messages.push(StoredMessage {
                id,
                role: StoredRole::parse(&role)?,
                content,
                created_at,
                feedback,
                extra: extra.and_then(|value| serde_json::from_str(&value).ok()),
            });
        }

        Ok(Some(StoredConversation {
            id,
            title,
            created_at,
            updated_at,
            messages,
        }))
    }

    /// Write a conversation, replacing whatever was there.
    ///
    /// The client owns the transcript and sends it whole; reconciling a partial
    /// update against a stream that is still arriving is a race with no winner.
    /// Replacing is idempotent, which is what makes it safe to call on every
    /// change.
    pub fn save(&self, user: &str, conversation: &StoredConversation) -> Result<(), StoreError> {
        validate(conversation)?;
        let mut connection = self.lock();
        let transaction = connection.transaction()?;

        // The insert names the user, and the update is filtered by them, so a
        // second person claiming an existing id changes nothing rather than
        // taking it over.
        let owned = transaction.execute(
            "INSERT INTO conversations (id, user_id, title, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                updated_at = excluded.updated_at
             WHERE conversations.user_id = ?2",
            params![
                conversation.id,
                user,
                conversation.title,
                conversation.created_at,
                conversation.updated_at
            ],
        )?;
        if owned == 0 {
            return Err(StoreError::Invalid(
                "that conversation belongs to someone else".into(),
            ));
        }

        transaction.execute(
            "DELETE FROM messages WHERE conversation_id = ?1",
            params![conversation.id],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO messages
                    (id, conversation_id, position, role, content, created_at, feedback, extra)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (position, message) in conversation.messages.iter().enumerate() {
                insert.execute(params![
                    message.id,
                    conversation.id,
                    position as i64,
                    message.role.as_str(),
                    message.content,
                    message.created_at,
                    message.feedback,
                    message
                        .extra
                        .as_ref()
                        .map(|value| serde_json::to_string(value).unwrap_or_default()),
                ])?;
            }
        }

        transaction.commit()?;
        Ok(())
    }

    /// Delete one conversation. Returns whether there was one to delete, which
    /// is false both for an id that does not exist and for one that is not this
    /// user's.
    pub fn delete(&self, user: &str, id: &str) -> Result<bool, StoreError> {
        let connection = self.lock();
        let removed = connection.execute(
            "DELETE FROM conversations WHERE user_id = ?1 AND id = ?2",
            params![user, id],
        )?;
        Ok(removed > 0)
    }

    /// Delete everything belonging to one user, and nothing belonging to anyone
    /// else. Returns how many conversations went.
    pub fn delete_all(&self, user: &str) -> Result<usize, StoreError> {
        let connection = self.lock();
        Ok(connection.execute(
            "DELETE FROM conversations WHERE user_id = ?1",
            params![user],
        )?)
    }

    /// Total conversations held, for the metrics endpoint. Not per user, and
    /// deliberately not exposed to a client.
    pub fn total(&self) -> Result<i64, StoreError> {
        let connection = self.lock();
        Ok(connection.query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))?)
    }

    /// A poisoned lock means a previous caller panicked mid-statement. The
    /// connection is still usable -- SQLite rolled back whatever was open -- and
    /// refusing every later request would turn one panic into an outage.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn validate(conversation: &StoredConversation) -> Result<(), StoreError> {
    if conversation.id.trim().is_empty() {
        return Err(StoreError::Invalid("a conversation needs an id".into()));
    }
    if conversation.title.chars().count() > MAX_TITLE_CHARS {
        return Err(StoreError::Invalid(format!(
            "a title is at most {MAX_TITLE_CHARS} characters"
        )));
    }
    if conversation.messages.len() > MAX_MESSAGES_PER_CONVERSATION {
        return Err(StoreError::Invalid(format!(
            "a conversation holds at most {MAX_MESSAGES_PER_CONVERSATION} messages"
        )));
    }
    for message in &conversation.messages {
        if message.id.trim().is_empty() {
            return Err(StoreError::Invalid("a message needs an id".into()));
        }
        if message.content.len() > MAX_CONTENT_BYTES {
            return Err(StoreError::Invalid(format!(
                "a message is at most {MAX_CONTENT_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS conversations (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL,
    title       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS conversations_by_user
    ON conversations (user_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS messages (
    id              TEXT NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    position        INTEGER NOT NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    feedback        TEXT,
    extra           TEXT,
    PRIMARY KEY (conversation_id, position)
);
CREATE INDEX IF NOT EXISTS messages_by_conversation
    ON messages (conversation_id, position);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, role: StoredRole, content: &str) -> StoredMessage {
        StoredMessage {
            id: id.into(),
            role,
            content: content.into(),
            created_at: 1,
            feedback: None,
            extra: None,
        }
    }

    fn conversation(id: &str, title: &str, updated_at: i64) -> StoredConversation {
        StoredConversation {
            id: id.into(),
            title: title.into(),
            created_at: updated_at,
            updated_at,
            messages: vec![
                message("m1", StoredRole::User, "a question"),
                message("m2", StoredRole::Assistant, "an answer"),
            ],
        }
    }

    fn store() -> ConversationStore {
        ConversationStore::in_memory().unwrap()
    }

    #[test]
    fn a_saved_conversation_comes_back_whole() {
        let store = store();
        let saved = conversation("c1", "First chat", 10);
        store.save("riley", &saved).unwrap();

        assert_eq!(store.get("riley", "c1").unwrap().as_ref(), Some(&saved));
    }

    #[test]
    fn the_history_is_ordered_by_when_it_was_last_touched() {
        let store = store();
        store
            .save("riley", &conversation("older", "Older", 10))
            .unwrap();
        store
            .save("riley", &conversation("newer", "Newer", 20))
            .unwrap();

        let listed = store.list("riley").unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["newer", "older"]
        );
        assert_eq!(listed[0].message_count, 2);
    }

    #[test]
    fn saving_again_replaces_the_transcript_rather_than_appending_to_it() {
        let store = store();
        store
            .save("riley", &conversation("c1", "First", 10))
            .unwrap();

        let mut updated = conversation("c1", "Renamed", 20);
        updated
            .messages
            .push(message("m3", StoredRole::User, "another"));
        store.save("riley", &updated).unwrap();

        let read = store.get("riley", "c1").unwrap().unwrap();
        assert_eq!(read.title, "Renamed");
        assert_eq!(read.updated_at, 20);
        assert_eq!(read.messages.len(), 3);
        assert_eq!(store.list("riley").unwrap().len(), 1);
    }

    #[test]
    fn message_order_survives_the_round_trip() {
        let store = store();
        let mut saved = conversation("c1", "Ordered", 10);
        saved.messages = (0..10)
            .map(|index| {
                message(
                    &format!("m{index}"),
                    if index % 2 == 0 {
                        StoredRole::User
                    } else {
                        StoredRole::Assistant
                    },
                    &format!("message {index}"),
                )
            })
            .collect();
        store.save("riley", &saved).unwrap();

        let read = store.get("riley", "c1").unwrap().unwrap();
        assert_eq!(
            read.messages
                .iter()
                .map(|item| item.content.as_str())
                .collect::<Vec<_>>(),
            saved
                .messages
                .iter()
                .map(|item| item.content.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn whatever_the_client_keeps_on_a_message_comes_back_unchanged() {
        let store = store();
        let mut saved = conversation("c1", "With tools", 10);
        saved.messages[1].feedback = Some("up".into());
        saved.messages[1].extra = Some(serde_json::json!({
            "tools": [{"id": "call-1", "name": "read_file", "output": "ok"}],
            "contextRecords": 4,
            "incomplete": true,
        }));
        store.save("riley", &saved).unwrap();

        let read = store.get("riley", "c1").unwrap().unwrap();
        assert_eq!(read.messages[1].feedback.as_deref(), Some("up"));
        assert_eq!(read.messages[1].extra, saved.messages[1].extra);
    }

    // -----------------------------------------------------------------------
    // Isolation. These are the reason this module exists in this shape.
    // -----------------------------------------------------------------------

    #[test]
    fn one_users_history_never_includes_anothers() {
        let store = store();
        store
            .save("riley", &conversation("riley-1", "Riley's", 10))
            .unwrap();
        store
            .save("sam", &conversation("sam-1", "Sam's", 20))
            .unwrap();

        assert_eq!(
            store
                .list("riley")
                .unwrap()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["riley-1"]
        );
        assert_eq!(
            store
                .list("sam")
                .unwrap()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["sam-1"]
        );
    }

    #[test]
    fn a_conversation_cannot_be_read_by_guessing_its_id() {
        let store = store();
        store
            .save("riley", &conversation("secret", "Private", 10))
            .unwrap();

        assert_eq!(store.get("sam", "secret").unwrap(), None);
    }

    #[test]
    fn a_conversation_cannot_be_deleted_by_someone_it_does_not_belong_to() {
        let store = store();
        store
            .save("riley", &conversation("secret", "Private", 10))
            .unwrap();

        assert!(!store.delete("sam", "secret").unwrap());
        assert!(store.get("riley", "secret").unwrap().is_some());
    }

    #[test]
    fn a_conversation_cannot_be_taken_over_by_saving_over_its_id() {
        let store = store();
        store
            .save("riley", &conversation("shared-id", "Riley's", 10))
            .unwrap();

        let attempt = store.save("sam", &conversation("shared-id", "Sam's", 20));
        assert!(
            attempt.is_err(),
            "an id belonging to someone else was overwritten"
        );

        let read = store.get("riley", "shared-id").unwrap().unwrap();
        assert_eq!(read.title, "Riley's", "the original was modified");
        assert_eq!(store.get("sam", "shared-id").unwrap(), None);
    }

    #[test]
    fn clearing_one_users_history_leaves_everyone_elses() {
        let store = store();
        store
            .save("riley", &conversation("riley-1", "One", 10))
            .unwrap();
        store
            .save("riley", &conversation("riley-2", "Two", 20))
            .unwrap();
        store
            .save("sam", &conversation("sam-1", "Sam's", 30))
            .unwrap();

        assert_eq!(store.delete_all("riley").unwrap(), 2);
        assert!(store.list("riley").unwrap().is_empty());
        assert_eq!(store.list("sam").unwrap().len(), 1);
    }

    // -----------------------------------------------------------------------

    #[test]
    fn deleting_a_conversation_takes_its_messages_with_it() {
        let store = store();
        store
            .save("riley", &conversation("c1", "Doomed", 10))
            .unwrap();

        assert!(store.delete("riley", "c1").unwrap());
        assert_eq!(store.get("riley", "c1").unwrap(), None);

        let connection = store.lock();
        let orphans: i64 = connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            orphans, 0,
            "messages outlived the conversation they belonged to"
        );
    }

    #[test]
    fn deleting_something_that_is_not_there_is_not_an_error() {
        assert!(!store().delete("riley", "never-existed").unwrap());
        assert_eq!(store().delete_all("riley").unwrap(), 0);
    }

    #[test]
    fn a_conversation_that_could_fill_the_volume_is_refused() {
        let store = store();

        let mut untitled = conversation("c1", "ok", 10);
        untitled.id = "  ".into();
        assert!(store.save("riley", &untitled).is_err());

        let mut long_title = conversation("c2", &"x".repeat(MAX_TITLE_CHARS + 1), 10);
        long_title.title = "x".repeat(MAX_TITLE_CHARS + 1);
        assert!(store.save("riley", &long_title).is_err());

        let mut many = conversation("c3", "ok", 10);
        many.messages = (0..MAX_MESSAGES_PER_CONVERSATION + 1)
            .map(|index| message(&format!("m{index}"), StoredRole::User, "x"))
            .collect();
        assert!(store.save("riley", &many).is_err());

        let mut huge = conversation("c4", "ok", 10);
        huge.messages[0].content = "x".repeat(MAX_CONTENT_BYTES + 1);
        assert!(store.save("riley", &huge).is_err());

        assert!(
            store.list("riley").unwrap().is_empty(),
            "a refused save left rows behind"
        );
    }

    #[test]
    fn an_empty_conversation_is_allowed_because_a_new_chat_is_one() {
        let store = store();
        let mut empty = conversation("c1", "New conversation", 10);
        empty.messages.clear();
        store.save("riley", &empty).unwrap();

        assert_eq!(store.get("riley", "c1").unwrap().unwrap().messages.len(), 0);
        assert_eq!(store.list("riley").unwrap()[0].message_count, 0);
    }

    #[test]
    fn the_store_survives_being_reopened() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/conversations.db");

        let store = ConversationStore::open(&path).unwrap();
        store
            .save("riley", &conversation("c1", "Persisted", 10))
            .unwrap();
        drop(store);

        let reopened = ConversationStore::open(&path).unwrap();
        assert_eq!(
            reopened.get("riley", "c1").unwrap().unwrap().title,
            "Persisted"
        );
        assert_eq!(reopened.total().unwrap(), 1);
    }

    #[test]
    fn an_unreadable_role_is_reported_rather_than_guessed() {
        let store = store();
        store
            .save("riley", &conversation("c1", "Fine", 10))
            .unwrap();
        store
            .lock()
            .execute("UPDATE messages SET role = 'system' WHERE position = 0", [])
            .unwrap();

        assert!(matches!(
            store.get("riley", "c1"),
            Err(StoreError::Invalid(_))
        ));
    }
}
