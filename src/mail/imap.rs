use crate::mail::mime;
use crate::models::{Account, MailFolder, Message, PendingAction, SecurityMode};
use chrono::Utc;
use imap::types::{Fetch, Name, NameAttribute};
use native_tls::TlsConnector;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ImapError {
    #[error("could not establish a secure IMAP connection: {0}")]
    Tls(#[from] native_tls::Error),
    #[error("IMAP server error: {0}")]
    Protocol(String),
    #[error("message parsing failed: {0}")]
    Mime(#[from] mime::MimeError),
    #[error("queued mail action is invalid: {0}")]
    InvalidAction(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    pub name: String,
    pub remote_name: String,
    pub kind: String,
    pub unread_count: u32,
}

#[derive(Debug, Clone)]
pub struct SyncSnapshot {
    pub messages: Vec<Message>,
    pub folders: Vec<RemoteFolder>,
}

/// Discovers selectable mailboxes and fetches a bounded window from INBOX.
/// This function is intended to run on a worker thread; it never touches GTK
/// state and can therefore be retried with backoff after suspend or a lost
/// network connection.
pub fn sync_inbox(
    account: &Account,
    password: &str,
    limit: usize,
) -> Result<SyncSnapshot, ImapError> {
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, password, "INBOX", "Inbox", limit, true)
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, password, "INBOX", "Inbox", limit, true)
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, password, "INBOX", "Inbox", limit, true)
        }
    }
}

pub fn sync_folder(
    account: &Account,
    password: &str,
    remote_name: &str,
    local_name: &str,
    limit: usize,
) -> Result<Vec<Message>, ImapError> {
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    let snapshot = match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(
                client,
                account,
                password,
                remote_name,
                local_name,
                limit,
                false,
            )?
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(
                client,
                account,
                password,
                remote_name,
                local_name,
                limit,
                false,
            )?
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(
                client,
                account,
                password,
                remote_name,
                local_name,
                limit,
                false,
            )?
        }
    };
    Ok(snapshot.messages)
}

/// Applies local actions that were recorded while the account was offline.
/// Actions are deliberately removed from the queue only after the server has
/// acknowledged them. A UIDVALIDITY change therefore leaves the action queued
/// instead of risking a change to an unrelated, recycled UID.
pub fn reconcile_actions(
    account: &Account,
    password: &str,
    actions: &[PendingAction],
    folders: &[MailFolder],
) -> Result<Vec<i64>, ImapError> {
    if actions.is_empty() {
        return Ok(Vec::new());
    }
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            reconcile_client(client, account, password, actions, folders)
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            reconcile_client(client, account, password, actions, folders)
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            reconcile_client(client, account, password, actions, folders)
        }
    }
}

