//! Google Calendar sync. The OAuth tokens live in the Secret Service under a
//! dedicated `google-calendar` slot; the Python helper only ever sees a
//! short-lived access token on stdin.
use crate::{
    database::Database,
    icloud::{RemoteCalendar, Snapshot, worker},
    mail::{
        credentials,
        oauth::{self, OAuthTokens, Provider},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const KEYRING_PROTOCOL: &str = "google-calendar";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoogleCalendarAccount {
    pub email: String,
    pub calendar_id: String,
    pub calendar_name: String,
    pub namespace: String,
    pub upload_after: i64,
}

impl GoogleCalendarAccount {
    /// Source key for `calendar_sync_links`, distinct from iCloud URLs.
    pub fn source(&self) -> String {
        format!("google:{}", self.calendar_id)
    }
}

pub fn store_tokens(email: &str, tokens: &OAuthTokens) -> Result<(), String> {
    credentials::store_oauth2_tokens(email, KEYRING_PROTOCOL, tokens)
        .map_err(|_| "Could not save the Google sign-in in the system keyring.".to_string())
}

/// Returns a usable access token, refreshing (and re-storing) it when needed.
pub fn access_token(email: &str) -> Result<String, String> {
    let tokens = credentials::load_oauth2_tokens(email, KEYRING_PROTOCOL).map_err(|_| {
        "No Google sign-in is stored for this calendar. Sign in again in Settings → Calendar."
            .to_string()
    })?;
    if !tokens.is_expired() {
        return Ok(tokens.access_token);
    }
    let refresh_token = tokens.refresh_token.as_deref().ok_or(
        "Google sign-in has expired and cannot be renewed. Sign in again in Settings → Calendar.",
    )?;
    let client_id = oauth::configured_client_id(Provider::Google).map_err(|e| e.to_string())?;
    let mut refreshed = oauth::refresh_access_token(Provider::Google, &client_id, refresh_token)
        .map_err(|error| format!("Could not renew Google sign-in: {error}"))?;
    if refreshed.refresh_token.is_none() {
        refreshed.refresh_token = tokens.refresh_token.clone();
    }
    store_tokens(email, &refreshed)?;
    Ok(refreshed.access_token)
}

pub fn discover(access_token: &str) -> Result<Vec<RemoteCalendar>, String> {
    let response = worker(json!({"action": "google_discover", "token": access_token}))?;
    serde_json::from_value(response["calendars"].clone())
        .map_err(|_| "Invalid calendar list".into())
}

/// `upload` is false when another calendar is the destination for new events,
/// so a local event is never pushed to two services.
pub fn sync(
    database: &Database,
    account: &GoogleCalendarAccount,
    upload: bool,
) -> Result<String, String> {
    let token = access_token(&account.email)?;
    let pending = if upload {
        database
            .pending_calendar_events(account.upload_after)
            .map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };
    let changes = database
        .pending_calendar_changes(&account.source())
        .map_err(|e| e.to_string())?;
    let response = worker(json!({
        "action": "google_sync", "token": token, "calendar_url": account.calendar_id,
        "namespace": account.namespace, "pending": pending, "changes": changes,
    }))?;
    let snapshot: Snapshot =
        serde_json::from_value(response).map_err(|_| "Invalid calendar snapshot")?;
    crate::icloud::apply_snapshot(database, &account.source(), &snapshot)?;
    Ok(crate::icloud::summary("Google", &snapshot))
}
