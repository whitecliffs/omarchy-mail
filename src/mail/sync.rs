use crate::database::Database;
use crate::mail::{credentials, imap};
use crate::models::{Account, MailFolder};
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
    pub initial: bool,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct FolderSyncReport {
    pub account_id: Option<i64>,
    pub email: String,
    pub folder: String,
    pub fetched: usize,
    pub new_messages: usize,
    pub error: Option<String>,
}

pub fn notify_new_mail(account: &str, fetched: usize) {
    if fetched == 0 {
        return;
    }
    let _ = notify_rust::Notification::new()
        .summary(account)
        .body(&format!("{fetched} message(s) available in your inbox"))
        .icon("org.omarchy.Mail")
        .show();
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
            initial_sync = false;
            let sync_failed = report.error.is_some();
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

            let password = match credentials::load_password(&account.email, "imap") {
                Ok(password) => password,
                Err(error) => {
                    let _ = sender.send_blocking(SyncReport {
                        account_id: account.id,
                        email: account.email.clone(),
                        fetched: 0,
                        new_messages: 0,
                        initial: false,
                        error: Some(error.to_string()),
                    });
                    sleep_with_stop(&stop, Duration::from_secs(60));
                    continue;
                }
            };

            match imap::wait_for_inbox_change(&account, &password, Duration::from_secs(25 * 60)) {
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
    match credentials::load_password(&account.email, "imap") {
        Ok(password) => {
            let reconciliation_error = reconcile_pending_actions(account, &password, database);
            let mut last_error = None;
            let mut report = None;
            for attempt in 0..3 {
                match imap::sync_inbox(account, &password, 250) {
                    Ok(snapshot) => {
                        let fetched = snapshot.messages.len();
                        report = Some(match database.upsert_messages(&snapshot.messages) {
                            Ok(new_messages) => {
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
                                match folders {
                                    Some(folders) => match database.upsert_folders(&folders) {
                                        Ok(()) => SyncReport {
                                            account_id: account.id,
                                            email: account.email.clone(),
                                            fetched,
                                            new_messages,
                                            initial: false,
                                            error: reconciliation_error.clone(),
                                        },
                                        Err(error) => SyncReport {
                                            account_id: account.id,
                                            email: account.email.clone(),
                                            fetched,
                                            new_messages,
                                            initial: false,
                                            error: Some(match reconciliation_error.as_deref() {
                                                Some(reconciliation_error) => {
                                                    format!("{error}; {reconciliation_error}")
                                                }
                                                None => error.to_string(),
                                            }),
                                        },
                                    },
                                    None => SyncReport {
                                        account_id: account.id,
                                        email: account.email.clone(),
                                        fetched,
                                        new_messages,
                                        initial: false,
                                        error: reconciliation_error.clone(),
                                    },
                                }
                            }
                            Err(error) => SyncReport {
                                account_id: account.id,
                                email: account.email.clone(),
                                fetched,
                                new_messages: 0,
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
                initial: false,
                error: last_error,
            })
        }
        Err(error) => SyncReport {
            account_id: account.id,
            email: account.email.clone(),
            fetched: 0,
            new_messages: 0,
            initial: false,
            error: Some(error.to_string()),
        },
    }
}

fn reconcile_pending_actions(
    account: &Account,
    password: &str,
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
    match imap::reconcile_actions(account, password, &actions, &folders) {
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
        let report = match credentials::load_password(&account.email, "imap") {
            Ok(password) => {
                let mut messages = None;
                let mut last_error = None;
                for attempt in 0..3 {
                    match imap::sync_folder(&account, &password, &remote_name, &local_name, 250) {
                        Ok(fetched) => {
                            messages = Some(fetched);
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
                match messages {
                    Some(messages) => {
                        let fetched = messages.len();
                        match database.upsert_messages(&messages) {
                            Ok(new_messages) => FolderSyncReport {
                                account_id: account.id,
                                email: account.email,
                                folder: local_name,
                                fetched,
                                new_messages,
                                error: None,
                            },
                            Err(error) => FolderSyncReport {
                                account_id: account.id,
                                email: account.email,
                                folder: local_name,
                                fetched,
                                new_messages: 0,
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
                error: Some(error.to_string()),
            },
        };
        let _ = sender.send_blocking(report);
    });
}
