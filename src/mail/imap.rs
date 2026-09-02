use crate::mail::credentials::AuthMaterial;
use crate::mail::mime;
use crate::models::{Account, MailFolder, Message, PendingAction, SecurityMode};
use chrono::Utc;
use imap::types::{Fetch, Name, NameAttribute};
use native_tls::TlsConnector;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
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
    pub uidvalidity: Option<u32>,
    pub all_uids: Vec<u32>,
    pub skipped_messages: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleOutcome {
    Changed,
    TimedOut,
    Unsupported,
}

/// Discovers selectable mailboxes and fetches a bounded window from INBOX.
/// This function is intended to run on a worker thread; it never touches GTK
/// state and can therefore be retried with backoff after suspend or a lost
/// network connection.
pub fn sync_inbox(
    account: &Account,
    auth: &AuthMaterial,
    limit: usize,
) -> Result<SyncSnapshot, ImapError> {
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, auth, "INBOX", "Inbox", limit, true)
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, auth, "INBOX", "Inbox", limit, true)
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, auth, "INBOX", "Inbox", limit, true)
        }
    }
}

/// Verifies the configured IMAP endpoint and credentials without fetching a
/// mailbox. This is used by account settings so a user can recover from a
/// missing keyring entry or correct server details without waiting for the
/// background monitor to report an error.
pub fn test_connection(account: &Account, auth: &AuthMaterial) -> Result<(), ImapError> {
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            test_authenticated_client(client, account, auth)
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            test_authenticated_client(client, account, auth)
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            test_authenticated_client(client, account, auth)
        }
    }
}

/// Creates a mailbox on the configured IMAP server. Folder mutations are
/// intentionally kept separate from sync: the local folder map is updated by
/// the UI only after this operation has been acknowledged by the server.
pub fn create_folder(
    account: &Account,
    auth: &AuthMaterial,
    remote_name: &str,
) -> Result<(), ImapError> {
    mailbox_operation(
        account,
        auth,
        MailboxOperation::Create(remote_name.to_string()),
    )
}

/// Renames a mailbox on the configured IMAP server.
pub fn rename_folder(
    account: &Account,
    auth: &AuthMaterial,
    old_remote_name: &str,
    new_remote_name: &str,
) -> Result<(), ImapError> {
    mailbox_operation(
        account,
        auth,
        MailboxOperation::Rename(old_remote_name.to_string(), new_remote_name.to_string()),
    )
}

/// Deletes a mailbox on the configured IMAP server. The caller must prevent
/// deletion of provider-managed standard folders before reaching this layer.
pub fn delete_folder(
    account: &Account,
    auth: &AuthMaterial,
    remote_name: &str,
) -> Result<(), ImapError> {
    mailbox_operation(
        account,
        auth,
        MailboxOperation::Delete(remote_name.to_string()),
    )
}

#[derive(Clone)]
enum MailboxOperation {
    Create(String),
    Rename(String, String),
    Delete(String),
}

fn mailbox_operation(
    account: &Account,
    auth: &AuthMaterial,
    operation: MailboxOperation,
) -> Result<(), ImapError> {
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            mailbox_operation_client(client, account, auth, operation)
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            mailbox_operation_client(client, account, auth, operation)
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            mailbox_operation_client(client, account, auth, operation)
        }
    }
}

fn mailbox_operation_client<T: Read + Write>(
    client: imap::Client<T>,
    account: &Account,
    auth: &AuthMaterial,
    operation: MailboxOperation,
) -> Result<(), ImapError> {
    let mut session = authenticate(client, account, auth)?;
    let result = match operation {
        MailboxOperation::Create(remote_name) => session
            .create(remote_name)
            .map_err(|error| ImapError::Protocol(error.to_string())),
        MailboxOperation::Rename(old_remote_name, new_remote_name) => session
            .rename(old_remote_name, new_remote_name)
            .map_err(|error| ImapError::Protocol(error.to_string())),
        MailboxOperation::Delete(remote_name) => session
            .delete(remote_name)
            .map_err(|error| ImapError::Protocol(error.to_string())),
    };
    let _ = session.logout();
    result
}

fn test_authenticated_client<T: Read + Write>(
    client: imap::Client<T>,
    account: &Account,
    auth: &AuthMaterial,
) -> Result<(), ImapError> {
    let mut session = authenticate(client, account, auth)?;
    session
        .capabilities()
        .map_err(|error| ImapError::Protocol(error.to_string()))?;
    let _ = session.logout();
    Ok(())
}

