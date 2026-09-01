use crate::database::Database;
use crate::mail::{credentials, imap};
use crate::models::Account;
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
                let mut last_error = None;
                let mut report = None;
                for attempt in 0..3 {
                    match imap::sync_inbox(&account, &password, 250) {
                        Ok(messages) => {
                            report = Some(match database.upsert_messages(&messages) {
                                Ok(new_messages) => SyncReport {
                                    account_id: account.id,
                                    email: account.email.clone(),
                                    fetched: messages.len(),
                                    new_messages,
                                    error: None,
                                },
                                Err(error) => SyncReport {
                                    account_id: account.id,
                                    email: account.email.clone(),
                                    fetched: 0,
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
