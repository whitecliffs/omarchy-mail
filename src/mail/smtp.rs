use crate::mail::credentials::AuthMaterial;
use crate::models::{Account, AuthMethod, PendingSend, SecurityMode};
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::Mechanism;
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

impl SmtpError {
    /// Network and transient SMTP failures are safe to retry from the
    /// outbox. Authentication/server response failures remain visible for
    /// manual correction instead of looping forever.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(error) => {
                (error.is_transient()
                    || error.is_timeout()
                    || error.is_transport_shutdown()
                    || (!error.is_response() && !error.is_tls() && !error.is_client()))
                    && !error.is_permanent()
            }
            Self::Message(_) | Self::Io(_) => false,
        }
    }
}

pub fn send_text_with_auth(
    account: &Account,
    auth: &AuthMaterial,
    to: &str,
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body: &str,
    attachments: &[PathBuf],
) -> Result<(), SmtpError> {
    let named_attachments = attachments
        .iter()
        .map(|path| {
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(safe_attachment_filename)
                .unwrap_or_else(|| "attachment".into());
            (filename, path.clone())
        })
        .collect::<Vec<_>>();
    let (auth_method, secret) = match auth {
        AuthMaterial::Password(password) => (AuthMethod::Password, password.as_str()),
        AuthMaterial::OAuth2AccessToken(token) => (AuthMethod::OAuth2, token.as_str()),
    };
    send_named_attachments(
        account,
        secret,
        auth_method,
        to,
        cc,
        bcc,
        subject,
        body,
        &named_attachments,
    )
}

fn send_named_attachments(
    account: &Account,
    secret: &str,
    auth_method: AuthMethod,
    to: &str,
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body: &str,
    attachments: &[(String, PathBuf)],
) -> Result<(), SmtpError> {
    let message = build_message_with_names(account, to, cc, bcc, subject, body, attachments)?;
    let credentials = lettre::transport::smtp::authentication::Credentials::new(
        account.outgoing.username.clone(),
        secret.to_string(),
    );
    let mut builder =
        match account.outgoing.security {
            SecurityMode::Tls => {
                SmtpTransport::relay(&account.outgoing.hostname)?.port(account.outgoing.port)
            }
            SecurityMode::StartTls => SmtpTransport::starttls_relay(&account.outgoing.hostname)?
                .port(account.outgoing.port),
            SecurityMode::None => SmtpTransport::builder_dangerous(&account.outgoing.hostname)
                .port(account.outgoing.port),
        };
    builder = builder.credentials(credentials);
    if auth_method == AuthMethod::OAuth2 {
        builder = builder.authentication(vec![Mechanism::Xoauth2]);
    }
    let mailer = builder.build();
    mailer.send(&message)?;
    Ok(())
}

pub fn send_pending_with_auth(
    account: &Account,
    auth: &AuthMaterial,
    send: &PendingSend,
) -> Result<(), SmtpError> {
    let attachments = send
        .attachments
        .iter()
        .map(|attachment| {
            (
                safe_attachment_filename(&attachment.filename),
                PathBuf::from(&attachment.path),
            )
        })
        .collect::<Vec<_>>();
    let (auth_method, secret) = match auth {
        AuthMaterial::Password(password) => (AuthMethod::Password, password.as_str()),
        AuthMaterial::OAuth2AccessToken(token) => (AuthMethod::OAuth2, token.as_str()),
    };
    send_named_attachments(
        account,
        secret,
        auth_method,
        &send.to,
        &send.cc,
        &send.bcc,
        &send.subject,
        &send.body,
        &attachments,
    )
}

fn build_message_with_names(
    account: &Account,
    to: &str,
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body: &str,
    attachments: &[(String, PathBuf)],
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
    for address in crate::mail::split_recipients(to) {
        builder = builder.to(address
            .parse()
            .map_err(|_| lettre::error::Error::MissingTo)?);
    }
    for address in cc
        .iter()
        .flat_map(|value| crate::mail::split_recipients(value))
    {
        builder = builder.cc(address
            .parse()
            .map_err(|_| lettre::error::Error::MissingTo)?);
    }
    for address in bcc
        .iter()
        .flat_map(|value| crate::mail::split_recipients(value))
    {
        builder = builder.bcc(
            address
                .parse()
                .map_err(|_| lettre::error::Error::MissingTo)?,
        );
    }
    let message = if attachments.is_empty() {
        builder.body(body.to_string())?
    } else {
        let mut multipart = MultiPart::mixed().singlepart(SinglePart::plain(body.to_string()));
        for (filename, path) in attachments {
            let bytes = fs::read(path)?;
            let attachment = Attachment::new(filename.clone()).body(
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

fn safe_attachment_filename(filename: &str) -> String {
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
    use crate::models::Account;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn builds_multipart_message_with_safe_attachment_name() {
        let directory = tempdir().expect("temp directory");
        let path = directory.path().join("notes.txt");
        fs::write(&path, b"attachment body").expect("write attachment");
        let account = Account::new("jim@example.com", "Jim");
        let message = build_message_with_names(
            &account,
            "jane@example.com",
            &[],
            &[],
            "Notes",
            "Hello",
            &[("notes.txt".into(), path)],
        )
        .expect("build message");
        let raw = message.formatted();
        let formatted = String::from_utf8_lossy(&raw);

        assert!(formatted.contains("Content-Disposition: attachment; filename=\"notes.txt\""));
        assert!(formatted.contains("attachment body"));
    }

    #[test]
    fn preserves_the_original_name_for_staged_attachments() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("pending-123").join("000-notes.txt");
        fs::create_dir_all(path.parent().expect("staging directory")).expect("directory");
        fs::write(&path, b"attachment body").expect("write attachment");
        let account = Account::new("jim@example.com", "Jim");
        let message = build_message_with_names(
            &account,
            "jane@example.com",
            &[],
            &["archive@example.com".into()],
            "Notes",
            "Hello",
            &[("notes.txt".into(), path)],
        )
        .expect("build message");
        let raw = message.formatted();
        let formatted = String::from_utf8_lossy(&raw);

        assert!(formatted.contains("filename=\"notes.txt\""));
        assert!(!formatted.contains("filename=\"000-notes.txt\""));
        assert!(
            message
                .envelope()
                .to()
                .iter()
                .any(|address| address.to_string() == "archive@example.com")
        );
    }
}
