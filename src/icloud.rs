use crate::{database::Database, mail::credentials, models::CalendarEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CalendarAccount {
    pub username: String,
    pub calendar_url: String,
    pub calendar_name: String,
    pub namespace: String,
    pub upload_after: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteCalendar {
    pub name: String,
    pub url: String,
}
#[derive(Debug, Deserialize)]
pub struct RemoteEvent {
    pub key: String,
    pub href: String,
    pub event: CalendarEvent,
}
#[derive(Debug, Deserialize)]
pub struct Uploaded {
    pub id: i64,
    pub href: String,
}
#[derive(Debug, Deserialize)]
pub struct Snapshot {
    pub events: Vec<RemoteEvent>,
    pub uploaded: Vec<Uploaded>,
}

pub(crate) fn worker(request: Value) -> Result<Value, String> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    let worker_source = include_str!("../scripts/icloud_worker.py");
    let python = data.join("omarchy-mail/icloud-venv/bin/python");
    let mut command = if python.is_file() {
        let mut command = Command::new(python);
        command.args(["-c", worker_source]);
        command
    } else {
        let mut command = Command::new("python3");
        command.args(["-c", worker_source]);
        command
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Calendar helper is unavailable. Install the calendar dependencies or reinstall the app.".to_string())?;
    let payload = serde_json::to_vec(&request).map_err(|_| "Cannot prepare calendar request")?;
    child
        .stdin
        .take()
        .ok_or("Cannot open calendar helper")?
        .write_all(&payload)
        .map_err(|_| "Cannot send calendar request")?;
    let result = child
        .wait_with_output()
        .map_err(|_| "Calendar helper stopped")?;
    if !result.status.success() {
        return Err("Calendar sync timed out or stopped. Local events are retained.".into());
    }
    let value: Value =
        serde_json::from_slice(&result.stdout).map_err(|_| "Invalid calendar response")?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(error.into());
    }
    Ok(value)
}

pub fn discover(username: &str, password: &str) -> Result<Vec<RemoteCalendar>, String> {
    let response = worker(json!({"action":"discover", "username":username, "password":password}))?;
    serde_json::from_value(response["calendars"].clone())
        .map_err(|_| "Invalid calendar list".into())
}

pub fn sync(database: &Database, account: &CalendarAccount) -> Result<String, String> {
    let password = credentials::load_password(&account.username, "icloud-calendar").map_err(
        |_| "Cannot read iCloud password from the keyring. Reconnect in Calendar settings.",
    )?;
    let pending = database
        .pending_calendar_events(account.upload_after)
        .map_err(|e| e.to_string())?;
    let response = worker(
        json!({"action":"sync", "username":account.username, "password":password,
        "calendar_url":account.calendar_url, "namespace":account.namespace, "pending":pending}),
    )?;
    let snapshot: Snapshot =
        serde_json::from_value(response).map_err(|_| "Invalid calendar snapshot")?;
    database
        .apply_calendar_snapshot(&account.calendar_url, &snapshot)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "iCloud synced: {} events, {} uploaded",
        snapshot.events.len(),
        snapshot.uploaded.len()
    ))
}

#[cfg(test)]
mod live_diagnostics {
    use super::*;
    #[test]
    #[ignore = "Explicit read-only check of the configured iCloud account; requires its keyring credential"]
    fn read_configured_calendar_without_uploads() {
        let account = crate::preferences::load()
            .icloud_calendar
            .expect("No saved calendar");
        let password = credentials::load_password(&account.username, "icloud-calendar")
            .expect("Keyring credential unavailable");
        let response = worker(
            json!({"action":"sync", "username":account.username, "password":password,
            "calendar_url":account.calendar_url, "namespace":account.namespace, "pending":[]}),
        );
        match response {
            Ok(value) => eprintln!(
                "Read-only sync succeeded: {} instances",
                value["events"].as_array().unwrap().len()
            ),
            Err(error) => panic!("{error}"),
        }
    }
}
