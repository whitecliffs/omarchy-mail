use crate::database::Database;
use crate::mail::{credentials, imap, outbox, smtp};
use crate::models::{Account, MailFolder, Message};
use chrono::{Duration as ChronoDuration, Utc};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub struct SyncReport {
    pub account_id: Option<i64>,
    pub email: String,
    pub fetched: usize,
    pub new_messages: usize,
    pub newest_message: Option<NotificationMessage>,
    pub skipped_messages: usize,
    pub initial: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NotificationMessage {
    pub sender: String,
    pub subject: String,
}

#[derive(Debug)]
pub struct FolderSyncReport {
    pub account_id: Option<i64>,
    pub email: String,
    pub folder: String,
    pub fetched: usize,
    pub new_messages: usize,
    pub skipped_messages: usize,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct MessageFetchReport {
    pub message: Option<Message>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct OutboxReport {
    pub email: String,
    pub sent: usize,
    pub remaining: usize,
    pub error: Option<String>,
}

pub fn notify_new_mail(
    account: &str,
    fetched: usize,
    newest_message: Option<&NotificationMessage>,
) {
    if fetched == 0 {
        return;
    }
    let summary = newest_message
        .map(|message| {
            if message.sender.trim().is_empty() {
                account.to_string()
            } else {
                message.sender.clone()
            }
        })
        .unwrap_or_else(|| account.to_string());
    let body = newest_message
        .map(|message| message.subject.trim())
        .filter(|subject| !subject.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{fetched} message(s) available in your inbox"));
    let _ = notify_rust::Notification::new()
        .summary(&summary)
        .body(&body)
        .icon("org.omarchy.Mail")
        .show();
}

fn load_imap_auth(account: &Account) -> Result<credentials::AuthMaterial, String> {
    credentials::load_auth_material(&account.email, "imap", &account.incoming.auth)
        .map_err(|error| credentials::friendly_load_error("IMAP", &account.incoming.auth, &error))
}

fn load_smtp_auth(account: &Account) -> Result<credentials::AuthMaterial, String> {
    credentials::load_auth_material(&account.email, "smtp", &account.outgoing.auth)
        .map_err(|error| credentials::friendly_load_error("SMTP", &account.outgoing.auth, &error))
}

/// Starts one isolated worker per account. A failing account returns a report
/// rather than taking down the unified mailbox or another account's worker.
pub fn spawn_account_sync(
    account: Account,
    database: Database,
    sender: async_channel::Sender<SyncReport>,
) {
    thread::spawn(move || {
        let report = sync_account_once(&account, &database);
        let _ = sender.send_blocking(report);
    });
}

/// Keeps one account connected for the lifetime of the application. The
/// initial sync and every reconnect use the same bounded worker path as manual
/// refresh; between syncs the worker waits for server-side mailbox changes via
/// IMAP IDLE. Servers without IDLE support fall back to a gentle five-minute
/// poll, and failures use capped exponential backoff.
pub fn spawn_account_monitor(
    account: Account,
    database: Database,
    sender: async_channel::Sender<SyncReport>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut reconnect_backoff = Duration::from_secs(2);
        let mut initial_sync = true;
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let mut report = sync_account_once(&account, &database);
            report.initial = initial_sync;
            if initial_sync && report.error.is_none() {
                match load_imap_auth(&account) {
                    Ok(auth) => match sync_standard_folders(&account, &auth, &database) {
                        Ok((fetched, new_messages, skipped_messages)) => {
                            report.fetched += fetched;
                            report.new_messages += new_messages;
                            report.skipped_messages += skipped_messages;
                        }
                        Err(error) => report.error = Some(error),
                    },
                    Err(error) => report.error = Some(error.to_string()),
                }
            }
            let sync_failed = report.error.is_some();
            if !sync_failed {
                initial_sync = false;
            }
            let _ = sender.send_blocking(report);
            if stop.load(Ordering::Relaxed) {
                break;
            }

            if sync_failed {
                sleep_with_stop(&stop, reconnect_backoff);
                reconnect_backoff = (reconnect_backoff * 2).min(Duration::from_secs(300));
                continue;
            }
            reconnect_backoff = Duration::from_secs(2);

            let auth = match load_imap_auth(&account) {
                Ok(auth) => auth,
                Err(error) => {
                    let _ = sender.send_blocking(SyncReport {
                        account_id: account.id,
                        email: account.email.clone(),
                        fetched: 0,
                        new_messages: 0,
                        newest_message: None,
                        skipped_messages: 0,
                        initial: false,
                        error: Some(error.to_string()),
                    });
                    sleep_with_stop(&stop, Duration::from_secs(60));
                    continue;
                }
            };

            match imap::wait_for_inbox_change(&account, &auth, Duration::from_secs(25 * 60)) {
                Ok(imap::IdleOutcome::Changed | imap::IdleOutcome::TimedOut) => {}
                Ok(imap::IdleOutcome::Unsupported) => {
                    sleep_with_stop(&stop, Duration::from_secs(300));
                }
                Err(error) => {
                    let _ = sender.send_blocking(SyncReport {
                        account_id: account.id,
                        email: account.email.clone(),
                        fetched: 0,
                        new_messages: 0,
                        newest_message: None,
                        skipped_messages: 0,
                        initial: false,
                        error: Some(format!("IMAP monitor reconnecting: {error}")),
                    });
                    sleep_with_stop(&stop, reconnect_backoff);
                    reconnect_backoff = (reconnect_backoff * 2).min(Duration::from_secs(300));
                }
            }
        }
    });
}

/// Keeps queued sends moving independently of the IMAP monitor. SMTP may
/// recover before IMAP does, and a small SQLite check every 30 seconds is
/// cheaper and more reliable than forcing an inbox refresh just to retry mail.
pub fn spawn_outbox_monitor(
    account: Account,
    database: Database,
    sender: async_channel::Sender<OutboxReport>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let report = retry_pending_sends(&account, &database);
            if report.sent > 0 || report.error.is_some() {
                let _ = sender.send_blocking(report);
            }
            sleep_with_stop(&stop, Duration::from_secs(30));
        }
    });
}