fn reconcile_client<T: Read + Write>(
    client: imap::Client<T>,
    account: &Account,
    password: &str,
    actions: &[PendingAction],
    folders: &[MailFolder],
) -> Result<Vec<i64>, ImapError> {
    let mut session = client
        .login(&account.incoming.username, password)
        .map_err(|error| ImapError::Protocol(error.0.to_string()))?;
    let mut applied = Vec::new();
    let mut ordered_actions = actions.to_vec();
    // Flags are applied before moves. This preserves the user's intent even
    // when several actions were recorded for the same message offline and a
    // copy operation would otherwise assign it a new UID in the destination.
    ordered_actions.sort_by_key(|action| (action.action == "move", action.id));
    let mut move_sources = HashMap::new();
    for action in actions.iter().filter(|action| action.action == "move") {
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&action.payload_json) else {
            continue;
        };
        if let (Some(message_id), Some(source_folder)) = (
            action.message_id,
            payload
                .get("source_folder")
                .and_then(serde_json::Value::as_str),
        ) {
            move_sources
                .entry(message_id)
                .or_insert_with(|| source_folder.to_string());
        }
    }

    for action in &ordered_actions {
        let Some(remote_uid) = action.remote_uid else {
            continue;
        };
        let payload = serde_json::from_str::<serde_json::Value>(&action.payload_json)
            .map_err(|error| ImapError::InvalidAction(error.to_string()))?;
        let uid_set = remote_uid.to_string();

        match action.action.as_str() {
            "read" | "star" => {
                let local_folder = payload
                    .get("folder")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|folder| {
                        let moved = action
                            .message_id
                            .map(|message_id| move_sources.contains_key(&message_id))
                            .unwrap_or(false);
                        if moved { None } else { Some(folder) }
                    })
                    .or_else(|| {
                        action
                            .message_id
                            .and_then(|message_id| move_sources.get(&message_id))
                            .map(String::as_str)
                    })
                    .or(action.folder.as_deref())
                    .unwrap_or("Inbox");
                let remote_folder = remote_name_for_local(folders, account.id, local_folder)
                    .ok_or_else(|| {
                        ImapError::InvalidAction(format!("unknown mailbox: {local_folder}"))
                    })?;
                let mailbox = session
                    .select(&remote_folder)
                    .map_err(|error| ImapError::Protocol(error.to_string()))?;
                if mailbox.uid_validity != action.uidvalidity {
                    continue;
                }
                let value = payload
                    .get("value")
                    .and_then(serde_json::Value::as_bool)
                    .ok_or_else(|| {
                        ImapError::InvalidAction(format!("missing value for {}", action.action))
                    })?;
                let flag = match (action.action.as_str(), value) {
                    ("read", true) => "+FLAGS (\\Seen)",
                    ("read", false) => "-FLAGS (\\Seen)",
                    ("star", true) => "+FLAGS (\\Flagged)",
                    ("star", false) => "-FLAGS (\\Flagged)",
                    _ => unreachable!(),
                };
                session
                    .uid_store(&uid_set, flag)
                    .map_err(|error| ImapError::Protocol(error.to_string()))?;
                applied.push(action.id);
            }
            "move" => {
                let Some(source_folder) = payload
                    .get("source_folder")
                    .and_then(serde_json::Value::as_str)
                else {
                    // Older queue entries did not record the source mailbox;
                    // keep them queued rather than guessing dangerously.
                    continue;
                };
                let target_folder = payload
                    .get("folder")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| ImapError::InvalidAction("move has no target".into()))?;
                let source_remote = remote_name_for_local(folders, account.id, source_folder)
                    .ok_or_else(|| {
                        ImapError::InvalidAction(format!("unknown mailbox: {source_folder}"))
                    })?;
                let target_remote = remote_name_for_local(folders, account.id, target_folder)
                    .ok_or_else(|| {
                        ImapError::InvalidAction(format!("unknown mailbox: {target_folder}"))
                    })?;
                let mailbox = session
                    .select(&source_remote)
                    .map_err(|error| ImapError::Protocol(error.to_string()))?;
                if mailbox.uid_validity != action.uidvalidity {
                    continue;
                }
                session
                    .uid_copy(&uid_set, &target_remote)
                    .map_err(|error| ImapError::Protocol(error.to_string()))?;
                session
                    .uid_store(&uid_set, "+FLAGS (\\Deleted)")
                    .map_err(|error| ImapError::Protocol(error.to_string()))?;
                session
                    .uid_expunge(&uid_set)
                    .map_err(|error| ImapError::Protocol(error.to_string()))?;
                applied.push(action.id);
            }
            other => {
                return Err(ImapError::InvalidAction(other.to_string()));
            }
        }
    }
    let _ = session.logout();
    Ok(applied)
}

fn remote_name_for_local(
    folders: &[MailFolder],
    account_id: Option<i64>,
    local_name: &str,
) -> Option<String> {
    if let Some(folder) = folders.iter().find(|folder| {
        Some(folder.account_id) == account_id && folder.name.eq_ignore_ascii_case(local_name)
    }) {
        return Some(folder.remote_name.clone());
    }
    match local_name.to_ascii_lowercase().as_str() {
        "inbox" => Some("INBOX".into()),
        "sent" => Some("Sent".into()),
        "drafts" => Some("Drafts".into()),
        "archive" => Some("Archive".into()),
        "trash" => Some("Trash".into()),
        "spam" => Some("Spam".into()),
        _ => None,
    }
}

