use crate::database::{Database, DatabaseError};
use crate::models::{OutgoingAttachment, PendingSend};
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
    fs::create_dir_all(&root)?;
    let timestamp = Utc::now().timestamp_micros();
    let staging_dir = (0..100)
        .map(|attempt| {
            root.join(format!(
                "pending-{timestamp}-{}-{attempt}",
                std::process::id()
            ))
        })
        .find(|directory| fs::create_dir(directory).is_ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not reserve an outbox attachment directory",
            )
        })?;

    let result = (|| {
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
            let destination = staging_dir.join(format!("{index:03}-{filename}"));
            fs::copy(source, &destination)?;
            attachments.push(OutgoingAttachment {
                filename,
                path: destination.to_string_lossy().into_owned(),
            });
        }
        Ok(attachments)
    })();
    match result {
        Ok(attachments) => Ok((Some(staging_dir), attachments)),
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_dir);
            Err(error.into())
        }
    }
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
}