fn retry_pending_sends(account: &Account, database: &Database) -> OutboxReport {
    let account_id = account.id;
    let mut report = OutboxReport {
        email: account.email.clone(),
        sent: 0,
        remaining: 0,
        error: None,
    };
    let Some(account_id) = account_id else {
        report.error = Some("The account has no local id for queued mail".into());
        return report;
    };
    let sends = match database.due_pending_sends(account_id) {
        Ok(sends) => sends,
        Err(error) => {
            report.error = Some(format!("Could not load queued mail: {error}"));
            return report;
        }
    };
    if sends.is_empty() {
        report.remaining = database
            .pending_sends(Some(account_id))
            .map(|sends| sends.len())
            .unwrap_or_default();
        return report;
    }
    let auth = match load_smtp_auth(account) {
        Ok(auth) => auth,
        Err(error) => {
            report.error = Some(error);
            report.remaining = database
                .pending_sends(Some(account_id))
                .map(|sends| sends.len())
                .unwrap_or(sends.len());
            return report;
        }
    };

    for send in sends {
        match smtp::send_pending_with_auth(account, &auth, &send) {
            Ok(()) => {
                let result = database
                    .mark_pending_send_sent(send.id)
                    .and_then(|_| database.delete_pending_send(send.id));
                match result {
                    Ok(()) => {
                        outbox::remove_staged_files(&send);
                        report.sent += 1;
                    }
                    Err(error) => {
                        report.error = Some(format!(
                            "A queued message was sent but could not be cleared from the outbox: {error}"
                        ))
                    }
                }
            }
            Err(error) => {
                let retryable = error.is_retryable();
                let next_attempt = retryable.then(|| {
                    (Utc::now() + retry_delay(send.attempts.saturating_add(1))).to_rfc3339()
                });
                if let Err(database_error) = database.record_send_failure(
                    send.id,
                    &error.to_string(),
                    retryable,
                    next_attempt.as_deref(),
                ) {
                    report.error = Some(format!(
                        "Could not update queued message state: {database_error}"
                    ));
                } else if !retryable {
                    report.error = Some(format!("Outbox needs attention: {error}"));
                }
            }
        }
    }
    report.remaining = database
        .pending_sends(Some(account_id))
        .map(|sends| sends.len())
        .unwrap_or_default();
    report
}

