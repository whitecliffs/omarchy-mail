use crate::{icloud, models::CalendarEvent, security};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subscription {
    pub name: String,
    pub url: String,
    pub visible: bool,
}

pub fn normalize_url(value: &str) -> Result<String, String> {
    let value = value.trim();
    let value = if value
        .get(..9)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("webcal://"))
    {
        format!("https://{}", &value[9..])
    } else {
        value.to_owned()
    };
    let mut url =
        url::Url::parse(&value).map_err(|_| "Enter a valid webcal:// or https:// calendar URL.")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err("Use an HTTPS calendar URL without a username or password.".into());
    }
    url.set_fragment(None);
    Ok(url.to_string())
}

fn cache_path(subscription: &Subscription) -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    data.join("omarchy-mail/subscriptions").join(format!(
        "{:x}.json",
        Sha256::digest(subscription.url.as_bytes())
    ))
}

pub fn cached(subscription: &Subscription) -> Vec<CalendarEvent> {
    fs::read(cache_path(subscription))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

// Feed events never enter calendar_events or its upload queue. They are a
// read-only overlay, with a last-good snapshot retained on any fetch failure.
pub fn refresh(subscription: &Subscription) -> Result<usize, String> {
    let url = normalize_url(&subscription.url)?;
    let result = icloud::worker(serde_json::json!({"action":"subscription", "url":url}))?;
    let events: Vec<CalendarEvent> = serde_json::from_value(result["events"].clone())
        .map_err(|_| "Invalid subscription response")?;
    let path = cache_path(subscription);
    let save = || -> Result<(), Box<dyn std::error::Error>> {
        security::ensure_private_dir(path.parent().unwrap())?;
        let temporary = path.with_extension(format!("{}.tmp", glib::uuid_string_random()));
        fs::write(&temporary, serde_json::to_vec(&events)?)?;
        security::set_private_file_permissions(&temporary)?;
        fs::rename(temporary, &path)?;
        Ok(())
    };
    save().map_err(|_| "Could not save subscription cache".to_string())?;
    Ok(events.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_preferences_keep_personal_calendar_visible() {
        let prefs: crate::preferences::Preferences = serde_json::from_str("{}").unwrap();
        assert!(prefs.show_personal_calendar);
        assert!(prefs.calendar_subscriptions.is_empty());
    }
    #[test]
    #[ignore = "Downloads saved subscriptions into their local cache; never writes to remote calendars"]
    fn refresh_saved_subscriptions() {
        for subscription in crate::preferences::load().calendar_subscriptions {
            let count = refresh(&subscription).expect("subscription download failed");
            assert_eq!(cached(&subscription).len(), count);
            eprintln!("{}: {count} cached events", subscription.name);
        }
    }
    #[test]
    fn normalizes_feeds_without_losing_access_tokens() {
        assert_eq!(
            normalize_url("webcal://example.com/a%20b.ics?key=abc#fragment").unwrap(),
            "https://example.com/a%20b.ics?key=abc"
        );
        for url in [
            "http://example.com/a.ics",
            "file:///tmp/a.ics",
            "https://user:pass@example.com/a.ics",
            "https://example.com:123/a.ics",
        ] {
            assert!(normalize_url(url).is_err());
        }
    }
}