fn sync_client<T: Read + Write>(
    client: imap::Client<T>,
    account: &Account,
    password: &str,
    remote_name: &str,
    local_name: &str,
    limit: usize,
    discover_folders: bool,
) -> Result<SyncSnapshot, ImapError> {
    let mut session = client
        .login(&account.incoming.username, password)
        .map_err(|error| ImapError::Protocol(error.0.to_string()))?;
    let folders = if discover_folders {
        session
            .list(None, Some("*"))
            .map_err(|error| ImapError::Protocol(error.to_string()))?
            .into_iter()
            .filter_map(remote_folder)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mailbox = session
        .select(remote_name)
        .map_err(|error| ImapError::Protocol(error.to_string()))?;
    let uidvalidity = mailbox.uid_validity;
    let uids = session
        .uid_search("ALL")
        .map_err(|error| ImapError::Protocol(error.to_string()))?;
    let mut selected = uids.into_iter().collect::<Vec<_>>();
    selected.sort_unstable_by(|left, right| right.cmp(left));
    selected.truncate(limit);
    let sequence = selected
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut messages = Vec::new();
    if !sequence.is_empty() {
        let fetches = session
            .uid_fetch(sequence, "(RFC822 FLAGS INTERNALDATE)")
            .map_err(|error| ImapError::Protocol(error.to_string()))?;
        for fetch in fetches.iter() {
            if let Some(message) = message_from_fetch(fetch, account.id, uidvalidity, local_name)? {
                messages.push(message);
            }
        }
    }
    messages.sort_by(|left, right| right.received_at.cmp(&left.received_at));
    let _ = session.logout();
    Ok(SyncSnapshot { messages, folders })
}

fn remote_folder(folder: &Name) -> Option<RemoteFolder> {
    if folder
        .attributes()
        .iter()
        .any(|attribute| matches!(attribute, NameAttribute::NoSelect))
    {
        return None;
    }
    let remote_name = folder.name().to_string();
    let (name, kind) = classify_folder(&remote_name);
    Some(RemoteFolder {
        name,
        remote_name,
        kind,
        unread_count: 0,
    })
}

pub fn classify_folder(remote_name: &str) -> (String, String) {
    let lower = remote_name.to_ascii_lowercase();
    let leaf = remote_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(remote_name);
    let leaf_lower = leaf.to_ascii_lowercase();
    if lower == "inbox" {
        return ("Inbox".into(), "inbox".into());
    }
    if leaf_lower.contains("sent") || leaf_lower == "outbox" {
        return ("Sent".into(), "sent".into());
    }
    if leaf_lower.contains("draft") {
        return ("Drafts".into(), "drafts".into());
    }
    if leaf_lower.contains("archive") || leaf_lower.contains("all mail") {
        return ("Archive".into(), "archive".into());
    }
    if leaf_lower.contains("trash") || leaf_lower == "bin" || leaf_lower.contains("deleted") {
        return ("Trash".into(), "trash".into());
    }
    if leaf_lower.contains("spam") || leaf_lower.contains("junk") {
        return ("Spam".into(), "spam".into());
    }
    (leaf.to_string(), "custom".into())
}

fn message_from_fetch(
    fetch: &Fetch,
    account_id: Option<i64>,
    uidvalidity: Option<u32>,
    folder: &str,
) -> Result<Option<Message>, ImapError> {
    let Some(raw) = fetch.body() else {
        return Ok(None);
    };
    let parsed = mime::parse(raw)?;
    let Some(remote_uid) = fetch.uid else {
        return Ok(None);
    };
    let id = stable_id(account_id, folder, remote_uid, uidvalidity);
    let thread_key = parsed
        .references
        .last()
        .cloned()
        .or_else(|| parsed.in_reply_to.clone())
        .or_else(|| parsed.message_id.clone());
    let received_at = fetch
        .internal_date()
        .map(|date| date.with_timezone(&Utc).to_rfc3339())
        .unwrap_or_else(|| Utc::now().to_rfc3339());
    let unread = !fetch
        .flags()
        .iter()
        .any(|flag| matches!(flag, imap::types::Flag::Seen));
    let (sender_name, sender_email) = split_sender(&parsed.sender);
    let subject = parsed.subject.clone();
    let preview = preview(&parsed.body);
    let attachments = mime::cache_attachments(id, &parsed.attachments);
    Ok(Some(Message {
        id,
        account_id,
        folder: folder.into(),
        remote_uid: Some(remote_uid),
        uidvalidity,
        message_id: parsed.message_id,
        thread_key,
        sender_name,
        sender_email,
        recipients: parsed.recipients,
        subject,
        preview,
        body: parsed.body,
        received_at,
        unread,
        starred: false,
        has_attachments: !attachments.is_empty(),
        attachments,
        thread_size: 1,
    }))
}

pub fn stable_id(account_id: Option<i64>, folder: &str, uid: u32, uidvalidity: Option<u32>) -> i64 {
    let mut value = account_id.unwrap_or_default() as u64;
    for byte in folder.as_bytes() {
        value = value.wrapping_mul(1_000_003).wrapping_add(*byte as u64);
    }
    value = value.wrapping_mul(1_000_003).wrapping_add(uid as u64);
    value = value
        .wrapping_mul(1_000_003)
        .wrapping_add(uidvalidity.unwrap_or_default() as u64);
    (value & i64::MAX as u64) as i64
}

fn split_sender(sender: &str) -> (String, String) {
    if let Some((name, address)) = sender.split_once('<') {
        return (
            name.trim().trim_matches('"').to_string(),
            address.trim_end_matches('>').trim().to_string(),
        );
    }
    (sender.to_string(), sender.to_string())
}

fn preview(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_ids_are_repeatable() {
        assert_eq!(
            stable_id(Some(4), "Inbox", 12, Some(9)),
            stable_id(Some(4), "Inbox", 12, Some(9))
        );
        assert_ne!(
            stable_id(Some(4), "Inbox", 12, Some(9)),
            stable_id(Some(4), "Inbox", 13, Some(9))
        );
        assert_ne!(
            stable_id(Some(4), "Inbox", 12, Some(9)),
            stable_id(Some(4), "Archive", 12, Some(9))
        );
    }

    #[test]
    fn preview_collapses_whitespace() {
        assert_eq!(preview("hello\n\nthere"), "hello there");
    }

    #[test]
    fn classifies_common_and_custom_mailboxes() {
        assert_eq!(classify_folder("INBOX"), ("Inbox".into(), "inbox".into()));
        assert_eq!(
            classify_folder("[Gmail]/Sent Mail"),
            ("Sent".into(), "sent".into())
        );
        assert_eq!(
            classify_folder("Archive/Receipts"),
            ("Receipts".into(), "custom".into())
        );
        assert_eq!(classify_folder("Junk"), ("Spam".into(), "spam".into()));
    }
}
