use crate::models::Account;
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
    #[serde(default)]
    pub plain_text_warning: bool,
    #[serde(default)]
    pub signatures: HashMap<String, String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            block_remote_images: true,
            conversation_view: true,
            plain_text_warning: false,
            signatures: HashMap::new(),
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
}

pub fn load() -> Preferences {
    let Some(path) = path() else {
        return Preferences::default();
    };
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
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let contents = serde_json::to_vec_pretty(preferences)
        .map_err(|error| io::Error::other(error.to_string()))?;
    fs::write(&temporary, contents)?;
    fs::rename(temporary, path)
}

fn path() -> Option<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config_home.join("omarchy-mail").join("preferences.json"))
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_privacy_and_conversation_view_enabled() {
        let preferences = Preferences::default();
        assert!(preferences.block_remote_images);
        assert!(preferences.conversation_view);
        assert!(!preferences.plain_text_warning);
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
}