fn retry_delay(attempts: u32) -> ChronoDuration {
    let seconds = (1_u64 << attempts.min(8)).min(300);
    ChronoDuration::seconds(seconds as i64)
}

fn sleep_with_stop(stop: &AtomicBool, duration: Duration) {
    let interval = Duration::from_secs(1);
    let mut remaining = duration;
    while remaining > Duration::ZERO && !stop.load(Ordering::Relaxed) {
        let current = remaining.min(interval);
        thread::sleep(current);
        remaining = remaining.saturating_sub(current);
    }
}

fn sync_account_once(account: &Account, database: &Database) -> SyncReport {
    match load_imap_auth(account) {
        Ok(auth) => {
            let reconciliation_error = reconcile_pending_actions(account, &auth, database);
            let mut last_error = None;
            let mut report = None;
            for attempt in 0..3 {
                match imap::sync_inbox(account, &auth, 250) {
                    Ok(snapshot) => {
                        let fetched = snapshot.messages.len();
                        let skipped_messages = snapshot.skipped_messages;
                        let uidvalidity = snapshot.uidvalidity;
                        let all_uids = snapshot.all_uids;
                        let messages = snapshot.messages;
                        let folders = account.id.map(|account_id| {
                            snapshot
                                .folders
                                .into_iter()
                                .map(|folder| MailFolder {
                                    account_id,
                                    name: folder.name,
                                    remote_name: folder.remote_name,
                                    kind: folder.kind,
                                    unread_count: folder.unread_count,
                                })
                                .collect::<Vec<_>>()
                        });
                        report = Some(match database.upsert_messages(&messages) {
                            Ok(new_messages) => {
                                let newest_message = if new_messages > 0 {
                                    messages.first().map(|message| NotificationMessage {
                                        sender: if message.sender_name.trim().is_empty() {
                                            message.sender_email.clone()
                                        } else {
                                            message.sender_name.clone()
                                        },
                                        subject: message.subject.clone(),
                                    })
                                } else {
                                    None
                                };
                                let mut cache_error = reconciliation_error.clone();
                                if let Some(account_id) = account.id
                                    && let Err(error) = database.reapply_pending_actions(account_id)
                                {
                                    append_error(&mut cache_error, error);
                                }
                                if let (Some(account_id), Some(uidvalidity)) =
                                    (account.id, uidvalidity)
                                    && let Err(error) = database.reconcile_folder(
                                        account_id,
                                        "Inbox",
                                        uidvalidity,
                                        &all_uids,
                                    )
                                {
                                    append_error(&mut cache_error, error);
                                }
                                if let Some(folders) = folders {
                                    if let Err(error) = database.upsert_folders(&folders) {
                                        append_error(&mut cache_error, error);
                                    } else if let Some(account_id) = account.id {
                                        let remote_names = folders
                                            .iter()
                                            .map(|folder| folder.remote_name.clone())
                                            .collect::<Vec<_>>();
                                        if let Err(error) =
                                            database.reconcile_folders(account_id, &remote_names)
                                        {
                                            append_error(&mut cache_error, error);
                                        }
                                    }
                                }
                                SyncReport {
                                    account_id: account.id,
                                    email: account.email.clone(),
                                    fetched,
                                    new_messages,
                                    newest_message,
                                    skipped_messages,
                                    initial: false,
                                    error: cache_error,
                                }
                            }
                            Err(error) => SyncReport {
                                account_id: account.id,
                                email: account.email.clone(),
                                fetched,
                                new_messages: 0,
                                newest_message: None,
                                skipped_messages,
                                initial: false,
                                error: Some(error.to_string()),
                            },
                        });
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error.to_string());
                        if attempt < 2 {
                            thread::sleep(Duration::from_secs(1 << attempt));
                        }
                    }
                }
            }
            report.unwrap_or(SyncReport {
                account_id: account.id,
                email: account.email.clone(),
                fetched: 0,
                new_messages: 0,
                newest_message: None,
                skipped_messages: 0,
                initial: false,
                error: last_error,
            })
        }
        Err(error) => SyncReport {
            account_id: account.id,
            email: account.email.clone(),
            fetched: 0,
            new_messages: 0,
            newest_message: None,
            skipped_messages: 0,
            initial: false,
            error: Some(error.to_string()),
        },
    }
}

