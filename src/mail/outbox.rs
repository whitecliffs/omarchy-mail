use crate::database::{Database, DatabaseError};
use crate::models::{OutgoingAttachment, PendingSend};
use crate::security;
use chrono::{Duration, Utc};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OutboxError {
    #[error("could not prepare outbox attachments: {0}")]
    Io(#[from] io::Error),
    #[error("could not save the message to the outbox: {0}")]
    Database(#[from] DatabaseError),
}

/// Copies user-selected attachments into the app's persistent XDG data area
/// before the queue row is created. The SMTP worker can therefore retry after
/// the original files move or disappear, without retaining arbitrary source
/// paths in the outbox.
pub fn queue_failed_send(
    database: &Database,
    account_id: i64,
    to: &str,
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body: &str,
    body_html: Option<&str>,
    attachment_paths: &[PathBuf],
    smtp_error: &str,
    retryable: bool,
) -> Result<i64, OutboxError> {
    let (staging_dir, attachments) = stage_attachments(attachment_paths)?;
    let send = PendingSend {
        id: 0,
        account_id,
        to: to.to_string(),
        cc: cc.to_vec(),
        bcc: bcc.to_vec(),
        subject: subject.to_string(),
        body: body.to_string(),
        body_html: body_html.map(crate::mail::mime::sanitize_html),
        attachments,
        created_at: Utc::now().to_rfc3339(),
        attempts: 1,
        retryable,
        next_attempt_at: retryable.then(|| (Utc::now() + Duration::seconds(2)).to_rfc3339()),
        last_error: Some(smtp_error.to_string()),
    };
    match database.queue_send(&send) {
        Ok(id) => Ok(id),
        Err(error) => {
            remove_staged_directory(staging_dir.as_deref());
            Err(error.into())
        }
    }
}

pub fn remove_staged_files(send: &PendingSend) {
    let Some(root) = outbox_root() else {
        return;
    };
    let mut directories = send
        .attachments
        .iter()
        .filter_map(|attachment| {
            let path = PathBuf::from(&attachment.path);
            let parent = path.parent()?;
            let directory_name = parent.file_name()?.to_str()?;
            (parent.parent() == Some(root.as_path())
                && directory_name.starts_with("pending-")
                && path.starts_with(&root))
            .then(|| parent.to_path_buf())
        })
        .collect::<Vec<_>>();
    directories.sort();
    directories.dedup();
    for directory in directories {
        let _ = fs::remove_dir_all(directory);
    }
}

/// Persists the current composer attachments under XDG data using a stable
/// per-draft directory. Re-saving replaces that directory atomically, so an
/// interrupted autosave cannot leave the database pointing at half-copied
/// files.
pub fn stage_draft_attachments(
    draft_id: i64,
    source_paths: &[PathBuf],
) -> Result<Vec<OutgoingAttachment>, OutboxError> {
    let root = draft_root().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the XDG data directory is not available",
        )
    })?;
    security::ensure_private_dir(&root)?;
    let final_directory = root.join(format!("draft-{draft_id}"));
    if source_paths.is_empty() {
        remove_draft_files(draft_id);
        return Ok(Vec::new());
    }
    let temporary_directory = root.join(format!(".draft-{draft_id}.tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temporary_directory);
    security::ensure_private_dir(&temporary_directory)?;
    let result = copy_attachments_to_directory(source_paths, &temporary_directory);
    let attachments = match result {
        Ok(attachments) => attachments,
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary_directory);
            return Err(error.into());
        }
    };
    let retired_directory = root.join(format!(".draft-{draft_id}.old-{}", std::process::id()));
    let _ = fs::remove_dir_all(&retired_directory);
    if final_directory.exists() {
        fs::rename(&final_directory, &retired_directory)?;
    }
    if let Err(error) = fs::rename(&temporary_directory, &final_directory) {
        if retired_directory.exists() {
            let _ = fs::rename(&retired_directory, &final_directory);
        }
        let _ = fs::remove_dir_all(&temporary_directory);
        return Err(error.into());
    }
    let _ = fs::remove_dir_all(&retired_directory);
    Ok(attachments
        .into_iter()
        .map(|attachment| OutgoingAttachment {
            path: final_directory
                .join(Path::new(&attachment.path).file_name().unwrap_or_default())
                .to_string_lossy()
                .into_owned(),
            ..attachment
        })
        .collect())
}

pub fn remove_draft_files(draft_id: i64) {
    let Some(root) = draft_root() else {
        return;
    };
    let directory = root.join(format!("draft-{draft_id}"));
    if directory.parent() == Some(root.as_path()) {
        let _ = fs::remove_dir_all(directory);
    }
}

fn stage_attachments(
    source_paths: &[PathBuf],
) -> Result<(Option<PathBuf>, Vec<OutgoingAttachment>), OutboxError> {
    if source_paths.is_empty() {
        return Ok((None, Vec::new()));
    }
    let root = outbox_root().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the XDG data directory is not available",
        )
    })?;
    stage_attachments_in_root(source_paths, &root)
}

