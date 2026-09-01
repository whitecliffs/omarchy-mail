use crate::models::{
    Account, MailFolder, Message, OutgoingAttachment, PendingAction, PendingSend, ServerConfig,
};
use chrono::Utc;
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::fs;
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
        fs::create_dir_all(&data_dir)?;
        let database = Self {
            path: data_dir.join("mail.db"),
        };
        database.migrate()?;
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

    pub fn save_draft(
        &self,
        account_id: Option<i64>,
        recipients: &str,
        subject: &str,
        body: &str,
    ) -> Result<i64> {
        let connection = self.connection()?;
        let id = -Utc::now().timestamp_micros();
        connection.execute(
            "INSERT INTO messages(id, account_id, folder, sender_name, sender_email, recipients, subject, preview, body, received_at, unread, starred, has_attachments, thread_size)\n             VALUES (?1, ?2, 'Drafts', '', '', ?3, ?4, ?5, ?6, ?7, 0, 0, 0, 1)",
            params![id, account_id, recipients, subject, body.chars().take(160).collect::<String>(), body, Utc::now().to_rfc3339()],
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
        let connection = self.connection()?;
        let updated = connection.execute(
            "UPDATE messages
             SET account_id = ?1, recipients = ?2, subject = ?3, preview = ?4,
                 body = ?5, received_at = ?6
             WHERE id = ?7 AND folder = 'Drafts'",
            params![
                account_id,
                recipients,
                subject,
                body.chars().take(160).collect::<String>(),
                body,
                Utc::now().to_rfc3339(),
                draft_id,
            ],
        )?;
        if updated > 0 {
            return Ok(draft_id);
        }
        drop(connection);
        self.save_draft(account_id, recipients, subject, body)
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
                "INSERT INTO messages(id, account_id, folder, remote_uid, uidvalidity, message_id, thread_key, sender_name, sender_email, recipients, subject, preview, body, received_at, unread, starred, has_attachments, thread_size, attachments_json)\n                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)\n                 ON CONFLICT(id) DO UPDATE SET unread=excluded.unread, starred=excluded.starred, body=excluded.body, attachments_json=excluded.attachments_json",
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
                    message.received_at,
                    message.unread as i64,
                    message.starred as i64,
                    message.has_attachments as i64,
                    message.thread_size,
                    serde_json::to_string(&message.attachments)
                        .map_err(|error| DatabaseError::InvalidValue(error.to_string()))?,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(inserted)
    }

    pub fn list_messages(&self, account_id: Option<i64>, folder: &str) -> Result<Vec<Message>> {
        self.list_messages_filtered(account_id, Some(folder), false)
    }

    pub fn list_messages_filtered(
        &self,
        account_id: Option<i64>,
        folder: Option<&str>,
        starred_only: bool,
    ) -> Result<Vec<Message>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, account_id, folder, remote_uid, uidvalidity, message_id, thread_key, sender_name, sender_email, recipients, subject, preview, body,\n                    received_at, unread, starred, has_attachments, thread_size, attachments_json\n             FROM messages\n             WHERE (?1 IS NULL OR account_id = ?1)\n               AND (?2 IS NULL OR folder = ?2)\n               AND (?3 = 0 OR starred = 1)\n             ORDER BY id DESC",
        )?;
        let rows = statement.query_map(
            params![account_id, folder, starred_only as i64],
            message_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DatabaseError::from)
    }

    pub fn search_messages(&self, query: &str) -> Result<Vec<Message>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT m.id, m.account_id, m.folder, m.remote_uid, m.uidvalidity, m.message_id, m.thread_key, m.sender_name, m.sender_email, m.recipients, m.subject,\n                    m.preview, m.body, m.received_at, m.unread, m.starred, m.has_attachments, m.thread_size, m.attachments_json\n             FROM message_search s JOIN messages m ON m.id = s.rowid\n             WHERE message_search MATCH ?1 ORDER BY m.id DESC",
        )?;
        let rows = statement.query_map([query], message_from_row)?;
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

    pub fn move_message(&self, message_id: i64, folder: &str) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE messages SET folder = ?1 WHERE id = ?2",
            params![folder, message_id],
        )?;
        Ok(())
    }

    pub fn delete_message(&self, message_id: i64) -> Result<()> {
        let connection = self.connection()?;
        connection.execute("DELETE FROM messages WHERE id = ?1", [message_id])?;
        Ok(())
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
            "INSERT INTO pending_sends(account_id, to_recipients, cc_json, bcc_json, subject, body, attachments_json, created_at, attempts, retryable, next_attempt_at, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                send.account_id,
                send.to,
                cc_json,
                bcc_json,
                send.subject,
                send.body,
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
                    attachments_json, created_at, attempts, retryable, next_attempt_at, last_error
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
                    attachments_json, created_at, attempts, retryable, next_attempt_at, last_error
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

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let attachments_json: String = row.get(18)?;
    let attachments = serde_json::from_str(&attachments_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(18, rusqlite::types::Type::Text, Box::new(error))
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
        received_at: row.get(13)?,
        unread: row.get::<_, i64>(14)? != 0,
        starred: row.get::<_, i64>(15)? != 0,
        has_attachments: row.get::<_, i64>(16)? != 0,
        attachments,
        thread_size: row.get(17)?,
    })
}

fn pending_send_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingSend> {
    let cc_json: String = row.get(3)?;
    let bcc_json: String = row.get(4)?;
    let attachments_json: String = row.get(7)?;
    let cc = serde_json::from_str(&cc_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let bcc = serde_json::from_str(&bcc_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let attachments =
        serde_json::from_str::<Vec<OutgoingAttachment>>(&attachments_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
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
        attachments,
        created_at: row.get(8)?,
        attempts: row.get::<_, i64>(9)?.max(0) as u32,
        retryable: row.get::<_, i64>(10)? != 0,
        next_attempt_at: row.get(11)?,
        last_error: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AuthMethod, OutgoingAttachment, PendingSend, SecurityMode};
    use tempfile::tempdir;

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
}
