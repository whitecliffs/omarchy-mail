use crate::models::{
    Account, AttachmentInfo, CalendarChange, CalendarEvent, MailFolder, Message,
    OutgoingAttachment, PendingAction, PendingSend, ServerConfig, TodoItem,
};
use crate::security;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not create application data directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid stored value: {0}")]
    InvalidValue(String),
}

pub type Result<T> = std::result::Result<T, DatabaseError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub name: String,
    pub email: String,
    pub score: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoSyncItem {
    pub todo: TodoItem,
    pub revision: i64,
    pub remote_id: Option<String>,
    pub synced_revision: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RemoteTask {
    pub remote_id: String,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub due_at: Option<String>,
    #[serde(default)]
    pub completed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoSyncOutcome {
    /// (todo id, new remote id, revision uploaded)
    pub created: Vec<(i64, String, i64)>,
    /// (todo id, revision uploaded)
    pub pushed: Vec<(i64, i64)>,
    /// todo_deletions rows that were applied remotely
    pub deletions_done: Vec<i64>,
    /// complete remote task list after the pushes
    pub remote: Vec<RemoteTask>,
}

/// Highest schema version `migrate` produces. Backups record it so a newer
/// backup is never restored into an older app.
pub const SCHEMA_VERSION: i64 = 8;

#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn open_default() -> Result<Self> {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
            });
        Self::open(data_home.join("omarchy-mail"))
    }

    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        security::ensure_private_dir(&data_dir)?;
        let database = Self {
            path: data_dir.join("mail.db"),
        };
        if database.path.exists() {
            security::set_private_file_permissions(&database.path)?;
        }
        database.migrate()?;
        security::set_private_file_permissions(&database.path)?;
        Ok(database)
    }

    fn connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)?;
        connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 3000;")?;
        Ok(connection)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self.connection()?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);\n             INSERT INTO schema_version(version)\n             SELECT 0 WHERE NOT EXISTS (SELECT 1 FROM schema_version);",
        )?;
        let mut version: i64 =
            connection.query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
                row.get(0)
            })?;
        if version < 1 {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS accounts (\n                    id INTEGER PRIMARY KEY,\n                    email TEXT NOT NULL UNIQUE,\n                    display_name TEXT NOT NULL,\n                    incoming_json TEXT NOT NULL,\n                    outgoing_json TEXT NOT NULL,\n                    enabled INTEGER NOT NULL DEFAULT 1,\n                    notify INTEGER NOT NULL DEFAULT 1\n                );\n                CREATE TABLE IF NOT EXISTS folders (\n                    id INTEGER PRIMARY KEY,\n                    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,\n                    name TEXT NOT NULL,\n                    remote_name TEXT NOT NULL,\n                    kind TEXT NOT NULL,\n                    unread_count INTEGER NOT NULL DEFAULT 0,\n                    UNIQUE(account_id, remote_name)\n                );\n                CREATE TABLE IF NOT EXISTS messages (\n                    id INTEGER PRIMARY KEY,\n                    account_id INTEGER REFERENCES accounts(id) ON DELETE CASCADE,\n                    folder TEXT NOT NULL,\n                    remote_uid INTEGER,\n                    uidvalidity INTEGER,\n                    message_id TEXT,\n                    thread_key TEXT,\n                    sender_name TEXT NOT NULL,\n                    sender_email TEXT NOT NULL,\n                    recipients TEXT NOT NULL,\n                    subject TEXT NOT NULL,\n                    preview TEXT NOT NULL,\n                    body TEXT NOT NULL,\n                    received_at TEXT NOT NULL,\n                    unread INTEGER NOT NULL DEFAULT 1,\n                    starred INTEGER NOT NULL DEFAULT 0,\n                    has_attachments INTEGER NOT NULL DEFAULT 0,\n                    thread_size INTEGER NOT NULL DEFAULT 1,\n                    UNIQUE(account_id, folder, remote_uid, uidvalidity)\n                );\n                CREATE TABLE IF NOT EXISTS pending_actions (\n                    id INTEGER PRIMARY KEY,\n                    account_id INTEGER REFERENCES accounts(id) ON DELETE CASCADE,\n                    message_id INTEGER REFERENCES messages(id) ON DELETE CASCADE,\n                    action TEXT NOT NULL,\n                    payload_json TEXT NOT NULL,\n                    created_at TEXT NOT NULL\n                );\n                CREATE INDEX IF NOT EXISTS messages_received_idx ON messages(received_at DESC);\n                CREATE INDEX IF NOT EXISTS messages_folder_idx ON messages(account_id, folder);\n                CREATE INDEX IF NOT EXISTS messages_unread_idx ON messages(unread);\n                CREATE VIRTUAL TABLE IF NOT EXISTS message_search USING fts5(\n                    subject, sender, recipients, body, content='messages', content_rowid='id'\n                );\n                CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN\n                    INSERT INTO message_search(rowid, subject, sender, recipients, body)\n                    VALUES (new.id, new.subject, new.sender_name || ' ' || new.sender_email, new.recipients, new.body);\n                END;\n                CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN\n                    INSERT INTO message_search(message_search, rowid, subject, sender, recipients, body)\n                    VALUES ('delete', old.id, old.subject, old.sender_name || ' ' || old.sender_email, old.recipients, old.body);\n                END;\n                CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN\n                    INSERT INTO message_search(message_search, rowid, subject, sender, recipients, body)\n                    VALUES ('delete', old.id, old.subject, old.sender_name || ' ' || old.sender_email, old.recipients, old.body);\n                    INSERT INTO message_search(rowid, subject, sender, recipients, body)\n                    VALUES (new.id, new.subject, new.sender_name || ' ' || new.sender_email, new.recipients, new.body);\n                END;\n                UPDATE schema_version SET version = 1;",
            )?;
            version = 1;
        }
        if version < 2 {
            connection.execute_batch(
                "ALTER TABLE messages ADD COLUMN attachments_json TEXT NOT NULL DEFAULT '[]';
                 UPDATE schema_version SET version = 2;",
            )?;
        }
        if version < 3 {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_sends (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                    to_recipients TEXT NOT NULL,
                    cc_json TEXT NOT NULL DEFAULT '[]',
                    subject TEXT NOT NULL,
                    body TEXT NOT NULL,
                    attachments_json TEXT NOT NULL DEFAULT '[]',
                    created_at TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    retryable INTEGER NOT NULL DEFAULT 1,
                    next_attempt_at TEXT,
                    last_error TEXT,
                    sent INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS pending_sends_due_idx
                    ON pending_sends(account_id, sent, retryable, next_attempt_at);
                UPDATE schema_version SET version = 3;",
            )?;
            version = 3;
        }
        if version < 4 {
            connection.execute_batch(
                "ALTER TABLE pending_sends ADD COLUMN bcc_json TEXT NOT NULL DEFAULT '[]';
                 UPDATE schema_version SET version = 4;",
            )?;
            version = 4;
        }
        if version < 5 {
            connection.execute_batch(
                "ALTER TABLE messages ADD COLUMN body_html TEXT;
                 ALTER TABLE pending_sends ADD COLUMN body_html TEXT;
                 UPDATE schema_version SET version = 5;",
            )?;
        }
        if version < 6 {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS calendar_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    title TEXT NOT NULL,
                    notes TEXT NOT NULL DEFAULT '',
                    starts_at TEXT NOT NULL,
                    ends_at TEXT NOT NULL,
                    location TEXT NOT NULL DEFAULT '',
                    message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL
                );
                CREATE INDEX IF NOT EXISTS calendar_events_start_idx
                    ON calendar_events(starts_at);
                CREATE TABLE IF NOT EXISTS todo_items (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    title TEXT NOT NULL,
                    notes TEXT NOT NULL DEFAULT '',
                    due_at TEXT,
                    completed INTEGER NOT NULL DEFAULT 0,
                    message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL
                );
                CREATE INDEX IF NOT EXISTS todo_items_due_idx
                    ON todo_items(completed, due_at);
                UPDATE schema_version SET version = 6;",
            )?;
        }
        if version < 7 {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS calendar_sync_links (
                event_id INTEGER PRIMARY KEY REFERENCES calendar_events(id) ON DELETE CASCADE,
                source TEXT NOT NULL, remote_key TEXT NOT NULL, href TEXT NOT NULL,
                UNIQUE(source, remote_key)); UPDATE schema_version SET version = 7;",
            )?;
        }
        if version < 8 {
            connection.execute_batch(
                "ALTER TABLE calendar_sync_links ADD COLUMN recurring INTEGER NOT NULL DEFAULT 0;
                 CREATE TABLE IF NOT EXISTS calendar_changes (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    source TEXT NOT NULL, href TEXT NOT NULL, remote_key TEXT NOT NULL,
                    action TEXT NOT NULL, event_json TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS reminders_sent (
                    event_key TEXT PRIMARY KEY, sent_at TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS snoozed_messages (
                    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
                    until TEXT NOT NULL);
                 ALTER TABLE messages ADD COLUMN list_unsubscribe TEXT;
                 ALTER TABLE messages ADD COLUMN list_unsubscribe_one_click INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE todo_items ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
                 CREATE TABLE IF NOT EXISTS todo_sync_links (
                    todo_id INTEGER PRIMARY KEY REFERENCES todo_items(id) ON DELETE CASCADE,
                    source TEXT NOT NULL, remote_id TEXT NOT NULL, synced_revision INTEGER NOT NULL,
                    UNIQUE(source, remote_id));
                 CREATE TABLE IF NOT EXISTS todo_deletions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, source TEXT NOT NULL, remote_id TEXT NOT NULL);
                 UPDATE schema_version SET version = 8;",
            )?;
        }
        Ok(())
    }

    pub fn has_accounts(&self) -> Result<bool> {
        let connection = self.connection()?;
        Ok(
            connection.query_row("SELECT EXISTS(SELECT 1 FROM accounts)", [], |row| {
                row.get(0)
            })?,
        )
    }

    pub fn load_accounts(&self) -> Result<Vec<Account>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, email, display_name, incoming_json, outgoing_json, enabled, notify\n             FROM accounts ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            let incoming: String = row.get(3)?;
            let outgoing: String = row.get(4)?;
            let incoming = serde_json::from_str::<ServerConfig>(&incoming).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            let outgoing = serde_json::from_str::<ServerConfig>(&outgoing).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok(Account {
                id: row.get(0)?,
                email: row.get(1)?,
                display_name: row.get(2)?,
                incoming,
                outgoing,
                enabled: row.get::<_, i64>(5)? != 0,
                notify: row.get::<_, i64>(6)? != 0,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn save_account(&self, account: &Account) -> Result<i64> {
        let connection = self.connection()?;
        let incoming = serde_json::to_string(&account.incoming)
            .map_err(|error| DatabaseError::InvalidValue(error.to_string()))?;
        let outgoing = serde_json::to_string(&account.outgoing)
            .map_err(|error| DatabaseError::InvalidValue(error.to_string()))?;
        connection.execute(
            "INSERT INTO accounts(email, display_name, incoming_json, outgoing_json, enabled, notify)\n             VALUES (?1, ?2, ?3, ?4, ?5, ?6)\n             ON CONFLICT(email) DO UPDATE SET display_name=excluded.display_name, incoming_json=excluded.incoming_json,\n             outgoing_json=excluded.outgoing_json, enabled=excluded.enabled, notify=excluded.notify",
            params![account.email, account.display_name, incoming, outgoing, account.enabled as i64, account.notify as i64],
        )?;
        Ok(connection.query_row(
            "SELECT id FROM accounts WHERE email = ?1",
            [&account.email],
            |row| row.get(0),
        )?)
    }

    pub fn delete_account(&self, account_id: i64) -> Result<()> {
        let connection = self.connection()?;
        connection.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
        Ok(())
    }

    pub fn upsert_folders(&self, folders: &[MailFolder]) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        for folder in folders {
            transaction.execute(
                "INSERT INTO folders(account_id, name, remote_name, kind, unread_count)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account_id, remote_name) DO UPDATE SET
                 name=excluded.name, kind=excluded.kind, unread_count=excluded.unread_count",
                params![
                    folder.account_id,
                    folder.name,
                    folder.remote_name,
                    folder.kind,
                    folder.unread_count,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn load_folders(&self) -> Result<Vec<MailFolder>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT account_id, name, remote_name, kind, unread_count
             FROM folders ORDER BY account_id, name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(MailFolder {
                account_id: row.get(0)?,
                name: row.get(1)?,
                remote_name: row.get(2)?,
                kind: row.get(3)?,
                unread_count: row.get::<_, i64>(4)?.max(0) as u32,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    /// Records a mailbox created on the server. This is intentionally a
    /// small cache update; IMAP remains authoritative and the next account
    /// sync can reconcile the metadata if the server presents different
    /// casing or hierarchy details.
    pub fn create_folder(&self, folder: &MailFolder) -> Result<()> {
        self.upsert_folders(std::slice::from_ref(folder))
    }

    /// Renames cached folder metadata and all cached messages belonging to it
    /// in one transaction after the IMAP RENAME has succeeded.
    pub fn rename_folder(
        &self,
        account_id: i64,
        old_remote_name: &str,
        new_name: &str,
        new_remote_name: &str,
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let old_name: Option<String> = transaction
            .query_row(
                "SELECT name FROM folders WHERE account_id = ?1 AND remote_name = ?2",
                params![account_id, old_remote_name],
                |row| row.get(0),
            )
            .optional()?;
        let Some(old_name) = old_name else {
            return Ok(());
        };
        transaction.execute(
            "UPDATE folders SET name = ?1, remote_name = ?2
             WHERE account_id = ?3 AND remote_name = ?4",
            params![new_name, new_remote_name, account_id, old_remote_name],
        )?;
        transaction.execute(
            "UPDATE messages SET folder = ?1
             WHERE account_id = ?2 AND folder = ?3",
            params![new_name, account_id, old_name],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Removes cached folder metadata and its cached messages after the IMAP
    /// DELETE has succeeded. Pending actions for those messages are removed
    /// by the foreign-key cascade.
    pub fn delete_folder(
        &self,
        account_id: i64,
        remote_name: &str,
        local_name: &str,
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM messages WHERE account_id = ?1 AND folder = ?2",
            params![account_id, local_name],
        )?;
        transaction.execute(
            "DELETE FROM folders WHERE account_id = ?1 AND remote_name = ?2",
            params![account_id, remote_name],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn save_draft(
        &self,
        account_id: Option<i64>,
        recipients: &str,
        subject: &str,
        body: &str,
    ) -> Result<i64> {
        self.save_draft_with_html(account_id, recipients, subject, body, None)
    }

    pub fn save_draft_with_html(
        &self,
        account_id: Option<i64>,
        recipients: &str,
        subject: &str,
        body: &str,
        body_html: Option<&str>,
    ) -> Result<i64> {
        self.save_draft_with_html_and_attachments(
            account_id,
            recipients,
            subject,
            body,
            body_html,
            &[],
        )
    }

    pub fn save_draft_with_html_and_attachments(
        &self,
        account_id: Option<i64>,
        recipients: &str,
        subject: &str,
        body: &str,
        body_html: Option<&str>,
        attachments: &[AttachmentInfo],
    ) -> Result<i64> {
        let body_html = body_html.map(crate::mail::mime::sanitize_html);
        let attachments_json = serde_json::to_string(attachments)
            .map_err(|error| DatabaseError::InvalidValue(error.to_string()))?;
        let connection = self.connection()?;
        let id = -Utc::now().timestamp_micros();
        connection.execute(
            "INSERT INTO messages(id, account_id, folder, sender_name, sender_email, recipients, subject, preview, body, body_html, received_at, unread, starred, has_attachments, thread_size, attachments_json)\n             VALUES (?1, ?2, 'Drafts', '', '', ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, ?9, 1, ?10)",
            params![
                id,
                account_id,
                recipients,
                subject,
                body.chars().take(160).collect::<String>(),
                body,
                body_html,
                Utc::now().to_rfc3339(),
                (!attachments.is_empty()) as i64,
                attachments_json,
            ],
        )?;
        Ok(id)
    }

    pub fn update_draft(
        &self,
        draft_id: i64,
        account_id: Option<i64>,
        recipients: &str,
        subject: &str,
        body: &str,
    ) -> Result<i64> {
        self.update_draft_with_html(draft_id, account_id, recipients, subject, body, None)
    }

    pub fn update_draft_with_html(
        &self,
        draft_id: i64,
        account_id: Option<i64>,
        recipients: &str,
        subject: &str,
        body: &str,
        body_html: Option<&str>,
    ) -> Result<i64> {
        self.update_draft_with_html_and_attachments(
            draft_id,
            account_id,
            recipients,
            subject,
            body,
            body_html,
            &[],
        )
    }

    pub fn update_draft_with_html_and_attachments(
        &self,
        draft_id: i64,
        account_id: Option<i64>,
        recipients: &str,
        subject: &str,
        body: &str,
        body_html: Option<&str>,
        attachments: &[AttachmentInfo],
    ) -> Result<i64> {
        let body_html = body_html.map(crate::mail::mime::sanitize_html);
        let attachments_json = serde_json::to_string(attachments)
            .map_err(|error| DatabaseError::InvalidValue(error.to_string()))?;
        let connection = self.connection()?;
        let updated = connection.execute(
            "UPDATE messages
             SET account_id = ?1, recipients = ?2, subject = ?3, preview = ?4,
                 body = ?5, body_html = ?6, received_at = ?7,
                 has_attachments = ?8, attachments_json = ?9
             WHERE id = ?10 AND folder = 'Drafts'",
            params![
                account_id,
                recipients,
                subject,
                body.chars().take(160).collect::<String>(),
                body,
                body_html,
                Utc::now().to_rfc3339(),
                (!attachments.is_empty()) as i64,
                attachments_json,
                draft_id,
            ],
        )?;
        if updated > 0 {
            return Ok(draft_id);
        }
        drop(connection);
        self.save_draft_with_html_and_attachments(
            account_id,
            recipients,
            subject,
            body,
            body_html.as_deref(),
            attachments,
        )
    }

    pub fn upsert_messages(&self, messages: &[Message]) -> Result<usize> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut inserted = 0;
        for message in messages {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1)",
                [message.id],
                |row| row.get(0),
            )?;
            if !exists {
                inserted += 1;
            }
            transaction.execute(
                "INSERT INTO messages(id, account_id, folder, remote_uid, uidvalidity, message_id, thread_key, sender_name, sender_email, recipients, subject, preview, body, body_html, received_at, unread, starred, has_attachments, thread_size, attachments_json, list_unsubscribe, list_unsubscribe_one_click)\n                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)\n                 ON CONFLICT(id) DO UPDATE SET\n                     message_id=excluded.message_id, thread_key=excluded.thread_key,\n                     sender_name=excluded.sender_name, sender_email=excluded.sender_email,\n                     recipients=excluded.recipients, subject=excluded.subject, preview=excluded.preview,\n                     body=excluded.body, body_html=excluded.body_html, received_at=excluded.received_at,\n                     unread=excluded.unread, starred=excluded.starred,\n                     has_attachments=excluded.has_attachments, thread_size=excluded.thread_size,\n                     attachments_json=excluded.attachments_json,
                     list_unsubscribe=excluded.list_unsubscribe,
                     list_unsubscribe_one_click=excluded.list_unsubscribe_one_click",
                params![
                    message.id,
                    message.account_id,
                    message.folder,
                    message.remote_uid,
                    message.uidvalidity,
                    message.message_id,
                    message.thread_key,
                    message.sender_name,
                    message.sender_email,
                    message.recipients,
                    message.subject,
                    message.preview,
                    message.body,
                    message.body_html,
                    message.received_at,
                    message.unread as i64,
                    message.starred as i64,
                    message.has_attachments as i64,
                    message.thread_size,
                    serde_json::to_string(&message.attachments)
                        .map_err(|error| DatabaseError::InvalidValue(error.to_string()))?,
                    message.list_unsubscribe,
                    message.list_unsubscribe_one_click as i64,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(inserted)
    }

    pub fn list_messages(&self, account_id: Option<i64>, folder: &str) -> Result<Vec<Message>> {
        self.list_messages_filtered(account_id, Some(folder), false)
    }

    pub fn unread_count(&self, account_id: Option<i64>, folder: &str) -> Result<usize> {
        let connection = self.connection()?;
        let count = connection.query_row(
            "SELECT COUNT(*) FROM messages
             WHERE (?1 IS NULL OR account_id = ?1) AND folder = ?2 AND unread = 1
               AND id NOT IN (SELECT message_id FROM snoozed_messages)",
            params![account_id, folder],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(count.max(0) as usize)
    }

    pub fn list_messages_filtered(
        &self,
        account_id: Option<i64>,
        folder: Option<&str>,
        starred_only: bool,
    ) -> Result<Vec<Message>> {
        self.list_messages_filtered_page(account_id, folder, starred_only, usize::MAX, 0)
    }

    pub fn list_messages_filtered_page(
        &self,
        account_id: Option<i64>,
        folder: Option<&str>,
        starred_only: bool,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Message>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, account_id, folder, remote_uid, uidvalidity, message_id, thread_key, sender_name, sender_email, recipients, subject, preview, body, body_html,\n                    received_at, unread, starred, has_attachments, thread_size, attachments_json, list_unsubscribe, list_unsubscribe_one_click\n             FROM messages\n             WHERE (?1 IS NULL OR account_id = ?1)\n               AND ((?2 IS NULL OR folder = ?2) OR (?2 = 'Snoozed' AND id IN (SELECT message_id FROM snoozed_messages)))\n               AND (?3 = 0 OR starred = 1)\n               AND (?2 = 'Snoozed' OR id NOT IN (SELECT message_id FROM snoozed_messages))\n             ORDER BY received_at DESC, id DESC\n             LIMIT ?4 OFFSET ?5",
        )?;
        let rows = statement.query_map(
            params![
                account_id,
                folder,
                starred_only as i64,
                limit.min(i64::MAX as usize) as i64,
                offset.min(i64::MAX as usize) as i64
            ],
            message_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn search_messages(&self, query: &str) -> Result<Vec<Message>> {
        self.search_messages_page(query, usize::MAX, 0)
    }

    pub fn search_messages_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Message>> {
        let query = fts_query(query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT m.id, m.account_id, m.folder, m.remote_uid, m.uidvalidity, m.message_id, m.thread_key, m.sender_name, m.sender_email, m.recipients, m.subject,\n                    m.preview, m.body, m.body_html, m.received_at, m.unread, m.starred, m.has_attachments, m.thread_size, m.attachments_json, m.list_unsubscribe, m.list_unsubscribe_one_click\n             FROM message_search s JOIN messages m ON m.id = s.rowid\n             WHERE message_search MATCH ?1\n             ORDER BY m.received_at DESC, m.id DESC\n             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = statement.query_map(
            params![
                query,
                limit.min(i64::MAX as usize) as i64,
                offset.min(i64::MAX as usize) as i64
            ],
            message_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn set_unread(&self, message_id: i64, unread: bool) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE messages SET unread = ?1 WHERE id = ?2",
            params![unread as i64, message_id],
        )?;
        Ok(())
    }

    pub fn set_starred(&self, message_id: i64, starred: bool) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE messages SET starred = ?1 WHERE id = ?2",
            params![starred as i64, message_id],
        )?;
        Ok(())
    }

    pub fn list_calendar_events(&self) -> Result<Vec<CalendarEvent>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, title, notes, starts_at, ends_at, location, message_id
             FROM calendar_events ORDER BY starts_at, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(CalendarEvent {
                id: row.get(0)?,
                title: row.get(1)?,
                notes: row.get(2)?,
                starts_at: row.get(3)?,
                ends_at: row.get(4)?,
                location: row.get(5)?,
                message_id: row.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    /// Maps synced event ids to their sync source (iCloud URL or `google:<id>`).
    pub fn calendar_event_sources(&self) -> Result<HashMap<i64, String>> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT event_id, source FROM calendar_sync_links")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<HashMap<_, _>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn pending_calendar_events(&self, after: i64) -> Result<Vec<CalendarEvent>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT event_id FROM calendar_sync_links")?;
        let mapped = statement
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(self
            .list_calendar_events()?
            .into_iter()
            .filter(|e| e.id > after && !mapped.contains(&e.id))
            .collect())
    }

    pub fn apply_calendar_snapshot(
        &self,
        source: &str,
        snapshot: &crate::icloud::Snapshot,
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction()?;
        let existing = {
            let mut statement = tx.prepare(
                "SELECT event_id, remote_key, href FROM calendar_sync_links WHERE source=?1",
            )?;
            statement
                .query_map([source], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let (pending_deletes, pending_updates) = {
            let mut statement =
                tx.prepare("SELECT remote_key, action FROM calendar_changes WHERE source=?1")?;
            let rows = statement
                .query_map([source], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let deletes = rows
                .iter()
                .filter(|(_, a)| a == "delete")
                .map(|(k, _)| k.clone())
                .collect::<HashSet<_>>();
            let updates = rows
                .iter()
                .filter(|(_, a)| a == "update")
                .map(|(k, _)| k.clone())
                .collect::<HashSet<_>>();
            (deletes, updates)
        };
        let mut seen = HashSet::new();
        for remote in &snapshot.events {
            // A local delete or edit not yet accepted remotely wins over a stale snapshot.
            if pending_deletes.contains(&remote.key) {
                continue;
            }
            let id = existing
                .iter()
                .find(|(id, key, _)| !seen.contains(id) && key == &remote.key)
                .or_else(|| {
                    if existing
                        .iter()
                        .filter(|(_, _, href)| href == &remote.href)
                        .count()
                        == 1
                        && snapshot
                            .events
                            .iter()
                            .filter(|e| e.href == remote.href)
                            .count()
                            == 1
                    {
                        existing
                            .iter()
                            .find(|(id, _, href)| !seen.contains(id) && href == &remote.href)
                    } else {
                        None
                    }
                })
                .map(|(id, _, _)| *id)
                .or_else(|| {
                    snapshot
                        .uploaded
                        .iter()
                        .find(|u| u.href == remote.href && !seen.contains(&u.id))
                        .map(|u| u.id)
                });
            let e = &remote.event;
            let id = if let Some(id) = id {
                if !pending_updates.contains(&remote.key) {
                    tx.execute("UPDATE calendar_events SET title=?1, notes=?2, starts_at=?3, ends_at=?4, location=?5 WHERE id=?6",
                    params![e.title,e.notes,e.starts_at,e.ends_at,e.location,id])?;
                }
                id
            } else {
                tx.execute("INSERT INTO calendar_events(title,notes,starts_at,ends_at,location) VALUES (?1,?2,?3,?4,?5)",
                    params![e.title,e.notes,e.starts_at,e.ends_at,e.location])?;
                tx.last_insert_rowid()
            };
            tx.execute("INSERT INTO calendar_sync_links(event_id,source,remote_key,href,recurring) VALUES (?1,?2,?3,?4,?5)
                ON CONFLICT(event_id) DO UPDATE SET remote_key=excluded.remote_key,href=excluded.href,source=excluded.source,recurring=excluded.recurring",
                params![id,source,remote.key,remote.href,remote.recurring as i64])?;
            seen.insert(id);
        }
        for (id, _, _) in existing {
            if !seen.contains(&id) {
                tx.execute("DELETE FROM calendar_events WHERE id=?1", [id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn create_calendar_event(&self, event: &CalendarEvent) -> Result<i64> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO calendar_events(title, notes, starts_at, ends_at, location, message_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.title,
                event.notes,
                event.starts_at,
                event.ends_at,
                event.location,
                event.message_id
            ],
        )?;
        Ok(connection.last_insert_rowid())
    }

    /// Sync identity of an event: (source, remote_key, href, recurring).
    pub fn calendar_event_link(
        &self,
        event_id: i64,
    ) -> Result<Option<(String, String, String, bool)>> {
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                "SELECT source, remote_key, href, recurring FROM calendar_sync_links WHERE event_id = ?1",
                [event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, i64>(3)? != 0)),
            )
            .optional()?)
    }

    /// Saves a local edit. Events that live on a remote calendar also get a
    /// queued change, replayed by the next sync for that calendar.
    pub fn update_calendar_event(&self, event: &CalendarEvent) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction()?;
        let updated = tx.execute(
            "UPDATE calendar_events SET title=?1, notes=?2, starts_at=?3, ends_at=?4, location=?5 WHERE id=?6",
            params![event.title, event.notes, event.starts_at, event.ends_at, event.location, event.id],
        )?;
        if updated == 0 {
            return Err(DatabaseError::InvalidValue(format!(
                "event {} does not exist",
                event.id
            )));
        }
        let link = tx
            .query_row(
                "SELECT source, remote_key, href FROM calendar_sync_links WHERE event_id = ?1",
                [event.id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((source, key, href)) = link {
            tx.execute(
                "DELETE FROM calendar_changes WHERE source=?1 AND remote_key=?2 AND action='update'",
                params![source, key],
            )?;
            tx.execute(
                "INSERT INTO calendar_changes(source, href, remote_key, action, event_json) VALUES (?1,?2,?3,'update',?4)",
                params![source, href, key, serde_json::to_string(event).map_err(|e| DatabaseError::InvalidValue(e.to_string()))?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_calendar_event(&self, event_id: i64) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction()?;
        let link = tx
            .query_row(
                "SELECT source, remote_key, href FROM calendar_sync_links WHERE event_id = ?1",
                [event_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let event = self.calendar_event_in(&tx, event_id)?;
        if let (Some((source, key, href)), Some(event)) = (link, event) {
            tx.execute(
                "DELETE FROM calendar_changes WHERE source=?1 AND remote_key=?2",
                params![source, key],
            )?;
            tx.execute(
                "INSERT INTO calendar_changes(source, href, remote_key, action, event_json) VALUES (?1,?2,?3,'delete',?4)",
                params![source, href, key, serde_json::to_string(&event).map_err(|e| DatabaseError::InvalidValue(e.to_string()))?],
            )?;
        }
        tx.execute("DELETE FROM calendar_events WHERE id = ?1", [event_id])?;
        tx.commit()?;
        Ok(())
    }

    fn calendar_event_in(
        &self,
        connection: &Connection,
        event_id: i64,
    ) -> Result<Option<CalendarEvent>> {
        Ok(connection
            .query_row(
                "SELECT id, title, notes, starts_at, ends_at, location, message_id FROM calendar_events WHERE id = ?1",
                [event_id],
                |row| {
                    Ok(CalendarEvent {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        notes: row.get(2)?,
                        starts_at: row.get(3)?,
                        ends_at: row.get(4)?,
                        location: row.get(5)?,
                        message_id: row.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn pending_calendar_changes(&self, source: &str) -> Result<Vec<CalendarChange>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, source, href, remote_key, action, event_json FROM calendar_changes WHERE source = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([source], |row| {
            let json: String = row.get(5)?;
            let event = serde_json::from_str(&json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok(CalendarChange {
                id: row.get(0)?,
                source: row.get(1)?,
                href: row.get(2)?,
                remote_key: row.get(3)?,
                action: row.get(4)?,
                event,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn clear_calendar_changes(&self, ids: &[i64]) -> Result<()> {
        let connection = self.connection()?;
        for id in ids {
            connection.execute("DELETE FROM calendar_changes WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    /// Records a reminder notification; returns false if it was already sent.
    pub fn claim_reminder(&self, event_key: &str) -> Result<bool> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM reminders_sent WHERE sent_at < ?1",
            [(Utc::now() - chrono::Duration::days(3)).to_rfc3339()],
        )?;
        let inserted = connection.execute(
            "INSERT OR IGNORE INTO reminders_sent(event_key, sent_at) VALUES (?1, ?2)",
            params![event_key, Utc::now().to_rfc3339()],
        )?;
        Ok(inserted == 1)
    }

    pub fn snooze_message(&self, message_id: i64, until: &str) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO snoozed_messages(message_id, until) VALUES (?1, ?2)
             ON CONFLICT(message_id) DO UPDATE SET until = excluded.until",
            params![message_id, until],
        )?;
        Ok(())
    }

    pub fn unsnooze_message(&self, message_id: i64) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM snoozed_messages WHERE message_id = ?1",
            [message_id],
        )?;
        Ok(())
    }

    pub fn snoozed_until(&self, message_id: i64) -> Result<Option<String>> {
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                "SELECT until FROM snoozed_messages WHERE message_id = ?1",
                [message_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn snoozed_count(&self) -> Result<usize> {
        let connection = self.connection()?;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM snoozed_messages", [], |row| {
                row.get(0)
            })?;
        Ok(count.max(0) as usize)
    }

    /// Returns snoozed messages whose time has come, removing their snooze.
    /// Each entry is (message id, account id, folder, subject, sender).
    pub fn wake_snoozed(
        &self,
        now: &str,
    ) -> Result<Vec<(i64, Option<i64>, String, String, String)>> {
        let mut connection = self.connection()?;
        let tx = connection.transaction()?;
        let due = {
            let mut statement = tx.prepare(
                "SELECT m.id, m.account_id, m.folder, m.subject, m.sender_name FROM snoozed_messages s
                 JOIN messages m ON m.id = s.message_id WHERE s.until <= ?1",
            )?;
            statement
                .query_map([now], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        tx.execute("DELETE FROM snoozed_messages WHERE until <= ?1", [now])?;
        tx.commit()?;
        Ok(due)
    }

    /// Builds an address book from people you have written to (weighted
    /// higher) and people who have written to you.
    pub fn contacts(&self) -> Result<Vec<Contact>> {
        let connection = self.connection()?;
        let mut book: std::collections::HashMap<String, Contact> = std::collections::HashMap::new();
        let mut add = |name: &str, email: &str, weight: i64| {
            let email = email.trim().trim_matches(['<', '>']).to_string();
            if !email.contains('@') || email.len() > 254 {
                return;
            }
            let entry = book
                .entry(email.to_ascii_lowercase())
                .or_insert_with(|| Contact {
                    name: String::new(),
                    email: email.clone(),
                    score: 0,
                });
            entry.score += weight;
            let name = name.trim().trim_matches('"').trim();
            if entry.name.is_empty() && !name.is_empty() && !name.contains('@') {
                entry.name = name.to_string();
            }
        };
        {
            let mut statement = connection.prepare(
                "SELECT sender_name, sender_email, COUNT(*) FROM messages
                 WHERE folder NOT IN ('Spam', 'Trash') AND sender_email != ''
                 GROUP BY lower(sender_email)",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (name, email, count) = row?;
                add(&name, &email, count);
            }
        }
        let mut recipient_lists = Vec::new();
        {
            let mut statement = connection
                .prepare("SELECT recipients FROM messages WHERE folder IN ('Sent', 'Drafts')")?;
            for row in statement.query_map([], |row| row.get::<_, String>(0))? {
                // Draft summaries put Cc/Bcc on their own "Cc: ..." lines.
                for line in row?.lines() {
                    let line = line.trim();
                    let line = ["Cc:", "Bcc:"]
                        .iter()
                        .find_map(|prefix| line.strip_prefix(prefix))
                        .unwrap_or(line);
                    recipient_lists.push(line.to_string());
                }
            }
            let mut statement =
                connection.prepare("SELECT to_recipients, cc_json FROM pending_sends")?;
            for row in statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (to, cc) = row?;
                recipient_lists.push(to);
                recipient_lists
                    .extend(serde_json::from_str::<Vec<String>>(&cc).unwrap_or_default());
            }
        }
        for list in recipient_lists {
            let Ok(parsed) = mailparse::addrparse(&list.replace(';', ",")) else {
                continue;
            };
            for address in parsed.iter() {
                match address {
                    mailparse::MailAddr::Single(single) => add(
                        single.display_name.as_deref().unwrap_or(""),
                        &single.addr,
                        5,
                    ),
                    mailparse::MailAddr::Group(group) => {
                        for single in &group.addrs {
                            add(
                                single.display_name.as_deref().unwrap_or(""),
                                &single.addr,
                                5,
                            )
                        }
                    }
                }
            }
        }
        let mut contacts = book.into_values().collect::<Vec<_>>();
        for contact in &mut contacts {
            let lower = contact.email.to_ascii_lowercase();
            if [
                "noreply",
                "no-reply",
                "donotreply",
                "do-not-reply",
                "notifications@",
                "mailer-daemon",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
            {
                contact.score /= 20;
            }
        }
        contacts.sort_by(|a, b| b.score.cmp(&a.score).then(a.email.cmp(&b.email)));
        Ok(contacts)
    }

    pub fn delete_todo_item(&self, todo_id: i64) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction()?;
        tx.execute(
            "INSERT INTO todo_deletions(source, remote_id)
             SELECT source, remote_id FROM todo_sync_links WHERE todo_id = ?1",
            [todo_id],
        )?;
        tx.execute("DELETE FROM todo_items WHERE id = ?1", [todo_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn todo_sync_items(&self, source: &str) -> Result<Vec<TodoSyncItem>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT t.id, t.title, t.notes, t.due_at, t.completed, t.message_id, t.revision, l.remote_id, l.synced_revision
             FROM todo_items t LEFT JOIN todo_sync_links l ON l.todo_id = t.id AND l.source = ?1",
        )?;
        let rows = statement.query_map([source], |row| {
            Ok(TodoSyncItem {
                todo: TodoItem {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    notes: row.get(2)?,
                    due_at: row.get(3)?,
                    completed: row.get::<_, i64>(4)? != 0,
                    message_id: row.get(5)?,
                },
                revision: row.get(6)?,
                remote_id: row.get(7)?,
                synced_revision: row.get(8)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn todo_deletions(&self, source: &str) -> Result<Vec<(i64, String)>> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT id, remote_id FROM todo_deletions WHERE source = ?1")?;
        let rows = statement.query_map([source], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    /// Applies a completed two-way todo sync. Local edits made after the push
    /// (a newer revision) are kept and pushed next time; untouched todos take
    /// the remote values; remote deletions remove untouched local todos.
    pub fn apply_todo_sync(&self, source: &str, outcome: &TodoSyncOutcome) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction()?;
        for (todo_id, remote_id, revision) in &outcome.created {
            tx.execute(
                "INSERT OR REPLACE INTO todo_sync_links(todo_id, source, remote_id, synced_revision) VALUES (?1,?2,?3,?4)",
                params![todo_id, source, remote_id, revision],
            )?;
        }
        for (todo_id, revision) in &outcome.pushed {
            tx.execute(
                "UPDATE todo_sync_links SET synced_revision = ?1 WHERE todo_id = ?2 AND source = ?3",
                params![revision, todo_id, source],
            )?;
        }
        for id in &outcome.deletions_done {
            tx.execute("DELETE FROM todo_deletions WHERE id = ?1", [id])?;
        }
        let links = {
            let mut statement = tx.prepare(
                "SELECT l.todo_id, l.remote_id, l.synced_revision, t.revision FROM todo_sync_links l
                 JOIN todo_items t ON t.id = l.todo_id WHERE l.source = ?1",
            )?;
            statement
                .query_map([source], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let pending_deletes = {
            let mut statement =
                tx.prepare("SELECT remote_id FROM todo_deletions WHERE source = ?1")?;
            statement
                .query_map([source], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<HashSet<_>, _>>()?
        };
        let remote_ids = outcome
            .remote
            .iter()
            .map(|task| task.remote_id.as_str())
            .collect::<HashSet<_>>();
        for (todo_id, remote_id, synced, revision) in &links {
            if remote_ids.contains(remote_id.as_str()) {
                continue;
            }
            if synced == revision {
                tx.execute("DELETE FROM todo_items WHERE id = ?1", [todo_id])?;
            } else {
                // Edited here but deleted remotely: keep it and upload it again.
                tx.execute("DELETE FROM todo_sync_links WHERE todo_id = ?1", [todo_id])?;
            }
        }
        for task in &outcome.remote {
            if pending_deletes.contains(&task.remote_id) {
                continue;
            }
            match links
                .iter()
                .find(|(_, remote_id, _, _)| remote_id == &task.remote_id)
            {
                Some((todo_id, _, synced, revision)) if synced == revision => {
                    tx.execute(
                        "UPDATE todo_items SET title=?1, notes=?2, due_at=?3, completed=?4 WHERE id=?5",
                        params![task.title, task.notes, task.due_at, task.completed as i64, todo_id],
                    )?;
                }
                Some(_) => {}
                None => {
                    tx.execute(
                        "INSERT INTO todo_items(title, notes, due_at, completed, revision) VALUES (?1,?2,?3,?4,1)",
                        params![task.title, task.notes, task.due_at, task.completed as i64],
                    )?;
                    let id = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT INTO todo_sync_links(todo_id, source, remote_id, synced_revision) VALUES (?1,?2,?3,1)",
                        params![id, source, task.remote_id],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_todo_items(&self) -> Result<Vec<TodoItem>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, title, notes, due_at, completed, message_id
             FROM todo_items ORDER BY completed, due_at IS NULL, due_at, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(TodoItem {
                id: row.get(0)?,
                title: row.get(1)?,
                notes: row.get(2)?,
                due_at: row.get(3)?,
                completed: row.get::<_, i64>(4)? != 0,
                message_id: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn create_todo_item(&self, item: &TodoItem) -> Result<i64> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO todo_items(title, notes, due_at, completed, message_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                item.title,
                item.notes,
                item.due_at,
                item.completed as i64,
                item.message_id
            ],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub fn update_todo_item(&self, item: &TodoItem) -> Result<()> {
        let connection = self.connection()?;
        let updated = connection.execute(
            "UPDATE todo_items SET title = ?1, notes = ?2, due_at = ?3, revision = revision + 1 WHERE id = ?4",
            params![item.title, item.notes, item.due_at, item.id],
        )?;
        if updated == 0 {
            return Err(DatabaseError::InvalidValue(format!(
                "todo {} does not exist",
                item.id
            )));
        }
        Ok(())
    }

    pub fn set_todo_completed(&self, todo_id: i64, completed: bool) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE todo_items SET completed = ?1, revision = revision + 1 WHERE id = ?2 AND completed != ?1",
            params![completed as i64, todo_id],
        )?;
        Ok(())
    }

    pub fn move_message(&self, message_id: i64, folder: &str) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE messages SET folder = ?1 WHERE id = ?2",
            params![folder, message_id],
        )?;
        Ok(())
    }

    /// Applies an optimistic move and records its replay payload atomically.
    /// The IMAP worker is still the authority for the remote mailbox; this
    /// transaction only keeps the local cache and its durable intent aligned.
    pub fn move_message_and_queue_action(
        &self,
        account_id: i64,
        message_id: i64,
        source_folder: &str,
        target_folder: &str,
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE messages SET folder = ?1 WHERE id = ?2 AND account_id = ?3",
            params![target_folder, message_id, account_id],
        )?;
        if changed != 1 {
            return Err(DatabaseError::InvalidValue(format!(
                "message {message_id} was not found for account {account_id}"
            )));
        }
        transaction.execute(
            "INSERT INTO pending_actions(account_id, message_id, action, payload_json, created_at)
             VALUES (?1, ?2, 'move', ?3, ?4)",
            params![
                account_id,
                message_id,
                serde_json::json!({
                    "folder": target_folder,
                    "source_folder": source_folder,
                })
                .to_string(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_message(&self, message_id: i64) -> Result<()> {
        let connection = self.connection()?;
        connection.execute("DELETE FROM messages WHERE id = ?1", [message_id])?;
        Ok(())
    }

    /// Removes cached messages from a mailbox after the IMAP server has
    /// acknowledged a permanent deletion. Keeping this separate from the
    /// remote operation prevents a failed empty-trash request from silently
    /// discarding offline-readable messages.
    pub fn delete_messages_in_folder(&self, account_id: i64, folder: &str) -> Result<usize> {
        let connection = self.connection()?;
        let deleted = connection.execute(
            "DELETE FROM messages WHERE account_id = ?1 AND folder = ?2",
            params![account_id, folder],
        )?;
        Ok(deleted)
    }

    /// Removes cached rows that the server has proven absent from a mailbox.
    /// The caller must provide the mailbox's current UIDVALIDITY and the
    /// complete UID set returned by IMAP; recent-message fetch limits are not
    /// sufficient evidence for deletion.
    pub fn reconcile_folder(
        &self,
        account_id: i64,
        folder: &str,
        uidvalidity: u32,
        remote_uids: &[u32],
    ) -> Result<usize> {
        let known_uids = remote_uids.iter().copied().collect::<HashSet<_>>();
        let connection = self.connection()?;
        let stale_ids = {
            let mut statement = connection.prepare(
                "SELECT id, remote_uid, uidvalidity FROM messages
                 WHERE account_id = ?1 AND folder = ?2",
            )?;
            let rows = statement.query_map(params![account_id, folder], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<u32>>(1)?,
                    row.get::<_, Option<u32>>(2)?,
                ))
            })?;
            let existing = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            existing
                .into_iter()
                .filter_map(|(id, remote_uid, cached_uidvalidity)| {
                    (cached_uidvalidity != Some(uidvalidity)
                        || remote_uid.is_none()
                        || !known_uids.contains(&remote_uid.unwrap_or_default()))
                    .then_some(id)
                })
                .collect::<Vec<_>>()
        };
        if stale_ids.is_empty() {
            return Ok(0);
        }
        let transaction = connection.unchecked_transaction()?;
        for id in &stale_ids {
            transaction.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        }
        transaction.commit()?;
        Ok(stale_ids.len())
    }

    /// Reconciles the local mailbox map against an authoritative IMAP LIST
    /// result. Cached messages belonging to a mailbox removed on the server
    /// are removed with its folder metadata.
    pub fn reconcile_folders(&self, account_id: i64, remote_names: &[String]) -> Result<usize> {
        let connection = self.connection()?;
        let stale_folders = {
            let mut statement = connection
                .prepare("SELECT id, name, remote_name FROM folders WHERE account_id = ?1")?;
            let rows = statement.query_map([account_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, _, remote_name)| {
                    !remote_names.iter().any(|known| known == remote_name)
                })
                .collect::<Vec<_>>()
        };
        if stale_folders.is_empty() {
            return Ok(0);
        }
        let transaction = connection.unchecked_transaction()?;
        for (folder_id, local_name, _) in &stale_folders {
            transaction.execute(
                "DELETE FROM messages WHERE account_id = ?1 AND folder = ?2",
                params![account_id, local_name],
            )?;
            transaction.execute("DELETE FROM folders WHERE id = ?1", [folder_id])?;
        }
        transaction.commit()?;
        Ok(stale_folders.len())
    }

    pub fn queue_send(&self, send: &PendingSend) -> Result<i64> {
        let connection = self.connection()?;
        let cc_json = serde_json::to_string(&send.cc)
            .map_err(|error| DatabaseError::InvalidValue(error.to_string()))?;
        let bcc_json = serde_json::to_string(&send.bcc)
            .map_err(|error| DatabaseError::InvalidValue(error.to_string()))?;
        let attachments_json = serde_json::to_string(&send.attachments)
            .map_err(|error| DatabaseError::InvalidValue(error.to_string()))?;
        connection.execute(
            "INSERT INTO pending_sends(account_id, to_recipients, cc_json, bcc_json, subject, body, body_html, attachments_json, created_at, attempts, retryable, next_attempt_at, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                send.account_id,
                send.to,
                cc_json,
                bcc_json,
                send.subject,
                send.body,
                send.body_html,
                attachments_json,
                send.created_at,
                send.attempts,
                send.retryable as i64,
                send.next_attempt_at,
                send.last_error,
            ],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub fn pending_sends(&self, account_id: Option<i64>) -> Result<Vec<PendingSend>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, account_id, to_recipients, cc_json, bcc_json, subject, body,
                    body_html, attachments_json, created_at, attempts, retryable, next_attempt_at, last_error
             FROM pending_sends
             WHERE sent = 0 AND (?1 IS NULL OR account_id = ?1)
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([account_id], pending_send_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn due_pending_sends(&self, account_id: i64) -> Result<Vec<PendingSend>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, account_id, to_recipients, cc_json, bcc_json, subject, body,
                    body_html, attachments_json, created_at, attempts, retryable, next_attempt_at, last_error
             FROM pending_sends
             WHERE sent = 0 AND account_id = ?1 AND retryable = 1
               AND (next_attempt_at IS NULL OR next_attempt_at <= ?2)
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map(
            params![account_id, Utc::now().to_rfc3339()],
            pending_send_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn record_send_failure(
        &self,
        send_id: i64,
        error: &str,
        retryable: bool,
        next_attempt_at: Option<&str>,
    ) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE pending_sends
             SET attempts = attempts + 1, retryable = ?1, next_attempt_at = ?2, last_error = ?3
             WHERE id = ?4 AND sent = 0",
            params![retryable as i64, next_attempt_at, error, send_id],
        )?;
        Ok(())
    }

    pub fn retry_pending_send(&self, send_id: i64) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE pending_sends
             SET retryable = 1, next_attempt_at = ?1, last_error = NULL
             WHERE id = ?2 AND sent = 0",
            params![Utc::now().to_rfc3339(), send_id],
        )?;
        Ok(())
    }

    pub fn mark_pending_send_sent(&self, send_id: i64) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE pending_sends SET sent = 1, last_error = NULL WHERE id = ?1",
            [send_id],
        )?;
        Ok(())
    }

    pub fn delete_pending_send(&self, send_id: i64) -> Result<()> {
        let connection = self.connection()?;
        connection.execute("DELETE FROM pending_sends WHERE id = ?1", [send_id])?;
        Ok(())
    }

    pub fn queue_action(
        &self,
        account_id: Option<i64>,
        message_id: Option<i64>,
        action: &str,
        payload_json: &str,
    ) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO pending_actions(account_id, message_id, action, payload_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![account_id, message_id, action, payload_json, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Restores optimistic read/star state after a server refresh. Pending
    /// actions are deliberately applied in insertion order so the latest
    /// offline toggle wins until the IMAP worker acknowledges it.
    pub fn reapply_pending_actions(&self, account_id: i64) -> Result<usize> {
        let actions = self.pending_actions(account_id)?;
        let connection = self.connection()?;
        let mut changed = 0;
        for action in actions {
            let Some(message_id) = action.message_id else {
                continue;
            };
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&action.payload_json)
            else {
                continue;
            };
            let Some(value) = payload.get("value").and_then(serde_json::Value::as_bool) else {
                continue;
            };
            let updated = match action.action.as_str() {
                "read" => connection.execute(
                    "UPDATE messages SET unread = ?1 WHERE id = ?2",
                    params![value as i64, message_id],
                )?,
                "star" => connection.execute(
                    "UPDATE messages SET starred = ?1 WHERE id = ?2",
                    params![value as i64, message_id],
                )?,
                _ => 0,
            };
            changed += updated;
        }
        Ok(changed)
    }

    pub fn pending_actions(&self, account_id: i64) -> Result<Vec<PendingAction>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT p.id, p.account_id, p.message_id, p.action, p.payload_json,
                    m.folder, m.remote_uid, m.uidvalidity
             FROM pending_actions p
             LEFT JOIN messages m ON m.id = p.message_id
             WHERE p.account_id = ?1
             ORDER BY p.id",
        )?;
        let rows = statement.query_map([account_id], |row| {
            Ok(PendingAction {
                id: row.get(0)?,
                account_id: row.get(1)?,
                message_id: row.get(2)?,
                action: row.get(3)?,
                payload_json: row.get(4)?,
                folder: row.get(5)?,
                remote_uid: row.get(6)?,
                uidvalidity: row.get(7)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn delete_pending_action(&self, action_id: i64) -> Result<()> {
        let connection = self.connection()?;
        connection.execute("DELETE FROM pending_actions WHERE id = ?1", [action_id])?;
        Ok(())
    }

    /// Cancels the newest matching queued move and restores the cached folder
    /// in one transaction. If the sync worker has already removed the action,
    /// returning false avoids fabricating an unsafe inverse move with a UID
    /// that may have changed on the destination mailbox.
    pub fn undo_pending_move(
        &self,
        account_id: i64,
        message_id: i64,
        source_folder: &str,
        target_folder: &str,
    ) -> Result<bool> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut statement = transaction.prepare(
            "SELECT id, payload_json
             FROM pending_actions
             WHERE account_id = ?1 AND message_id = ?2 AND action = 'move'
             ORDER BY id DESC",
        )?;
        let action_id = statement
            .query_map(params![account_id, message_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|row| row.ok())
            .find_map(|(id, payload)| {
                let payload = serde_json::from_str::<serde_json::Value>(&payload).ok()?;
                (payload
                    .get("source_folder")
                    .and_then(serde_json::Value::as_str)
                    == Some(source_folder)
                    && payload.get("folder").and_then(serde_json::Value::as_str)
                        == Some(target_folder))
                .then_some(id)
            });
        drop(statement);
        let Some(action_id) = action_id else {
            return Ok(false);
        };
        transaction.execute("DELETE FROM pending_actions WHERE id = ?1", [action_id])?;
        transaction.execute(
            "UPDATE messages SET folder = ?1 WHERE id = ?2",
            params![source_folder, message_id],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter_map(|term| {
            let term = term.trim_matches(['"', '\'']);
            let term = term.replace('"', "\"\"");
            (!term.is_empty()).then(|| format!("\"{term}\"*"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let attachments_json: String = row.get(19)?;
    let attachments = serde_json::from_str(&attachments_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(19, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Message {
        id: row.get(0)?,
        account_id: row.get(1)?,
        folder: row.get(2)?,
        remote_uid: row.get(3)?,
        uidvalidity: row.get(4)?,
        message_id: row.get(5)?,
        thread_key: row.get(6)?,
        sender_name: row.get(7)?,
        sender_email: row.get(8)?,
        recipients: row.get(9)?,
        subject: row.get(10)?,
        preview: row.get(11)?,
        body: row.get(12)?,
        body_html: row.get(13)?,
        received_at: row.get(14)?,
        unread: row.get::<_, i64>(15)? != 0,
        starred: row.get::<_, i64>(16)? != 0,
        has_attachments: row.get::<_, i64>(17)? != 0,
        attachments,
        thread_size: row.get(18)?,
        list_unsubscribe: row.get(20)?,
        list_unsubscribe_one_click: row.get::<_, i64>(21)? != 0,
    })
}

fn pending_send_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingSend> {
    let cc_json: String = row.get(3)?;
    let bcc_json: String = row.get(4)?;
    let body_html: Option<String> = row.get(7)?;
    let attachments_json: String = row.get(8)?;
    let cc = serde_json::from_str(&cc_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let bcc = serde_json::from_str(&bcc_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let attachments =
        serde_json::from_str::<Vec<OutgoingAttachment>>(&attachments_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    Ok(PendingSend {
        id: row.get(0)?,
        account_id: row.get(1)?,
        to: row.get(2)?,
        cc,
        bcc,
        subject: row.get(5)?,
        body: row.get(6)?,
        body_html,
        attachments,
        created_at: row.get(9)?,
        attempts: row.get::<_, i64>(10)?.max(0) as u32,
        retryable: row.get::<_, i64>(11)? != 0,
        next_attempt_at: row.get(12)?,
        last_error: row.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AuthMethod, OutgoingAttachment, PendingSend, SecurityMode};
    use tempfile::tempdir;

    #[test]
    fn todo_edits_persist_without_overwriting_completion_or_message_link() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        let messages = Message::demo_messages();
        database.upsert_messages(&messages).unwrap();
        let mut item = TodoItem {
            id: 0,
            title: "Original".into(),
            notes: "Original notes".into(),
            due_at: Some("2026-10-12".into()),
            completed: false,
            message_id: Some(messages[0].id),
        };
        item.id = database.create_todo_item(&item).unwrap();
        let mut untouched = item.clone();
        untouched.id = database.create_todo_item(&untouched).unwrap();
        database.set_todo_completed(item.id, true).unwrap();

        // An editor may still have the values from before the completion toggle.
        item.title = "Edited title".into();
        item.notes = "First line\nSecond line".into();
        item.due_at = Some("2026-10-20".into());
        item.message_id = None;
        database.update_todo_item(&item).unwrap();
        let mut expected = item.clone();
        expected.completed = true;
        expected.message_id = Some(messages[0].id);

        let reopened = Database::open(directory.path()).unwrap();
        let stored = reopened.list_todo_items().unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(
            stored.iter().find(|todo| todo.id == item.id),
            Some(&expected)
        );
        assert_eq!(
            stored.iter().find(|todo| todo.id == untouched.id),
            Some(&untouched)
        );

        item.due_at = None;
        reopened.update_todo_item(&item).unwrap();
        expected.due_at = None;
        let stored = database.list_todo_items().unwrap();
        assert_eq!(
            stored.iter().find(|todo| todo.id == item.id),
            Some(&expected)
        );

        reopened.set_todo_completed(item.id, false).unwrap();
        expected.completed = false;
        let stored = database.list_todo_items().unwrap();
        assert_eq!(
            stored.iter().find(|todo| todo.id == item.id),
            Some(&expected)
        );
    }

    #[test]
    fn updating_a_missing_todo_returns_an_error_without_creating_one() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        let item = TodoItem {
            id: 42,
            title: "Missing".into(),
            notes: String::new(),
            due_at: None,
            completed: false,
            message_id: None,
        };
        assert!(matches!(
            database.update_todo_item(&item),
            Err(DatabaseError::InvalidValue(_))
        ));
        assert!(database.list_todo_items().unwrap().is_empty());
    }

    #[test]
    #[ignore = "Migrates a copy of a real database named by OMARCHY_MAIL_MIGRATION_COPY"]
    fn migrates_a_real_database_copy() {
        let Some(directory) = std::env::var_os("OMARCHY_MAIL_MIGRATION_COPY") else {
            return;
        };
        let before = std::time::Instant::now();
        let database = Database::open(&directory).unwrap();
        eprintln!("migrated in {:?}", before.elapsed());
        let before = std::time::Instant::now();
        let contacts = database.contacts().unwrap();
        eprintln!("{} contacts in {:?}", contacts.len(), before.elapsed());
        let inbox = database.list_messages(None, "Inbox").unwrap().len();
        let first = database.list_messages(None, "Inbox").unwrap()[0].id;
        database
            .snooze_message(first, "2099-01-01T00:00:00+00:00")
            .unwrap();
        assert_eq!(
            database.list_messages(None, "Inbox").unwrap().len(),
            inbox - 1
        );
        assert_eq!(database.list_messages(None, "Snoozed").unwrap().len(), 1);
        eprintln!(
            "inbox {inbox}, events {}",
            database.list_calendar_events().unwrap().len()
        );
    }

    fn sample_event() -> CalendarEvent {
        CalendarEvent {
            id: 0,
            title: "Dentist".into(),
            notes: String::new(),
            starts_at: "2026-09-10 12:00".into(),
            ends_at: "2026-09-10 13:00".into(),
            location: String::new(),
            message_id: None,
        }
    }

    #[test]
    fn remote_event_edits_and_deletes_are_queued_and_survive_stale_snapshots() {
        use crate::icloud::{RemoteEvent, Snapshot};
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        let remote = |title: &str, key: &str| RemoteEvent {
            key: key.into(),
            href: format!("/{key}.ics"),
            event: CalendarEvent {
                title: title.into(),
                ..sample_event()
            },
            recurring: key == "series",
        };
        let snapshot = Snapshot {
            events: vec![
                remote("Dentist", "a"),
                remote("Gym", "b"),
                remote("Weekly", "series"),
            ],
            uploaded: vec![],
            applied: vec![],
            dropped: vec![],
        };
        database
            .apply_calendar_snapshot("icloud", &snapshot)
            .unwrap();
        let events = database.list_calendar_events().unwrap();
        let find = |title: &str| events.iter().find(|e| e.title == title).unwrap().clone();
        assert!(
            database
                .calendar_event_link(find("Weekly").id)
                .unwrap()
                .unwrap()
                .3
        );

        // A purely local event changes without queuing anything remote.
        let local = database.create_calendar_event(&sample_event()).unwrap();
        database
            .update_calendar_event(&CalendarEvent {
                id: local,
                title: "Local".into(),
                ..sample_event()
            })
            .unwrap();
        assert!(
            database
                .pending_calendar_changes("icloud")
                .unwrap()
                .is_empty()
        );

        let mut dentist = find("Dentist");
        dentist.title = "Dentist (moved)".into();
        database.update_calendar_event(&dentist).unwrap();
        dentist.location = "High St".into();
        database.update_calendar_event(&dentist).unwrap();
        database.delete_calendar_event(find("Gym").id).unwrap();
        let changes = database.pending_calendar_changes("icloud").unwrap();
        assert_eq!(changes.len(), 2, "repeated edits collapse into one update");
        assert_eq!(changes[0].action, "update");
        assert_eq!(changes[0].event.location, "High St");
        assert_eq!(
            (changes[1].action.as_str(), changes[1].href.as_str()),
            ("delete", "/b.ics")
        );

        // A snapshot taken before the changes reached the server must not undo them.
        database
            .apply_calendar_snapshot("icloud", &snapshot)
            .unwrap();
        let titles = database
            .list_calendar_events()
            .unwrap()
            .into_iter()
            .map(|e| e.title)
            .collect::<Vec<_>>();
        assert!(titles.contains(&"Dentist (moved)".to_string()));
        assert!(!titles.contains(&"Gym".to_string()));

        database
            .clear_calendar_changes(&changes.iter().map(|c| c.id).collect::<Vec<_>>())
            .unwrap();
        assert!(
            database
                .pending_calendar_changes("icloud")
                .unwrap()
                .is_empty()
        );
        assert!(database.claim_reminder("icloud#a").unwrap());
        assert!(!database.claim_reminder("icloud#a").unwrap());
    }

    #[test]
    fn snoozed_messages_leave_their_folder_until_they_wake() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        let messages = Message::demo_messages();
        database.upsert_messages(&messages).unwrap();
        let inbox_before = database.list_messages(None, "Inbox").unwrap().len();
        let id = messages.iter().find(|m| m.folder == "Inbox").unwrap().id;
        database
            .snooze_message(id, "2026-11-12T09:00:00+00:00")
            .unwrap();
        assert_eq!(
            database.list_messages(None, "Inbox").unwrap().len(),
            inbox_before - 1
        );
        let snoozed = database.list_messages(None, "Snoozed").unwrap();
        assert_eq!(snoozed.iter().map(|m| m.id).collect::<Vec<_>>(), vec![id]);
        assert_eq!(snoozed[0].folder, "Inbox");
        assert!(
            database
                .wake_snoozed("2026-11-11T09:00:00+00:00")
                .unwrap()
                .is_empty()
        );
        let woke = database.wake_snoozed("2026-11-12T09:00:00+00:00").unwrap();
        assert_eq!(woke[0].0, id);
        assert_eq!(
            database.list_messages(None, "Inbox").unwrap().len(),
            inbox_before
        );
        assert_eq!(database.snoozed_count().unwrap(), 0);
    }

    #[test]
    fn contacts_rank_people_you_write_to_above_newsletters() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        let mut messages = Message::demo_messages();
        for (index, message) in messages.iter_mut().enumerate() {
            message.id = 100 + index as i64;
            message.folder = "Inbox".into();
            message.sender_name = "News".into();
            message.sender_email = "noreply@shop.example".into();
        }
        messages[0].folder = "Sent".into();
        messages[0].recipients = "\"Jane Smith\" <jane@example.com>, bob@example.com".into();
        database.upsert_messages(&messages).unwrap();
        let contacts = database.contacts().unwrap();
        let jane = contacts
            .iter()
            .position(|c| c.email == "jane@example.com")
            .unwrap();
        assert_eq!(contacts[jane].name, "Jane Smith");
        assert!(
            jane < 2
                && contacts
                    .iter()
                    .take(2)
                    .any(|c| c.email == "bob@example.com")
        );
        assert_eq!(contacts.last().unwrap().email, "noreply@shop.example");
    }

    #[test]
    fn todo_sync_merges_both_directions_without_losing_local_edits() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        let todo = |title: &str| TodoItem {
            id: 0,
            title: title.into(),
            notes: String::new(),
            due_at: None,
            completed: false,
            message_id: None,
        };
        let a = database.create_todo_item(&todo("A")).unwrap();
        let b = database.create_todo_item(&todo("B")).unwrap();
        let items = database.todo_sync_items("g").unwrap();
        assert!(items.iter().all(|item| item.remote_id.is_none()));
        let remote = |id: &str, title: &str| RemoteTask {
            remote_id: id.into(),
            title: title.into(),
            notes: String::new(),
            due_at: Some("2026-12-01".into()),
            completed: false,
        };
        database
            .apply_todo_sync(
                "g",
                &TodoSyncOutcome {
                    created: vec![(a, "ra".into(), 1), (b, "rb".into(), 1)],
                    remote: vec![
                        remote("ra", "A"),
                        remote("rb", "B"),
                        remote("rc", "From phone"),
                    ],
                    ..Default::default()
                },
            )
            .unwrap();
        let titles = |db: &Database| {
            db.list_todo_items()
                .unwrap()
                .into_iter()
                .map(|t| t.title)
                .collect::<Vec<_>>()
        };
        assert_eq!(titles(&database).len(), 3);

        // B is edited locally while the phone edits A and deletes the imported task.
        database
            .update_todo_item(&TodoItem {
                id: b,
                ..todo("B edited here")
            })
            .unwrap();
        database
            .apply_todo_sync(
                "g",
                &TodoSyncOutcome {
                    remote: vec![remote("ra", "A edited on phone"), remote("rb", "B")],
                    ..Default::default()
                },
            )
            .unwrap();
        let now = titles(&database);
        assert!(now.contains(&"A edited on phone".to_string()));
        assert!(now.contains(&"B edited here".to_string()));
        assert!(!now.contains(&"From phone".to_string()));
        let b_item = database
            .todo_sync_items("g")
            .unwrap()
            .into_iter()
            .find(|i| i.todo.id == b)
            .unwrap();
        assert!(
            b_item.revision > b_item.synced_revision.unwrap(),
            "B still needs pushing"
        );

        database.delete_todo_item(a).unwrap();
        let deletions = database.todo_deletions("g").unwrap();
        assert_eq!(deletions[0].1, "ra");
        // The stale remote list still has A; the pending deletion keeps it gone.
        database
            .apply_todo_sync(
                "g",
                &TodoSyncOutcome {
                    pushed: vec![(b, b_item.revision)],
                    remote: vec![remote("ra", "A"), remote("rb", "B edited here")],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(titles(&database), vec!["B edited here".to_string()]);
        database
            .apply_todo_sync(
                "g",
                &TodoSyncOutcome {
                    deletions_done: vec![deletions[0].0],
                    remote: vec![remote("rb", "B edited here")],
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(database.todo_deletions("g").unwrap().is_empty());
    }

    #[test]
    fn calendar_sync_reuses_uploaded_ids_and_preserves_local_events() {
        use crate::icloud::{RemoteEvent, Snapshot, Uploaded};
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        let mut event = CalendarEvent {
            id: 0,
            title: "Local".into(),
            notes: "".into(),
            starts_at: "2026-09-10 12:00".into(),
            ends_at: "2026-09-10 13:00".into(),
            location: "".into(),
            message_id: None,
        };
        let untouched = database.create_calendar_event(&event).unwrap();
        let uploaded = database.create_calendar_event(&event).unwrap();
        event.title = "Changed on Apple Calendar".into();
        let snapshot = Snapshot {
            events: vec![RemoteEvent {
                key: "uid#time".into(),
                href: "/event.ics".into(),
                event: event.clone(),
                recurring: false,
            }],
            uploaded: vec![Uploaded {
                id: uploaded,
                href: "/event.ics".into(),
            }],
            applied: vec![],
            dropped: vec![],
        };
        database
            .apply_calendar_snapshot("icloud", &snapshot)
            .unwrap();
        database
            .apply_calendar_snapshot("icloud", &snapshot)
            .unwrap();
        let stored = database.list_calendar_events().unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(
            stored.iter().find(|e| e.id == uploaded).unwrap().title,
            event.title
        );
        assert_eq!(
            database.pending_calendar_events(0).unwrap()[0].id,
            untouched
        );
        database
            .apply_calendar_snapshot(
                "other-calendar",
                &Snapshot {
                    events: vec![],
                    uploaded: vec![],
                    applied: vec![],
                    dropped: vec![],
                },
            )
            .unwrap();
        assert_eq!(database.list_calendar_events().unwrap().len(), 2);
        database
            .apply_calendar_snapshot(
                "icloud",
                &Snapshot {
                    events: vec![],
                    uploaded: vec![],
                    applied: vec![],
                    dropped: vec![],
                },
            )
            .unwrap();
        assert_eq!(database.list_calendar_events().unwrap()[0].id, untouched);
        assert_eq!(database.list_calendar_events().unwrap().len(), 1);
    }

    #[test]
    fn invalid_calendar_snapshot_rolls_back_updates() {
        use crate::icloud::{RemoteEvent, Snapshot};
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        let event = CalendarEvent {
            id: 0,
            title: "Original".into(),
            notes: "".into(),
            starts_at: "2026-09-10 12:00".into(),
            ends_at: "2026-09-10 13:00".into(),
            location: "".into(),
            message_id: None,
        };
        let good = Snapshot {
            events: vec![RemoteEvent {
                key: "key".into(),
                href: "/one".into(),
                event: event.clone(),
                recurring: false,
            }],
            uploaded: vec![],
            applied: vec![],
            dropped: vec![],
        };
        database.apply_calendar_snapshot("icloud", &good).unwrap();
        let mut changed = event.clone();
        changed.title = "Should roll back".into();
        let bad = Snapshot {
            events: vec![
                RemoteEvent {
                    key: "key".into(),
                    href: "/one".into(),
                    event: changed.clone(),
                    recurring: false,
                },
                RemoteEvent {
                    key: "key".into(),
                    href: "/two".into(),
                    event: changed,
                    recurring: false,
                },
            ],
            uploaded: vec![],
            applied: vec![],
            dropped: vec![],
        };
        assert!(database.apply_calendar_snapshot("icloud", &bad).is_err());
        assert_eq!(
            database.list_calendar_events().unwrap()[0].title,
            "Original"
        );
    }

    #[test]
    fn creates_schema_and_round_trips_account_without_password() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        assert!(!database.has_accounts().expect("account query"));
        let account = Account::new("jim@example.com", "Jim");
        let id = database.save_account(&account).expect("save account");
        let accounts = database.load_accounts().expect("load accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, Some(id));
        assert!(!std::fs::read_to_string(database.path()).is_ok());
    }

    #[test]
    fn migrates_the_legacy_outbox_with_a_separate_bcc_list() {
        let directory = tempdir().expect("temp directory");
        let path = directory.path().join("mail.db");
        let connection = Connection::open(&path).expect("legacy database");
        connection
            .execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL);
                 INSERT INTO schema_version(version) VALUES (3);
                 CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    body TEXT NOT NULL
                 );
                 CREATE TABLE pending_sends (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    account_id INTEGER NOT NULL,
                    to_recipients TEXT NOT NULL,
                    cc_json TEXT NOT NULL DEFAULT '[]',
                    subject TEXT NOT NULL,
                    body TEXT NOT NULL,
                    attachments_json TEXT NOT NULL DEFAULT '[]',
                    created_at TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    retryable INTEGER NOT NULL DEFAULT 1,
                    next_attempt_at TEXT,
                    last_error TEXT,
                    sent INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .expect("create v3 schema");
        drop(connection);

        let database = Database::open(directory.path()).expect("migrate database");
        let connection = database.connection().expect("open migrated database");
        let bcc_column: String = connection
            .query_row(
                "SELECT name FROM pragma_table_info('pending_sends') WHERE name = 'bcc_json'",
                [],
                |row| row.get(0),
            )
            .expect("bcc migration");
        assert_eq!(bcc_column, "bcc_json");
        let body_html_column: String = connection
            .query_row(
                "SELECT name FROM pragma_table_info('messages') WHERE name = 'body_html'",
                [],
                |row| row.get(0),
            )
            .expect("message html migration");
        assert_eq!(body_html_column, "body_html");
        let outbox_html_column: String = connection
            .query_row(
                "SELECT name FROM pragma_table_info('pending_sends') WHERE name = 'body_html'",
                [],
                |row| row.get(0),
            )
            .expect("outbox html migration");
        assert_eq!(outbox_html_column, "body_html");
    }

    #[test]
    fn searches_cached_messages() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let messages = Message::demo_messages();
        database
            .upsert_messages(&messages)
            .expect("insert messages");
        let found = database.search_messages("garden").expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].sender_name, "Jane Smith");
    }

    #[test]
    fn searches_cached_messages_by_prefix_without_fts_syntax_injection() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        database
            .upsert_messages(&Message::demo_messages())
            .expect("insert messages");

        let prefix = database.search_messages("gard").expect("prefix search");
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].subject, "A quiet afternoon in the garden");

        let punctuation = database
            .search_messages("garden OR 1=1")
            .expect("quoted search");
        assert!(punctuation.is_empty());
    }

    #[test]
    fn counts_unread_messages_by_mailbox() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        database
            .upsert_messages(&Message::demo_messages())
            .expect("insert messages");

        assert_eq!(database.unread_count(None, "Inbox").expect("count"), 2);
        database.set_unread(1, false).expect("mark read");
        assert_eq!(database.unread_count(None, "Inbox").expect("count"), 1);
    }

    #[test]
    fn round_trips_attachment_metadata_with_cached_message() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let mut message = Message::demo_messages().remove(0);
        message.attachments = vec![crate::models::AttachmentInfo {
            filename: "notes.txt".into(),
            content_type: "text/plain".into(),
            size: 5,
            cache_path: "/tmp/notes.txt".into(),
            content_id: Some("notes@example.com".into()),
        }];
        database
            .upsert_messages(&[message])
            .expect("insert message");

        let loaded = database.list_messages(None, "Inbox").expect("load message");
        assert_eq!(loaded[0].attachments[0].filename, "notes.txt");
        assert_eq!(loaded[0].attachments[0].size, 5);
        assert_eq!(
            loaded[0].attachments[0].content_id.as_deref(),
            Some("notes@example.com")
        );
    }

    #[test]
    fn refreshes_existing_message_metadata_and_restores_pending_flags() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let account_id = database
            .save_account(&Account::new("jim@example.com", "Jim"))
            .expect("account");
        let mut message = Message::demo_messages().remove(0);
        message.account_id = Some(account_id);
        message.has_attachments = false;
        message.attachments.clear();
        message.unread = true;
        message.starred = false;
        database
            .upsert_messages(&[message.clone()])
            .expect("initial message");

        database
            .queue_action(
                Some(account_id),
                Some(message.id),
                "read",
                r#"{"value":false,"folder":"Inbox"}"#,
            )
            .expect("queue read action");
        database
            .queue_action(
                Some(account_id),
                Some(message.id),
                "star",
                r#"{"value":true,"folder":"Inbox"}"#,
            )
            .expect("queue star action");

        message.sender_name = "Updated sender".into();
        message.subject = "Updated subject".into();
        message.has_attachments = true;
        message.attachments = vec![AttachmentInfo {
            filename: "updated.txt".into(),
            content_type: "text/plain".into(),
            size: 7,
            cache_path: "/tmp/updated.txt".into(),
            content_id: None,
        }];
        message.unread = true;
        message.starred = false;
        database
            .upsert_messages(&[message])
            .expect("refreshed message");
        database
            .reapply_pending_actions(account_id)
            .expect("restore pending flags");

        let loaded = database
            .list_messages(Some(account_id), "Inbox")
            .expect("load refreshed message");
        assert_eq!(loaded[0].sender_name, "Updated sender");
        assert_eq!(loaded[0].subject, "Updated subject");
        assert!(loaded[0].has_attachments);
        assert!(!loaded[0].unread);
        assert!(loaded[0].starred);
    }

    #[test]
    fn filters_cached_messages_by_folder_and_star() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        database
            .upsert_messages(&Message::demo_messages())
            .expect("insert messages");
        database.move_message(1, "Archive").expect("move message");

        let archived = database
            .list_messages_filtered(None, Some("Archive"), false)
            .expect("list archive");
        let starred = database
            .list_messages_filtered(None, None, true)
            .expect("list starred");

        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, 1);
        assert_eq!(starred.len(), 1);
        assert_eq!(starred[0].id, 1);
    }

    #[test]
    fn deletes_only_the_cached_messages_in_an_emptied_folder() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let account_id = database
            .save_account(&Account::new("jim@example.com", "Jim"))
            .expect("account");
        let mut messages = Message::demo_messages();
        messages.truncate(2);
        for message in &mut messages {
            message.account_id = Some(account_id);
        }
        database.upsert_messages(&messages).expect("messages");
        database.move_message(1, "Trash").expect("trash message");

        assert_eq!(
            database
                .delete_messages_in_folder(account_id, "Trash")
                .expect("empty cached trash"),
            1
        );
        assert!(
            database
                .list_messages(Some(account_id), "Trash")
                .expect("trash")
                .is_empty()
        );
        assert_eq!(
            database
                .list_messages(Some(account_id), "Inbox")
                .expect("inbox")
                .len(),
            1
        );
    }

    #[test]
    fn paginates_cached_messages_with_stable_received_order() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        database
            .upsert_messages(&Message::demo_messages())
            .expect("insert messages");

        let first = database
            .list_messages_filtered_page(None, Some("Inbox"), false, 2, 0)
            .expect("first page");
        let second = database
            .list_messages_filtered_page(None, Some("Inbox"), false, 2, 2)
            .expect("second page");
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        assert!(
            first
                .iter()
                .all(|message| { second.iter().all(|other| message.id != other.id) })
        );
    }

    #[test]
    fn reconciles_deleted_messages_and_uidvalidity_changes() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let account_id = database
            .save_account(&Account::new("jim@example.com", "Jim"))
            .expect("account");
        let mut messages = Message::demo_messages();
        messages.truncate(3);
        for message in &mut messages {
            message.account_id = Some(account_id);
            message.remote_uid = Some(40 + message.id as u32);
            message.uidvalidity = Some(if message.id == 3 { 6 } else { 7 });
        }
        database.upsert_messages(&messages).expect("messages");

        assert_eq!(
            database
                .reconcile_folder(account_id, "Inbox", 7, &[41])
                .expect("reconcile"),
            2
        );
        let remaining = database
            .list_messages(Some(account_id), "Inbox")
            .expect("remaining messages");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].remote_uid, Some(41));

        let found = database.search_messages("Saturday").expect("search");
        assert!(found.is_empty());
    }

    #[test]
    fn round_trips_server_folder_metadata() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let account = Account::new("jim@example.com", "Jim");
        let account_id = database.save_account(&account).expect("save account");
        database
            .upsert_folders(&[
                MailFolder {
                    account_id,
                    name: "Inbox".into(),
                    remote_name: "INBOX".into(),
                    kind: "inbox".into(),
                    unread_count: 4,
                },
                MailFolder {
                    account_id,
                    name: "Receipts".into(),
                    remote_name: "Archive/Receipts".into(),
                    kind: "custom".into(),
                    unread_count: 0,
                },
            ])
            .expect("save folders");

        let folders = database.load_folders().expect("load folders");
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].name, "Inbox");
        assert_eq!(folders[0].unread_count, 4);
        assert_eq!(folders[1].remote_name, "Archive/Receipts");

        assert_eq!(
            database
                .reconcile_folders(account_id, &["INBOX".into()])
                .expect("reconcile folders"),
            1
        );
        assert_eq!(database.load_folders().expect("reload folders").len(), 1);
    }

    #[test]
    fn updates_cached_messages_when_a_custom_folder_is_renamed_or_deleted() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let account_id = database
            .save_account(&Account::new("jim@example.com", "Jim"))
            .expect("account");
        database
            .create_folder(&MailFolder {
                account_id,
                name: "Receipts".into(),
                remote_name: "Receipts".into(),
                kind: "custom".into(),
                unread_count: 1,
            })
            .expect("create folder cache");
        let mut message = Message::demo_messages().remove(0);
        message.account_id = Some(account_id);
        message.folder = "Receipts".into();
        message.remote_uid = Some(22);
        message.uidvalidity = Some(8);
        database.upsert_messages(&[message]).expect("message");

        database
            .rename_folder(account_id, "Receipts", "Invoices", "Invoices")
            .expect("rename folder cache");
        let renamed = database
            .list_messages(Some(account_id), "Invoices")
            .expect("renamed messages");
        assert_eq!(renamed.len(), 1);
        assert_eq!(
            database
                .load_folders()
                .expect("renamed folders")
                .iter()
                .find(|folder| folder.account_id == account_id)
                .map(|folder| folder.remote_name.as_str()),
            Some("Invoices")
        );

        database
            .delete_folder(account_id, "Invoices", "Invoices")
            .expect("delete folder cache");
        assert!(
            database
                .list_messages(Some(account_id), "Invoices")
                .expect("deleted messages")
                .is_empty()
        );
        assert!(
            database
                .load_folders()
                .expect("deleted folders")
                .iter()
                .all(|folder| folder.remote_name != "Invoices")
        );
    }

    #[test]
    fn queues_and_updates_an_outgoing_message_without_storing_credentials() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let account_id = database
            .save_account(&Account::new("jim@example.com", "Jim"))
            .expect("account");
        let send = PendingSend {
            id: 0,
            account_id,
            to: "jane@example.com".into(),
            cc: vec!["team@example.com".into()],
            bcc: vec!["archive@example.com".into()],
            subject: "Offline note".into(),
            body: "This should wait for the network.".into(),
            body_html: None,
            attachments: vec![OutgoingAttachment {
                filename: "notes.txt".into(),
                path: "/tmp/omarchy-mail-notes.txt".into(),
            }],
            created_at: Utc::now().to_rfc3339(),
            attempts: 1,
            retryable: true,
            next_attempt_at: None,
            last_error: Some("network unavailable".into()),
        };
        let id = database.queue_send(&send).expect("queue send");
        let loaded = database
            .pending_sends(Some(account_id))
            .expect("load queued send");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, id);
        assert_eq!(loaded[0].cc, send.cc);
        assert_eq!(loaded[0].bcc, send.bcc);
        assert_eq!(loaded[0].attachments, send.attachments);

        database
            .record_send_failure(id, "still offline", true, Some("2999-01-01T00:00:00Z"))
            .expect("record failure");
        let delayed = database
            .due_pending_sends(account_id)
            .expect("load due sends");
        assert!(delayed.is_empty());
        database.retry_pending_send(id).expect("retry send");
        assert_eq!(
            database
                .due_pending_sends(account_id)
                .expect("load retried sends")
                .len(),
            1
        );

        database.mark_pending_send_sent(id).expect("mark sent");
        assert!(
            database
                .pending_sends(Some(account_id))
                .expect("load sent sends")
                .is_empty()
        );
    }

    #[test]
    fn server_config_serializes_without_secret_material() {
        let config = ServerConfig::imap_defaults("example.com", "jim@example.com");
        let encoded = serde_json::to_string(&config).expect("json");
        assert!(!encoded.contains("password"));
        assert_eq!(config.security, SecurityMode::Tls);
        assert_eq!(config.auth, AuthMethod::Password);

        let legacy = r#"{"hostname":"imap.example.com","port":993,"security":"Tls","username":"jim@example.com"}"#;
        let legacy_config: ServerConfig = serde_json::from_str(legacy).expect("legacy config");
        assert_eq!(legacy_config.auth, AuthMethod::Password);
    }

    #[test]
    fn queues_an_action_for_later_reconciliation() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        database
            .queue_action(None, None, "move", r#"{"folder":"Archive"}"#)
            .expect("queue action");
        let connection = rusqlite::Connection::open(database.path()).expect("open database");
        let action: String = connection
            .query_row("SELECT action FROM pending_actions LIMIT 1", [], |row| {
                row.get(0)
            })
            .expect("read action");
        assert_eq!(action, "move");
    }

    #[test]
    fn loads_action_with_remote_message_identity() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let account_id = database
            .save_account(&Account::new("jim@example.com", "Jim"))
            .expect("save account");
        let mut message = Message::demo_messages().remove(0);
        message.account_id = Some(account_id);
        message.remote_uid = Some(42);
        message.uidvalidity = Some(7);
        database.upsert_messages(&[message]).expect("save message");
        database
            .queue_action(
                Some(account_id),
                Some(1),
                "read",
                r#"{"value":false,"folder":"Inbox"}"#,
            )
            .expect("queue action");

        let actions = database.pending_actions(account_id).expect("load actions");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].remote_uid, Some(42));
        assert_eq!(actions[0].uidvalidity, Some(7));
        assert_eq!(actions[0].folder.as_deref(), Some("Inbox"));

        database
            .delete_pending_action(actions[0].id)
            .expect("delete action");
        assert!(
            database
                .pending_actions(account_id)
                .expect("reload actions")
                .is_empty()
        );
    }

    #[test]
    fn undoes_a_queued_move_without_guessing_a_new_remote_uid() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let account_id = database
            .save_account(&Account::new("jim@example.com", "Jim"))
            .expect("save account");
        let mut message = Message::demo_messages().remove(0);
        message.account_id = Some(account_id);
        message.remote_uid = Some(42);
        message.uidvalidity = Some(7);
        database.upsert_messages(&[message]).expect("save message");
        database
            .move_message_and_queue_action(account_id, 1, "Inbox", "Archive")
            .expect("move and queue");

        assert!(
            database
                .undo_pending_move(account_id, 1, "Inbox", "Archive")
                .expect("undo move")
        );
        assert_eq!(
            database
                .list_messages(Some(account_id), "Inbox")
                .expect("load inbox")
                .len(),
            1
        );
        assert!(
            database
                .pending_actions(account_id)
                .expect("load actions")
                .is_empty()
        );
        assert!(
            !database
                .undo_pending_move(account_id, 1, "Inbox", "Archive")
                .expect("repeat undo")
        );
    }

    #[test]
    fn saves_draft_in_the_drafts_folder() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let account = Account::new("jim@example.com", "Jim");
        let account_id = database.save_account(&account).expect("save account");

        let draft_id = database
            .save_draft(
                Some(account_id),
                "jane@example.com",
                "A saved thought",
                "I will finish this later.",
            )
            .expect("save draft");
        let drafts = database
            .list_messages(Some(account_id), "Drafts")
            .expect("list drafts");

        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].id, draft_id);
        assert_eq!(drafts[0].recipients, "jane@example.com");
        assert_eq!(drafts[0].body, "I will finish this later.");
        assert!(!drafts[0].unread);

        database
            .update_draft(
                draft_id,
                Some(account_id),
                "jane@example.com",
                "An updated thought",
                "I changed my mind.",
            )
            .expect("update draft");
        let drafts = database
            .list_messages(Some(account_id), "Drafts")
            .expect("reload drafts");
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].subject, "An updated thought");
        assert_eq!(drafts[0].body, "I changed my mind.");
    }

    #[test]
    fn round_trips_a_sanitized_html_draft_with_plain_fallback() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let draft_id = database
            .save_draft_with_html(
                None,
                "jane@example.com",
                "Styled note",
                "A styled note",
                Some("<p><strong>A styled note</strong></p><script>bad()</script>"),
            )
            .expect("save html draft");

        let draft = database
            .list_messages(None, "Drafts")
            .expect("load html draft")
            .into_iter()
            .find(|message| message.id == draft_id)
            .expect("draft row");
        assert_eq!(draft.body, "A styled note");
        assert_eq!(
            draft.body_html.as_deref(),
            Some("<p><strong>A styled note</strong></p>")
        );
    }

    #[test]
    fn round_trips_draft_attachment_metadata() {
        let directory = tempdir().expect("temp directory");
        let database = Database::open(directory.path()).expect("database");
        let attachment = AttachmentInfo {
            filename: "notes.txt".into(),
            content_type: "text/plain".into(),
            size: 12,
            cache_path: directory
                .path()
                .join("drafts/draft-1/notes.txt")
                .to_string_lossy()
                .into_owned(),
            content_id: None,
        };
        let draft_id = database
            .save_draft_with_html_and_attachments(
                None,
                "jane@example.com",
                "A note with a file",
                "See attached.",
                None,
                std::slice::from_ref(&attachment),
            )
            .expect("save draft");
        let draft = database
            .list_messages(None, "Drafts")
            .expect("load draft")
            .into_iter()
            .find(|draft| draft.id == draft_id)
            .expect("draft row");
        assert!(draft.has_attachments);
        assert_eq!(draft.attachments, vec![attachment]);
    }
}