pub fn sync_folder(
    account: &Account,
    auth: &AuthMaterial,
    remote_name: &str,
    local_name: &str,
    limit: usize,
) -> Result<SyncSnapshot, ImapError> {
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    let snapshot = match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, auth, remote_name, local_name, limit, false)?
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, auth, remote_name, local_name, limit, false)?
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, auth, remote_name, local_name, limit, false)?
        }
    };
    Ok(snapshot)
}

/// Re-fetches one complete RFC822 message when its disposable attachment
/// cache has been evicted. The selected mailbox UIDVALIDITY is checked before
/// parsing so a recycled UID can never populate the wrong local message.
pub fn fetch_message(
    account: &Account,
    auth: &AuthMaterial,
    remote_name: &str,
    local_name: &str,
    remote_uid: u32,
    expected_uidvalidity: Option<u32>,
) -> Result<Option<Message>, ImapError> {
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            fetch_message_client(
                client,
                account,
                auth,
                remote_name,
                local_name,
                remote_uid,
                expected_uidvalidity,
            )
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            fetch_message_client(
                client,
                account,
                auth,
                remote_name,
                local_name,
                remote_uid,
                expected_uidvalidity,
            )
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            fetch_message_client(
                client,
                account,
                auth,
                remote_name,
                local_name,
                remote_uid,
                expected_uidvalidity,
            )
        }
    }
}

/// Waits for the selected inbox to change using IMAP IDLE. The timeout is
/// intentionally bounded so the caller can periodically refresh the IDLE
/// connection and recover cleanly from laptop suspend or server idle limits.
pub fn wait_for_inbox_change(
    account: &Account,
    auth: &AuthMaterial,
    timeout: Duration,
) -> Result<IdleOutcome, ImapError> {
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            idle_client(client, account, auth, timeout)
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            idle_client(client, account, auth, timeout)
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            idle_client(client, account, auth, timeout)
        }
    }
}

struct XOAuth2Authenticator {
    username: String,
    access_token: String,
}

impl imap::Authenticator for XOAuth2Authenticator {
    type Response = String;

    fn process(&self, _challenge: &[u8]) -> Self::Response {
        format!(
            "user={}\x01auth=Bearer {}\x01\x01",
            self.username, self.access_token
        )
    }
}

fn authenticate<T: Read + Write>(
    client: imap::Client<T>,
    account: &Account,
    auth: &AuthMaterial,
) -> Result<imap::Session<T>, ImapError> {
    match auth {
        AuthMaterial::Password(password) => client
            .login(&account.incoming.username, password)
            .map_err(|error| ImapError::Protocol(error.0.to_string())),
        AuthMaterial::OAuth2AccessToken(access_token) => {
            let authenticator = XOAuth2Authenticator {
                username: account.incoming.username.clone(),
                access_token: access_token.clone(),
            };
            client
                .authenticate("XOAUTH2", &authenticator)
                .map_err(|error| ImapError::Protocol(error.0.to_string()))
        }
    }
}

fn idle_client<T: Read + Write + imap::extensions::idle::SetReadTimeout>(
    client: imap::Client<T>,
    account: &Account,
    auth: &AuthMaterial,
    timeout: Duration,
) -> Result<IdleOutcome, ImapError> {
    let mut session = authenticate(client, account, auth)?;
    let supports_idle = session
        .capabilities()
        .map_err(|error| ImapError::Protocol(error.to_string()))?
        .has_str("IDLE");
    if !supports_idle {
        let _ = session.logout();
        return Ok(IdleOutcome::Unsupported);
    }

    session
        .select("INBOX")
        .map_err(|error| ImapError::Protocol(error.to_string()))?;
    let outcome = {
        let handle = session
            .idle()
            .map_err(|error| ImapError::Protocol(error.to_string()))?;
        handle
            .wait_with_timeout(timeout)
            .map_err(|error| ImapError::Protocol(error.to_string()))?
    };
    let _ = session.logout();
    Ok(match outcome {
        imap::extensions::idle::WaitOutcome::MailboxChanged => IdleOutcome::Changed,
        imap::extensions::idle::WaitOutcome::TimedOut => IdleOutcome::TimedOut,
    })
}

