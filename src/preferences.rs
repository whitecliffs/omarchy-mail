use crate::models::Account;
use crate::security;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Preferences {
    #[serde(default = "default_true")]
    pub block_remote_images: bool,
    #[serde(default = "default_true")]
    pub conversation_view: bool,
    #[serde(default = "default_true")]
    pub notifications_enabled: bool,
    #[serde(default)]
    pub plain_text_warning: bool,
    #[serde(default)]
    pub signatures: HashMap<String, String>,
    #[serde(default)]
    pub allowed_remote_image_senders: Vec<String>,
    #[serde(default)]
    pub collapsed_accounts: Vec<String>,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: i32,
    #[serde(default = "default_message_list_width")]
    pub message_list_width: i32,
    #[serde(default = "default_date_time_format")]
    pub date_time_format: String,
    #[serde(default)]
    pub icloud_calendar: Option<crate::icloud::CalendarAccount>,
    #[serde(default)]
    pub google_calendar: Option<crate::google_calendar::GoogleCalendarAccount>,
    /// Which connected calendar receives events created in Omarchy Mail:
    /// "icloud" or "google". Unset or unavailable falls back to whichever is connected.
    #[serde(default)]
    pub new_event_calendar: Option<String>,
    #[serde(default)]
    pub google_tasks: Option<crate::google_tasks::GoogleTasksAccount>,
    /// Minutes before a timed event to notify; None turns reminders off.
    #[serde(default = "default_reminder_minutes")]
    pub reminder_minutes: Option<u32>,
    #[serde(default)]
    pub remind_subscriptions: bool,
    /// Seconds a sent message waits in the Outbox so it can be undone.
    #[serde(default = "default_undo_send_seconds")]
    pub undo_send_seconds: u32,
    #[serde(default)]
    pub auto_backup: Option<AutoBackup>,
    #[serde(default = "default_true")]
    pub show_personal_calendar: bool,
    #[serde(default)]
    pub calendar_subscriptions: Vec<crate::subscriptions::Subscription>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            block_remote_images: true,
            conversation_view: true,
            notifications_enabled: true,
            plain_text_warning: false,
            signatures: HashMap::new(),
            allowed_remote_image_senders: Vec::new(),
            collapsed_accounts: Vec::new(),
            sidebar_width: default_sidebar_width(),
            message_list_width: default_message_list_width(),
            date_time_format: default_date_time_format(),
            icloud_calendar: None,
            google_calendar: None,
            new_event_calendar: None,
            google_tasks: None,
            reminder_minutes: default_reminder_minutes(),
            remind_subscriptions: false,
            undo_send_seconds: default_undo_send_seconds(),
            auto_backup: None,
            show_personal_calendar: true,
            calendar_subscriptions: Vec::new(),
        }
    }
}

impl Preferences {
    pub fn signature_for(&self, account: &Account) -> String {
        self.signatures
            .get(&account.email)
            .filter(|signature| !signature.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| {
                let name = if account.display_name.trim().is_empty() {
                    &account.email
                } else {
                    &account.display_name
                };
                format!("-- \n{name}")
            })
    }

    pub fn remote_images_allowed_for_sender(&self, sender_email: &str) -> bool {
        let sender_email = sender_email.trim().to_ascii_lowercase();
        !sender_email.is_empty()
            && self
                .allowed_remote_image_senders
                .iter()
                .any(|sender| sender.trim().eq_ignore_ascii_case(&sender_email))
    }

    pub fn allow_remote_images_for_sender(&mut self, sender_email: &str) {
        let sender_email = sender_email.trim();
        if sender_email.is_empty() || self.remote_images_allowed_for_sender(sender_email) {
            return;
        }
        self.allowed_remote_image_senders
            .push(sender_email.to_ascii_lowercase());
        self.allowed_remote_image_senders.sort_unstable();
    }

    /// The calendar that new local events upload to, if any is connected.
    pub fn upload_calendar(&self) -> Option<&'static str> {
        let icloud = self.icloud_calendar.is_some();
        let google = self.google_calendar.is_some();
        match self.new_event_calendar.as_deref() {
            Some("google") if google => Some("google"),
            Some("icloud") if icloud => Some("icloud"),
            _ if icloud => Some("icloud"),
            _ if google => Some("google"),
            _ => None,
        }
    }

    pub fn account_is_collapsed(&self, email: &str) -> bool {
        self.collapsed_accounts
            .iter()
            .any(|account| account.eq_ignore_ascii_case(email.trim()))
    }

    pub fn set_account_collapsed(&mut self, email: &str, collapsed: bool) {
        let email = email.trim();
        if email.is_empty() {
            return;
        }
        if collapsed {
            if !self.account_is_collapsed(email) {
                self.collapsed_accounts.push(email.to_ascii_lowercase());
                self.collapsed_accounts.sort_unstable();
            }
        } else {
            self.collapsed_accounts
                .retain(|account| !account.eq_ignore_ascii_case(email));
        }
    }
}

