use crate::database::Database;
use crate::mail::{credentials, imap};
use crate::models::{Account, MailFolder};
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub struct SyncReport {
    pub account_id: Option<i64>,
    pub email: String,
    pub fetched: usize,
    pub new_messages: usize,
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
        let report = match credentials::load_password(&account.email, "imap") {
            Ok(password) => {
                let reconciliation_error =
                    reconcile_pending_actions(&account, &password, &database);
                let mut last_error = None;
                let mut report = None;
                for attempt in 0..3 {
                    match imap::sync_inbox(&account, &password, 250) {
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
                                                error: reconciliation_error.clone(),
                                            },
                                            Err(error) => SyncReport {
                                                account_id: account.id,
                                                email: account.email.clone(),
                                                fetched,
                                                new_messages,
                                                error: Some(
                                                    match reconciliation_error.as_deref() {
                                                        Some(reconciliation_error) => format!(
                                                            "{error}; {reconciliation_error}"
                                                        ),
                                                        None => error.to_string(),
                                                    },
                                                ),
                                            },
                                        },
                                        None => SyncReport {
                                            account_id: account.id,
                                            email: account.email.clone(),
                                            fetched,
                                            new_messages,
                                            error: reconciliation_error.clone(),
                                        },
                                    }
                                }
                                Err(error) => SyncReport {
                                    account_id: account.id,
                                    email: account.email.clone(),
                                    fetched,
                                    new_messages: 0,
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
                    email: account.email,
                    fetched: 0,
                    new_messages: 0,
                    error: last_error,
                })
            }
            Err(error) => SyncReport {
                account_id: account.id,
                email: account.email,
                fetched: 0,
                new_messages: 0,
                error: Some(error.to_string()),
            },
        };
        let _ = sender.send_blocking(report);
    });
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