/// Applies local actions that were recorded while the account was offline.
/// Actions are deliberately removed from the queue only after the server has
/// acknowledged them. A UIDVALIDITY change therefore leaves the action queued
/// instead of risking a change to an unrelated, recycled UID.
pub fn reconcile_actions(
    account: &Account,
    auth: &AuthMaterial,
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
            reconcile_client(client, account, auth, actions, folders)
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            reconcile_client(client, account, auth, actions, folders)
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            reconcile_client(client, account, auth, actions, folders)
        }
    }
}

fn reconcile_client<T: Read + Write>(
    client: imap::Client<T>,
    account: &Account,
    auth: &AuthMaterial,
    actions: &[PendingAction],
    folders: &[MailFolder],
) -> Result<Vec<i64>, ImapError> {
    let mut session = authenticate(client, account, auth)?;
    let mut applied = Vec::new();
    let mut ordered_actions = actions.to_vec();
    // Flags are applied before moves. This preserves the user's intent even
    // when several actions were recorded for the same message offline and a
    // copy operation would otherwise assign it a new UID in the destination.
    ordered_actions.sort_by_key(|action| {
        (
            action.action == "move" || action.action == "copy",
            action.id,
        )
    });
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
            "move" | "copy" => {
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
                if action.action == "move" {
                    session
                        .uid_store(&uid_set, "+FLAGS (\\Deleted)")
                        .map_err(|error| ImapError::Protocol(error.to_string()))?;
                    session
                        .uid_expunge(&uid_set)
                        .map_err(|error| ImapError::Protocol(error.to_string()))?;
                }
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
    auth: &AuthMaterial,
    remote_name: &str,
    local_name: &str,
    limit: usize,
    discover_folders: bool,
) -> Result<SyncSnapshot, ImapError> {
    let mut session = authenticate(client, account, auth)?;
    let mut folders = if discover_folders {
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
    let unread_count = session
        .uid_search("UNSEEN")
        .map_err(|error| ImapError::Protocol(error.to_string()))?
        .len() as u32;
    if let Some(folder) = folders
        .iter_mut()
        .find(|folder| folder.remote_name.eq_ignore_ascii_case(remote_name))
    {
        folder.unread_count = unread_count;
    }
    let mut all_uids = uids.into_iter().collect::<Vec<_>>();
    all_uids.sort_unstable_by(|left, right| right.cmp(left));
    let mut selected = all_uids.clone();
    selected.truncate(limit);
    let sequence = selected
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut messages = Vec::new();
    let mut skipped_messages = 0;
    if !sequence.is_empty() {
        let fetches = session
            .uid_fetch(sequence, "(BODY.PEEK[] FLAGS INTERNALDATE)")
            .map_err(|error| ImapError::Protocol(error.to_string()))?;
        for fetch in fetches.iter() {
            match message_from_fetch(fetch, account.id, uidvalidity, local_name) {
                Ok(Some(message)) => messages.push(message),
                Ok(None) => {}
                Err(ImapError::Mime(_)) => skipped_messages += 1,
                Err(error) => return Err(error),
            }
        }
    }
    messages.sort_by(|left, right| right.received_at.cmp(&left.received_at));
    let _ = session.logout();
    Ok(SyncSnapshot {
        messages,
        folders,
        uidvalidity,
        all_uids,
        skipped_messages,
    })
}

fn fetch_message_client<T: Read + Write>(
    client: imap::Client<T>,
    account: &Account,
    auth: &AuthMaterial,
    remote_name: &str,
    local_name: &str,
    remote_uid: u32,
    expected_uidvalidity: Option<u32>,
) -> Result<Option<Message>, ImapError> {
    let mut session = authenticate(client, account, auth)?;
    let mailbox = session
        .select(remote_name)
        .map_err(|error| ImapError::Protocol(error.to_string()))?;
    if expected_uidvalidity.is_some() && mailbox.uid_validity != expected_uidvalidity {
        let _ = session.logout();
        return Err(ImapError::Protocol(
            "the mailbox changed while this attachment was being downloaded".into(),
        ));
    }
    let fetches = session
        .uid_fetch(remote_uid.to_string(), "(BODY.PEEK[] FLAGS INTERNALDATE)")
        .map_err(|error| ImapError::Protocol(error.to_string()))?;
    let message = fetches
        .iter()
        .find(|fetch| fetch.uid == Some(remote_uid))
        .map(|fetch| message_from_fetch(fetch, account.id, mailbox.uid_validity, local_name))
        .transpose()?;
    let _ = session.logout();
    Ok(message.flatten())
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
    let thread_key = derive_thread_key(
        &parsed.references,
        parsed.in_reply_to.as_deref(),
        parsed.message_id.as_deref(),
    );
    let received_at = fetch
        .internal_date()
        .map(|date| date.with_timezone(&Utc).to_rfc3339())
        .unwrap_or_else(|| Utc::now().to_rfc3339());
    let unread = !fetch
        .flags()
        .iter()
        .any(|flag| matches!(flag, imap::types::Flag::Seen));
    let starred = fetch
        .flags()
        .iter()
        .any(|flag| matches!(flag, imap::types::Flag::Flagged));
    let (sender_name, sender_email) = split_sender(&parsed.sender);
    let subject = parsed.subject.clone();
    let body_html = parsed.is_html.then(|| parsed.body.clone());
    let body = if parsed.is_html {
        mime::html_to_text(&parsed.body)
    } else {
        parsed.body.clone()
    };
    let preview = preview(&body);
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
        body,
        body_html,
        received_at,
        unread,
        starred,
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

fn derive_thread_key(
    references: &[String],
    in_reply_to: Option<&str>,
    message_id: Option<&str>,
) -> Option<String> {
    references
        .first()
        .cloned()
        .or_else(|| in_reply_to.map(str::to_string))
        .or_else(|| message_id.map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ServerConfig;
    use imap::Authenticator;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

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
    fn threading_prefers_the_root_reference() {
        let references = vec!["<root@example.com>".into(), "<parent@example.com>".into()];
        assert_eq!(
            derive_thread_key(
                &references,
                Some("<parent@example.com>"),
                Some("<reply@example.com>")
            ),
            Some("<root@example.com>".into())
        );
        assert_eq!(
            derive_thread_key(
                &[],
                Some("<parent@example.com>"),
                Some("<reply@example.com>")
            ),
            Some("<parent@example.com>".into())
        );
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

    #[test]
    fn builds_the_standard_xoauth2_authenticator_payload() {
        let authenticator = XOAuth2Authenticator {
            username: "jim@example.com".into(),
            access_token: "token-value".into(),
        };
        assert_eq!(
            authenticator.process(b"ignored challenge"),
            "user=jim@example.com\x01auth=Bearer token-value\x01\x01"
        );
    }

    #[test]
    fn syncs_a_real_plaintext_imap_session_with_folders_and_mime_messages() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake IMAP server");
        let port = listener.local_addr().expect("server address").port();
        let server = thread::spawn(move || run_fake_imap_server(listener));

        let mut account = Account::new("jim@example.com", "Jim");
        account.id = Some(42);
        account.incoming = ServerConfig {
            hostname: "127.0.0.1".into(),
            port,
            security: SecurityMode::None,
            username: "jim@example.com".into(),
            auth: crate::models::AuthMethod::Password,
        };
        let snapshot = sync_inbox(
            &account,
            &AuthMaterial::Password("test-password".into()),
            250,
        )
        .expect("IMAP sync");

        server
            .join()
            .expect("fake IMAP server thread")
            .expect("IMAP server");
        assert_eq!(snapshot.uidvalidity, Some(42));
        assert_eq!(snapshot.all_uids, vec![2, 1]);
        assert_eq!(snapshot.skipped_messages, 0);
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.messages[0].remote_uid, Some(2));
        assert_eq!(snapshot.messages[0].subject, "A newsletter with images");
        assert_eq!(snapshot.messages[1].remote_uid, Some(1));
        assert_eq!(snapshot.messages[1].subject, "Welcome 😀");
        assert!(snapshot.messages[1].unread);
        assert!(!snapshot.messages[0].unread);
        assert!(snapshot.messages[1].starred);
        assert_eq!(
            snapshot
                .folders
                .iter()
                .find(|folder| folder.name == "Inbox")
                .map(|folder| folder.unread_count),
            Some(1)
        );
        assert!(snapshot.folders.iter().any(|folder| folder.kind == "sent"));
    }

    #[test]
    fn reconciles_a_copy_action_without_deleting_the_source_message() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake IMAP server");
        let port = listener.local_addr().expect("server address").port();
        let server = thread::spawn(move || run_fake_copy_server(listener));
        let account = account_for_test_server(port);
        let action = PendingAction {
            id: 17,
            account_id: 42,
            message_id: Some(9001),
            action: "copy".into(),
            payload_json: r#"{"folder":"Archive","source_folder":"Inbox"}"#.into(),
            folder: Some("Inbox".into()),
            remote_uid: Some(12),
            uidvalidity: Some(42),
        };
        let folders = vec![
            MailFolder {
                account_id: 42,
                name: "Inbox".into(),
                remote_name: "INBOX".into(),
                kind: "inbox".into(),
                unread_count: 0,
            },
            MailFolder {
                account_id: 42,
                name: "Archive".into(),
                remote_name: "Archive".into(),
                kind: "archive".into(),
                unread_count: 0,
            },
        ];

        let applied = reconcile_actions(
            &account,
            &AuthMaterial::Password("test-password".into()),
            &[action],
            &folders,
        )
        .expect("copy action");
        server
            .join()
            .expect("fake IMAP server thread")
            .expect("IMAP server");
        assert_eq!(applied, vec![17]);
    }

    #[test]
    fn sends_create_rename_and_delete_folder_commands_to_imap() {
        for (operation, expected_command) in ["create", "rename", "delete"]
            .into_iter()
            .zip(["CREATE", "RENAME", "DELETE"])
        {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake IMAP server");
            let port = listener.local_addr().expect("server address").port();
            let server = thread::spawn(move || {
                run_fake_mailbox_operation_server(listener, expected_command)
            });
            let account = account_for_test_server(port);
            let result = match operation {
                "create" => create_folder(
                    &account,
                    &AuthMaterial::Password("test-password".into()),
                    "Receipts",
                ),
                "rename" => rename_folder(
                    &account,
                    &AuthMaterial::Password("test-password".into()),
                    "Receipts",
                    "Invoices",
                ),
                "delete" => delete_folder(
                    &account,
                    &AuthMaterial::Password("test-password".into()),
                    "Invoices",
                ),
                _ => unreachable!(),
            };
            result.expect("folder operation");
            server
                .join()
                .expect("fake IMAP server thread")
                .expect("IMAP server");
        }
    }

    fn account_for_test_server(port: u16) -> Account {
        let mut account = Account::new("jim@example.com", "Jim");
        account.id = Some(42);
        account.incoming = ServerConfig {
            hostname: "127.0.0.1".into(),
            port,
            security: SecurityMode::None,
            username: "jim@example.com".into(),
            auth: crate::models::AuthMethod::Password,
        };
        account
    }

    fn run_fake_copy_server(listener: TcpListener) -> std::io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        stream.write_all(b"* OK Omarchy Mail test server ready\r\n")?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut saw_copy = false;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let mut words = line.split_whitespace();
            let Some(tag) = words.next() else {
                continue;
            };
            let command = words.next().unwrap_or_default().to_ascii_uppercase();
            match command.as_str() {
                "LOGIN" => write_tagged(&mut stream, tag, "OK LOGIN completed")?,
                "SELECT" => {
                    stream.write_all(
                        b"* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
                          * 0 EXISTS\r\n\
                          * 0 RECENT\r\n\
                          * OK [UIDVALIDITY 42] UIDs valid\r\n\
                          * OK [UIDNEXT 13] Predicted next UID\r\n",
                    )?;
                    write_tagged(&mut stream, tag, "OK [READ-WRITE] SELECT completed")?;
                }
                "UID" if line.to_ascii_uppercase().contains("COPY") => {
                    saw_copy = true;
                    write_tagged(&mut stream, tag, "OK UID COPY completed")?;
                }
                "LOGOUT" => {
                    if !saw_copy {
                        return Err(std::io::Error::other("UID COPY was not sent"));
                    }
                    stream.write_all(b"* BYE Logging out\r\n")?;
                    write_tagged(&mut stream, tag, "OK LOGOUT completed")?;
                    break;
                }
                _ => write_tagged(&mut stream, tag, "OK command completed")?,
            }
        }
        Ok(())
    }

    fn run_fake_mailbox_operation_server(
        listener: TcpListener,
        expected_command: &str,
    ) -> std::io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        stream.write_all(b"* OK Omarchy Mail test server ready\r\n")?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut saw_expected = false;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let mut words = line.split_whitespace();
            let Some(tag) = words.next() else {
                continue;
            };
            let command = words.next().unwrap_or_default().to_ascii_uppercase();
            if command == expected_command {
                saw_expected = true;
            }
            match command.as_str() {
                "LOGIN" => write_tagged(&mut stream, tag, "OK LOGIN completed")?,
                "LOGOUT" => {
                    if !saw_expected {
                        return Err(std::io::Error::other(
                            "expected folder command was not sent",
                        ));
                    }
                    stream.write_all(b"* BYE Logging out\r\n")?;
                    write_tagged(&mut stream, tag, "OK LOGOUT completed")?;
                    break;
                }
                _ => write_tagged(&mut stream, tag, "OK command completed")?,
            }
        }
        Ok(())
    }

    fn run_fake_imap_server(listener: TcpListener) -> std::io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        stream.write_all(b"* OK Omarchy Mail test server ready\r\n")?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let message_one = include_bytes!("../../tests/fixtures/multipart-utf8.eml");
        let message_two = include_bytes!("../../tests/fixtures/related-remote-image.eml");

        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let mut words = line.split_whitespace();
            let Some(tag) = words.next() else {
                continue;
            };
            let command = words.next().unwrap_or_default().to_ascii_uppercase();
            match command.as_str() {
                "LOGIN" => write_tagged(&mut stream, tag, "OK LOGIN completed")?,
                "LIST" => {
                    stream.write_all(
                        b"* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                          * LIST (\\HasNoChildren) \"/\" \"Sent\"\r\n\
                          * LIST (\\HasNoChildren) \"/\" \"Archive/Receipts\"\r\n",
                    )?;
                    write_tagged(&mut stream, tag, "OK LIST completed")?;
                }
                "SELECT" => {
                    stream.write_all(
                        b"* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
                          * OK [PERMANENTFLAGS (\\* \\Answered \\Flagged \\Deleted \\Seen \\Draft)] Flags\r\n\
                          * 2 EXISTS\r\n\
                          * 0 RECENT\r\n\
                          * OK [UIDVALIDITY 42] UIDs valid\r\n\
                          * OK [UIDNEXT 3] Predicted next UID\r\n",
                    )?;
                    write_tagged(&mut stream, tag, "OK [READ-WRITE] SELECT completed")?;
                }
                "UID" if line.to_ascii_uppercase().contains("SEARCH") => {
                    if line.to_ascii_uppercase().contains("UNSEEN") {
                        stream.write_all(b"* SEARCH 1\r\n")?;
                    } else {
                        stream.write_all(b"* SEARCH 1 2\r\n")?;
                    }
                    write_tagged(&mut stream, tag, "OK UID SEARCH completed")?;
                }
                "UID" if line.to_ascii_uppercase().contains("FETCH") => {
                    assert!(line.to_ascii_uppercase().contains("BODY.PEEK[]"));
                    write_fetch(
                        &mut stream,
                        2,
                        2,
                        "\\Seen",
                        message_two,
                        "01-Sep-2026 12:00:00 +0000",
                    )?;
                    write_fetch(
                        &mut stream,
                        1,
                        1,
                        "\\Flagged",
                        message_one,
                        "31-Aug-2026 12:00:00 +0000",
                    )?;
                    write_tagged(&mut stream, tag, "OK UID FETCH completed")?;
                }
                "LOGOUT" => {
                    stream.write_all(b"* BYE Logging out\r\n")?;
                    write_tagged(&mut stream, tag, "OK LOGOUT completed")?;
                    break;
                }
                _ => write_tagged(&mut stream, tag, "OK command completed")?,
            }
        }
        Ok(())
    }

    fn write_tagged(stream: &mut TcpStream, tag: &str, response: &str) -> std::io::Result<()> {
        writeln!(stream, "{tag} {response}\r")
    }

    fn write_fetch(
        stream: &mut TcpStream,
        sequence: u32,
        uid: u32,
        flags: &str,
        raw: &[u8],
        internal_date: &str,
    ) -> std::io::Result<()> {
        write!(
            stream,
            "* {sequence} FETCH (UID {uid} FLAGS ({flags}) INTERNALDATE \"{internal_date}\" RFC822 {{{}}}\r\n",
            raw.len()
        )?;
        stream.write_all(raw)?;
        stream.write_all(b")\r\n")
    }
}