fn stage_attachments_in_root(
    source_paths: &[PathBuf],
    root: &Path,
) -> Result<(Option<PathBuf>, Vec<OutgoingAttachment>), OutboxError> {
    security::ensure_private_dir(root)?;
    let timestamp = Utc::now().timestamp_micros();
    let staging_dir = (0..100)
        .map(|attempt| {
            root.join(format!(
                "pending-{timestamp}-{}-{attempt}",
                std::process::id()
            ))
        })
        .find_map(|directory| {
            if fs::create_dir(&directory).is_err() {
                return None;
            }
            if security::ensure_private_dir(&directory).is_err() {
                let _ = fs::remove_dir(&directory);
                return None;
            }
            Some(directory)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not reserve an outbox attachment directory",
            )
        })?;

    let result = copy_attachments_to_directory(source_paths, &staging_dir);
    match result {
        Ok(attachments) => Ok((Some(staging_dir), attachments)),
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_dir);
            Err(error.into())
        }
    }
}

fn copy_attachments_to_directory(
    source_paths: &[PathBuf],
    directory: &Path,
) -> Result<Vec<OutgoingAttachment>, io::Error> {
    let mut attachments = Vec::with_capacity(source_paths.len());
    for (index, source) in source_paths.iter().enumerate() {
        let metadata = fs::metadata(source)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", source.display()),
            ));
        }
        let filename = source
            .file_name()
            .and_then(|name| name.to_str())
            .map(safe_filename)
            .unwrap_or_else(|| "attachment".into());
        let destination = directory.join(format!("{index:03}-{filename}"));
        fs::copy(source, &destination)?;
        security::set_private_file_permissions(&destination)?;
        attachments.push(OutgoingAttachment {
            filename,
            path: destination.to_string_lossy().into_owned(),
        });
    }
    Ok(attachments)
}

fn remove_staged_directory(directory: Option<&Path>) {
    if let Some(directory) = directory {
        let _ = fs::remove_dir_all(directory);
    }
}

fn outbox_root() -> Option<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(data_home.join("omarchy-mail").join("outbox"))
}

fn draft_root() -> Option<PathBuf> {
    outbox_root().map(|root| root.parent().unwrap_or(root.as_path()).join("drafts"))
}

fn safe_filename(filename: &str) -> String {
    let value = filename.replace(['/', '\\'], "_");
    let value = value.trim_matches('.');
    if value.is_empty() {
        "attachment".into()
    } else {
        value.chars().take(180).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn sanitizes_attachment_names() {
        assert_eq!(safe_filename("../secret.txt"), "_secret.txt");
        assert_eq!(
            safe_filename("../../"),
            "_.._",
            "keeps a safe non-empty name"
        );
        assert_eq!(safe_filename("..."), "attachment");
    }

    #[test]
    fn stages_and_cleans_attachments_inside_the_xdg_data_area() {
        let directory = tempdir().expect("temporary directory");
        let source = directory.path().join("notes.txt");
        fs::write(&source, b"offline attachment").expect("source file");

        let root = directory.path().join("omarchy-mail").join("outbox");
        let (staging, attachments) =
            stage_attachments_in_root(&[source], &root).expect("stage files");
        assert_eq!(attachments.len(), 1);
        assert_eq!(
            fs::read(&attachments[0].path).expect("staged file"),
            b"offline attachment"
        );
        remove_staged_directory(staging.as_deref());
        assert!(!Path::new(&attachments[0].path).exists());
    }

    #[test]
    fn replaces_draft_attachments_in_a_stable_directory() {
        let directory = tempdir().expect("temporary directory");
        let first = directory.path().join("first.txt");
        let second = directory.path().join("second.txt");
        fs::write(&first, b"first").expect("first source");
        fs::write(&second, b"second").expect("second source");
        let root = directory.path().join("omarchy-mail").join("drafts");
        fs::create_dir_all(&root).expect("draft root");

        let first_attachments =
            stage_draft_attachments_in_root(42, &[first], &root).expect("first draft stage");
        assert_eq!(
            fs::read(&first_attachments[0].path).expect("first staged"),
            b"first"
        );
        let second_attachments =
            stage_draft_attachments_in_root(42, &[second], &root).expect("second draft stage");
        assert_eq!(
            fs::read(&second_attachments[0].path).expect("second staged"),
            b"second"
        );
        assert!(!Path::new(&first_attachments[0].path).exists());
        remove_draft_files_in_root(42, &root);
        assert!(!Path::new(&second_attachments[0].path).exists());
    }

    fn stage_draft_attachments_in_root(
        draft_id: i64,
        source_paths: &[PathBuf],
        root: &Path,
    ) -> Result<Vec<OutgoingAttachment>, OutboxError> {
        let final_directory = root.join(format!("draft-{draft_id}"));
        let temporary_directory = root.join(format!(".draft-{draft_id}.test-tmp"));
        let _ = fs::remove_dir_all(&temporary_directory);
        fs::create_dir(&temporary_directory)?;
        let attachments = copy_attachments_to_directory(source_paths, &temporary_directory)?;
        let _ = fs::remove_dir_all(&final_directory);
        fs::rename(&temporary_directory, &final_directory)?;
        Ok(attachments
            .into_iter()
            .map(|attachment| OutgoingAttachment {
                path: final_directory
                    .join(Path::new(&attachment.path).file_name().unwrap_or_default())
                    .to_string_lossy()
                    .into_owned(),
                ..attachment
            })
            .collect())
    }

    fn remove_draft_files_in_root(draft_id: i64, root: &Path) {
        let directory = root.join(format!("draft-{draft_id}"));
        let _ = fs::remove_dir_all(directory);
    }
}
