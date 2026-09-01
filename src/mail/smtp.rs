use crate::models::{Account, SecurityMode};
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart, header::ContentType};
use lettre::{Message as LettreMessage, SmtpTransport, Transport};
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SmtpError {
    #[error("could not create the message: {0}")]
    Message(#[from] lettre::error::Error),
    #[error("could not establish an SMTP connection: {0}")]
    Transport(#[from] lettre::transport::smtp::Error),
    #[error("could not read attachment: {0}")]
    Io(#[from] std::io::Error),
}

pub fn send_text_with_attachments(
    account: &Account,
    password: &str,
    to: &str,
    cc: &[String],
    subject: &str,
    body: &str,
    attachments: &[PathBuf],
) -> Result<(), SmtpError> {
    let message = build_message(account, to, cc, subject, body, attachments)?;
    let credentials = lettre::transport::smtp::authentication::Credentials::new(
        account.outgoing.username.clone(),
        password.to_string(),
    );
    let mailer = match account.outgoing.security {
        SecurityMode::Tls => SmtpTransport::relay(&account.outgoing.hostname)?
            .port(account.outgoing.port)
            .credentials(credentials)
            .build(),
        SecurityMode::StartTls => SmtpTransport::starttls_relay(&account.outgoing.hostname)?
            .port(account.outgoing.port)
            .credentials(credentials)
            .build(),
        SecurityMode::None => SmtpTransport::builder_dangerous(&account.outgoing.hostname)
            .port(account.outgoing.port)
            .credentials(credentials)
            .build(),
    };
    mailer.send(&message)?;
    Ok(())
}

fn build_message(
    account: &Account,
    to: &str,
    cc: &[String],
    subject: &str,
    body: &str,
    attachments: &[PathBuf],
) -> Result<LettreMessage, SmtpError> {
    let sender = account
        .email
        .parse()
        .map_err(|_| lettre::error::Error::MissingFrom)?;
    let from = Mailbox::new(Some(account.display_name.clone()), sender);
    let mut builder = LettreMessage::builder()
        .from(from)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN);
    for address in to
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder = builder.to(address
            .parse()
            .map_err(|_| lettre::error::Error::MissingTo)?);
    }
    for address in cc
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder = builder.cc(address
            .parse()
            .map_err(|_| lettre::error::Error::MissingTo)?);
    }
    let message = if attachments.is_empty() {
        builder.body(body.to_string())?
    } else {
        let mut multipart = MultiPart::mixed().singlepart(SinglePart::plain(body.to_string()));
        for path in attachments {
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("attachment")
                .replace(['/', '\\'], "_");
            let bytes = fs::read(path)?;
            let attachment = Attachment::new(filename).body(
                bytes,
                ContentType::parse("application/octet-stream")
                    .expect("application/octet-stream is a valid MIME type"),
            );
            multipart = multipart.singlepart(attachment);
        }
        builder.multipart(multipart)?
    };
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Account;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn builds_multipart_message_with_safe_attachment_name() {
        let directory = tempdir().expect("temp directory");
        let path = directory.path().join("notes.txt");
        fs::write(&path, b"attachment body").expect("write attachment");
        let account = Account::new("jim@example.com", "Jim");
        let message = build_message(&account, "jane@example.com", &[], "Notes", "Hello", &[path])
            .expect("build message");
        let raw = message.formatted();
        let formatted = String::from_utf8_lossy(&raw);

        assert!(formatted.contains("Content-Disposition: attachment; filename=\"notes.txt\""));
        assert!(formatted.contains("attachment body"));
    }
}