fn append_error(errors: &mut Option<String>, error: impl ToString) {
    let error = error.to_string();
    match errors {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&error);
        }
        None => *errors = Some(error),
    }
}

fn sync_standard_folders(
    account: &Account,
    auth: &credentials::AuthMaterial,
    database: &Database,
) -> Result<(usize, usize, usize), String> {
    let Some(account_id) = account.id else {
        return Ok((0, 0, 0));
    };
    let folders = database
        .load_folders()
        .map_err(|error| format!("Could not load mailboxes: {error}"))?;
    let standard = folders
        .into_iter()
        .filter(|folder| {
            folder.account_id == account_id
                && folder.kind != "custom"
                && !folder.name.eq_ignore_ascii_case("Inbox")
        })
        .collect::<Vec<_>>();

    let mut fetched_total = 0;
    let mut new_total = 0;
    let mut skipped_total = 0;
    let mut errors = Vec::new();
    for folder in standard {
        let mut snapshot = None;
        let mut last_error = None;
        for attempt in 0..3 {
            match imap::sync_folder(account, auth, &folder.remote_name, &folder.name, 100) {
                Ok(result) => {
                    snapshot = Some(result);
                    break;
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                    if attempt < 2 {
                        thread::sleep(Duration::from_secs(1 << attempt));
                    }
                }
            }
        }
        match snapshot {
            Some(snapshot) => {
                fetched_total += snapshot.messages.len();
                skipped_total += snapshot.skipped_messages;
                match database.upsert_messages(&snapshot.messages) {
                    Ok(new_messages) => {
                        new_total += new_messages;
                        if let Err(error) = database.reapply_pending_actions(account_id) {
                            errors.push(format!("{}: {error}", folder.name));
                        }
                        if let Some(uidvalidity) = snapshot.uidvalidity
                            && let Err(error) = database.reconcile_folder(
                                account_id,
                                &folder.name,
                                uidvalidity,
                                &snapshot.all_uids,
                            )
                        {
                            errors.push(format!("{}: {error}", folder.name));
                        }
                    }
                    Err(error) => errors.push(format!("{}: {error}", folder.name)),
                }
            }
            None => errors.push(format!(
                "{}: {}",
                folder.name,
                last_error.unwrap_or_else(|| "unknown folder error".into())
            )),
        }
    }
    if errors.is_empty() {
        Ok((fetched_total, new_total, skipped_total))
    } else {
        Err(format!(
            "Some mailboxes could not sync: {}",
            errors.join(" · ")
        ))
    }
}

fn reconcile_pending_actions(
    account: &Account,
    auth: &credentials::AuthMaterial,
    database: &Database,
) -> Option<String> {
    let Some(account_id) = account.id else {
        return None;
    };
    let actions = match database.pending_actions(account_id) {
        Ok(actions) => actions,
        Err(error) => return Some(format!("Could not load queued mail changes: {error}")),
    };
    if actions.is_empty() {
        return None;
    }
    let folders = match database.load_folders() {
        Ok(folders) => folders,
        Err(error) => return Some(format!("Could not load mailboxes: {error}")),
    };
    match imap::reconcile_actions(account, auth, &actions, &folders) {
        Ok(applied) => {
            for action_id in applied {
                if let Err(error) = database.delete_pending_action(action_id) {
                    return Some(format!("Could not clear queued mail change: {error}"));
                }
            }
            None
        }
        Err(error) => Some(format!("Queued mail changes will retry: {error}")),
    }
}

