use crate::mail::mime;
use crate::models::{Account, Message, SecurityMode};
use chrono::Utc;
use imap::types::Fetch;
use native_tls::TlsConnector;
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
}

/// Fetches a bounded window from INBOX. This function is intended to run on a
/// worker thread; it never touches GTK state and can therefore be retried with
/// backoff after suspend or a lost network connection.
pub fn sync_inbox(
    account: &Account,
    password: &str,
    limit: usize,
) -> Result<Vec<Message>, ImapError> {
    let tls = TlsConnector::builder().build()?;
    let address = (account.incoming.hostname.as_str(), account.incoming.port);
    match account.incoming.security {
        SecurityMode::Tls => {
            let client = imap::connect(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, password, limit)
        }
        SecurityMode::StartTls => {
            let client = imap::connect_starttls(address, &account.incoming.hostname, &tls)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, password, limit)
        }
        SecurityMode::None => {
            let stream = TcpStream::connect(address)
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|error| ImapError::Protocol(error.to_string()))?;
            sync_client(client, account, password, limit)
        }
    }
}

fn sync_client<T: Read + Write>(
    client: imap::Client<T>,
    account: &Account,
    password: &str,
    limit: usize,
) -> Result<Vec<Message>, ImapError> {
    let mut session = client
        .login(&account.incoming.username, password)
        .map_err(|error| ImapError::Protocol(error.0.to_string()))?;
    let mailbox = session
        .select("INBOX")
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
            if let Some(message) = message_from_fetch(fetch, account.id, uidvalidity)? {
                messages.push(message);
            }
        }
    }
    messages.sort_by(|left, right| right.received_at.cmp(&left.received_at));
    let _ = session.logout();
    Ok(messages)
}

fn message_from_fetch(
    fetch: &Fetch,
    account_id: Option<i64>,
    uidvalidity: Option<u32>,
) -> Result<Option<Message>, ImapError> {
    let Some(raw) = fetch.body() else {
        return Ok(None);
    };
    let parsed = mime::parse(raw)?;
    let Some(remote_uid) = fetch.uid else {
        return Ok(None);
    };
    let id = stable_id(account_id, remote_uid, uidvalidity);
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
    Ok(Some(Message {
        id,
        account_id,
        folder: "Inbox".into(),
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
        has_attachments: !parsed.attachments.is_empty(),
        thread_size: 1,
    }))
}

pub fn stable_id(account_id: Option<i64>, uid: u32, uidvalidity: Option<u32>) -> i64 {
    let mut value = account_id.unwrap_or_default() as u64;
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
            stable_id(Some(4), 12, Some(9)),
            stable_id(Some(4), 12, Some(9))
        );
        assert_ne!(
            stable_id(Some(4), 12, Some(9)),
            stable_id(Some(4), 13, Some(9))
        );
    }

    #[test]
    fn preview_collapses_whitespace() {
        assert_eq!(preview("hello\n\nthere"), "hello there");
    }
}