pub fn load() -> Preferences {
    let Some(path) = path() else {
        return Preferences::default();
    };
    let _ = security::set_private_file_permissions(&path);
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

pub fn save(preferences: &Preferences) -> io::Result<()> {
    let Some(path) = path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the XDG config directory is unavailable",
        ));
    };
    if let Some(parent) = path.parent() {
        security::ensure_private_dir(parent)?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let contents = serde_json::to_vec_pretty(preferences)
        .map_err(|error| io::Error::other(error.to_string()))?;
    fs::write(&temporary, contents)?;
    security::set_private_file_permissions(&temporary)?;
    fs::rename(temporary, path)
}

fn path() -> Option<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config_home.join("omarchy-mail").join("preferences.json"))
}

/// Scheduled encrypted backups. The passphrase lives in the keyring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoBackup {
    pub folder: String,
    pub every_days: u32,
    pub keep: u32,
    #[serde(default)]
    pub last_run: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

fn default_reminder_minutes() -> Option<u32> {
    Some(10)
}

fn default_undo_send_seconds() -> u32 {
    10
}

fn default_true() -> bool {
    true
}

fn default_sidebar_width() -> i32 {
    258
}

fn default_message_list_width() -> i32 {
    440
}

fn default_date_time_format() -> String {
    "day-month-24-hour".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_privacy_and_conversation_view_enabled() {
        let preferences = Preferences::default();
        assert!(preferences.block_remote_images);
        assert!(preferences.conversation_view);
        assert!(preferences.notifications_enabled);
        assert!(!preferences.plain_text_warning);
        assert_eq!(preferences.sidebar_width, 258);
        assert_eq!(preferences.message_list_width, 440);
        assert_eq!(preferences.date_time_format, "day-month-24-hour");
    }

    #[test]
    fn signature_falls_back_to_a_small_account_identity() {
        let account = Account::new("jim@example.com", "Jim");
        assert_eq!(Preferences::default().signature_for(&account), "-- \nJim");
    }

    #[test]
    fn custom_signature_is_selected_by_account_email() {
        let account = Account::new("jim@example.com", "Jim");
        let mut preferences = Preferences::default();
        preferences
            .signatures
            .insert(account.email.clone(), "Regards,\nJim".into());
        assert_eq!(preferences.signature_for(&account), "Regards,\nJim");
    }

    #[test]
    fn remembers_remote_image_permission_per_sender() {
        let mut preferences = Preferences::default();
        assert!(!preferences.remote_images_allowed_for_sender("Jane@Example.com"));
        preferences.allow_remote_images_for_sender(" Jane@Example.com ");
        assert!(preferences.remote_images_allowed_for_sender("jane@example.com"));
        assert_eq!(
            preferences.allowed_remote_image_senders,
            vec!["jane@example.com"]
        );
    }

    #[test]
    fn remembers_collapsed_accounts_by_email() {
        let mut preferences = Preferences::default();
        assert!(!preferences.account_is_collapsed("Jim@Example.com"));
        preferences.set_account_collapsed(" Jim@Example.com ", true);
        assert!(preferences.account_is_collapsed("jim@example.com"));
        preferences.set_account_collapsed("jim@example.com", false);
        assert!(!preferences.account_is_collapsed("jim@example.com"));
    }

    #[test]
    fn new_events_upload_to_exactly_one_connected_calendar() {
        let icloud = crate::icloud::CalendarAccount {
            username: "a@icloud.com".into(),
            calendar_url: "https://caldav.icloud.com/1/".into(),
            calendar_name: "Home".into(),
            namespace: "n".into(),
            upload_after: 0,
        };
        let google = crate::google_calendar::GoogleCalendarAccount {
            email: "a@gmail.com".into(),
            calendar_id: "primary".into(),
            calendar_name: "Me".into(),
            namespace: "n".into(),
            upload_after: 0,
        };
        let mut preferences = Preferences::default();
        assert_eq!(preferences.upload_calendar(), None);
        preferences.new_event_calendar = Some("google".into());
        assert_eq!(preferences.upload_calendar(), None);
        preferences.google_calendar = Some(google);
        assert_eq!(preferences.upload_calendar(), Some("google"));
        preferences.icloud_calendar = Some(icloud);
        assert_eq!(preferences.upload_calendar(), Some("google"));
        preferences.new_event_calendar = None;
        assert_eq!(preferences.upload_calendar(), Some("icloud"));
        preferences.google_calendar = None;
        preferences.new_event_calendar = Some("google".into());
        assert_eq!(preferences.upload_calendar(), Some("icloud"));
    }

    #[test]
    fn older_preference_files_receive_pane_defaults() {
        let preferences: Preferences =
            serde_json::from_str(r#"{"block_remote_images":false,"conversation_view":true}"#)
                .expect("legacy preferences should deserialize");
        assert_eq!(preferences.sidebar_width, 258);
        assert_eq!(preferences.message_list_width, 440);
    }
}