pub fn spawn_folder_sync(
    account: Account,
    database: Database,
    remote_name: String,
    local_name: String,
    sender: async_channel::Sender<FolderSyncReport>,
) {
    thread::spawn(move || {
        let report = match load_imap_auth(&account) {
            Ok(auth) => {
                let mut snapshot = None;
                let mut last_error = None;
                for attempt in 0..3 {
                    match imap::sync_folder(&account, &auth, &remote_name, &local_name, 250) {
                        Ok(result) => {
                            snapshot = Some(result);
                            break;
                        }
                        Err(error) => {
                            last_error = Some(error.to_string());
                            if attempt < 2 {
                                thread::sleep(Duration::from_secs(1 << attempt));
                            }
                        }
                    }
                }
                match snapshot {
                    Some(snapshot) => {
                        let fetched = snapshot.messages.len();
                        let skipped_messages = snapshot.skipped_messages;
                        match database.upsert_messages(&snapshot.messages) {
                            Ok(new_messages) => {
                                let mut error = snapshot
                                    .uidvalidity
                                    .and_then(|uidvalidity| {
                                        database
                                            .reconcile_folder(
                                                account.id?,
                                                &local_name,
                                                uidvalidity,
                                                &snapshot.all_uids,
                                            )
                                            .err()
                                    })
                                    .map(|error| error.to_string());
                                if let Some(account_id) = account.id
                                    && let Err(reapply_error) =
                                        database.reapply_pending_actions(account_id)
                                {
                                    append_error(&mut error, reapply_error);
                                }
                                FolderSyncReport {
                                    account_id: account.id,
                                    email: account.email,
                                    folder: local_name,
                                    fetched,
                                    new_messages,
                                    skipped_messages,
                                    error,
                                }
                            }
                            Err(error) => FolderSyncReport {
                                account_id: account.id,
                                email: account.email,
                                folder: local_name,
                                fetched,
                                new_messages: 0,
                                skipped_messages,
                                error: Some(error.to_string()),
                            },
                        }
                    }
                    None => FolderSyncReport {
                        account_id: account.id,
                        email: account.email,
                        folder: local_name,
                        fetched: 0,
                        new_messages: 0,
                        skipped_messages: 0,
                        error: last_error,
                    },
                }
            }
            Err(error) => FolderSyncReport {
                account_id: account.id,
                email: account.email,
                folder: local_name,
                fetched: 0,
                new_messages: 0,
                skipped_messages: 0,
                error: Some(error.to_string()),
            },
        };
        let _ = sender.send_blocking(report);
    });
}

/// Re-fetches a cached message on a worker thread. This is intentionally a
/// complete-message fetch: MIME parsing and attachment caching stay in the
/// same trusted path as normal synchronisation, and the UI only receives the
/// resulting safe local model.
pub fn spawn_message_fetch(
    account: Account,
    database: Database,
    message: Message,
    remote_name: String,
    sender: async_channel::Sender<MessageFetchReport>,
) {
    thread::spawn(move || {
        let report = match (
            message.remote_uid,
            message.uidvalidity,
            load_imap_auth(&account),
        ) {
            (Some(remote_uid), uidvalidity, Ok(auth)) => {
                match imap::fetch_message(
                    &account,
                    &auth,
                    &remote_name,
                    &message.folder,
                    remote_uid,
                    uidvalidity,
                ) {
                    Ok(Some(fetched)) => match database.upsert_messages(&[fetched.clone()]) {
                        Ok(_) => MessageFetchReport {
                            message: Some(fetched),
                            error: None,
                        },
                        Err(error) => MessageFetchReport {
                            message: None,
                            error: Some(error.to_string()),
                        },
                    },
                    Ok(None) => MessageFetchReport {
                        message: None,
                        error: Some("The message is no longer available on the server.".into()),
                    },
                    Err(error) => MessageFetchReport {
                        message: None,
                        error: Some(error.to_string()),
                    },
                }
            }
            (None, _, _) => MessageFetchReport {
                message: None,
                error: Some("This message has no remote copy to download from.".into()),
            },
            (_, _, Err(error)) => MessageFetchReport {
                message: None,
                error: Some(error),
            },
        };
        let _ = sender.send_blocking(report);
    });
}
