//! Two-way todo sync with a Google Tasks list, using the Google Calendar
//! sign-in (which also requests the Tasks scope).
use crate::database::{Database, RemoteTask, TodoSyncOutcome};
use crate::icloud::{RemoteCalendar, worker};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoogleTasksAccount {
    pub email: String,
    pub list_id: String,
    pub list_name: String,
}

impl GoogleTasksAccount {
    pub fn source(&self) -> String {
        format!("gtasks:{}", self.list_id)
    }
}

pub fn discover(access_token: &str) -> Result<Vec<RemoteCalendar>, String> {
    let response = worker(json!({"action": "tasks_lists", "token": access_token}))?;
    serde_json::from_value(response["calendars"].clone()).map_err(|_| "Invalid task list".into())
}

#[derive(Deserialize)]
struct Created {
    id: i64,
    remote_id: String,
    revision: i64,
}

#[derive(Deserialize)]
struct Pushed {
    id: i64,
    revision: i64,
}

#[derive(Deserialize)]
struct Response {
    created: Vec<Created>,
    pushed: Vec<Pushed>,
    deletions_done: Vec<i64>,
    remote: Vec<RemoteTask>,
}

pub fn sync(database: &Database, account: &GoogleTasksAccount) -> Result<String, String> {
    let token = crate::google_calendar::access_token(&account.email)?;
    let source = account.source();
    let items = database
        .todo_sync_items(&source)
        .map_err(|e| e.to_string())?;
    let creates = items
        .iter()
        .filter(|item| item.remote_id.is_none())
        .map(|item| json!({"todo": item.todo, "revision": item.revision}))
        .collect::<Vec<_>>();
    let updates = items
        .iter()
        .filter(|item| item.remote_id.is_some() && item.synced_revision != Some(item.revision))
        .map(|item| json!({"remote_id": item.remote_id, "todo": item.todo, "revision": item.revision}))
        .collect::<Vec<_>>();
    let deletes = database
        .todo_deletions(&source)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(id, remote_id)| json!({"id": id, "remote_id": remote_id}))
        .collect::<Vec<_>>();
    let value = worker(json!({
        "action": "tasks_sync", "token": token, "list_id": account.list_id,
        "creates": creates, "updates": updates, "deletes": deletes,
    }))
    .map_err(|error| {
        if error.contains("refused") {
            "Google refused Tasks access. Enable the Google Tasks API and sign in to Google again in Settings → Calendar.".to_string()
        } else {
            error
        }
    })?;
    let response: Response = serde_json::from_value(value).map_err(|_| "Invalid task response")?;
    let outcome = TodoSyncOutcome {
        created: response
            .created
            .into_iter()
            .map(|c| (c.id, c.remote_id, c.revision))
            .collect(),
        pushed: response
            .pushed
            .into_iter()
            .map(|p| (p.id, p.revision))
            .collect(),
        deletions_done: response.deletions_done,
        remote: response.remote,
    };
    database
        .apply_todo_sync(&source, &outcome)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "Google Tasks synced: {} tasks",
        outcome.remote.len()
    ))
}
