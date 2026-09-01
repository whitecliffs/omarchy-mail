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
    body_html: Option<&str>,
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
        body_html,
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
    body_html: Option<&str>,
    attachments: &[(String, PathBuf)],
) -> Result<(), SmtpError> {
    let message =
        build_message_with_names(account, to, cc, bcc, subject, body, body_html, attachments)?;
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
        send.body_html.as_deref(),
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
    body_html: Option<&str>,
    attachments: &[(String, PathBuf)],
) -> Result<LettreMessage, SmtpError> {
    let sender = account
        .email
        .parse()
        .map_err(|_| lettre::error::Error::MissingFrom)?;
    let from = Mailbox::new(Some(account.display_name.clone()), sender);
    let mut builder = LettreMessage::builder().from(from).subject(subject);
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
    let sanitized_html = body_html.map(crate::mail::mime::sanitize_html);
    let message = if attachments.is_empty() {
        if let Some(html) = sanitized_html.as_deref() {
            builder.multipart(MultiPart::alternative_plain_html(
                body.to_string(),
                html.to_string(),
            ))?
        } else {
            builder.body(body.to_string())?
        }
    } else {
        let mut multipart = if let Some(html) = sanitized_html.as_deref() {
            MultiPart::mixed().multipart(MultiPart::alternative_plain_html(
                body.to_string(),
                html.to_string(),
            ))
        } else {
            MultiPart::mixed().singlepart(SinglePart::plain(body.to_string()))
        };
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Sender};
    use std::thread;
    use std::time::Duration;
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
            None,
            &[("notes.txt".into(), path)],
        )
        .expect("build message");
        let raw = message.formatted();
        let formatted = String::from_utf8_lossy(&raw);

        assert!(formatted.contains("Content-Disposition: attachment; filename=\"notes.txt\""));
        assert!(formatted.contains("attachment body"));
    }

    #[test]
    fn builds_plain_and_html_alternatives() {
        let account = Account::new("jim@example.com", "Jim");
        let message = build_message_with_names(
            &account,
            "jane@example.com",
            &[],
            &[],
            "A styled note",
            "A styled note",
            Some("<p><strong>A styled note</strong></p><script>bad()</script>"),
            &[],
        )
        .expect("build message");
        let bytes = message.formatted();
        let formatted = String::from_utf8_lossy(&bytes);

        assert!(formatted.contains("multipart/alternative"));
        assert!(formatted.contains("A styled note"));
        assert!(!formatted.contains("script"));
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
            None,
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

    #[test]
    fn serialized_smtp_message_round_trips_through_the_mime_reader() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("notes.txt");
        fs::write(&path, b"round-trip attachment").expect("write attachment");
        let account = Account::new("jim@example.com", "Jim");
        let message = build_message_with_names(
            &account,
            "Jane <jane@example.com>",
            &["Team <team@example.com>".into()],
            &["Archive <archive@example.com>".into()],
            "A styled note",
            "Plain fallback",
            Some("<p><strong>Rich note</strong></p>"),
            &[("notes.txt".into(), path)],
        )
        .expect("build message");

        let parsed = crate::mail::mime::parse(&message.formatted()).expect("parse serialized mail");
        assert_eq!(parsed.subject, "A styled note");
        assert!(parsed.body.contains("Rich note"));
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].bytes, b"round-trip attachment\r\n");
    }

    #[test]
    fn sends_through_a_real_plaintext_smtp_session_without_serializing_bcc() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake SMTP server");
        let port = listener.local_addr().expect("server address").port();
        let (capture_sender, capture_receiver) = mpsc::channel();
        let server = thread::spawn(move || run_fake_smtp_server(listener, capture_sender));

        let mut account = Account::new("jim@example.com", "Jim");
        account.outgoing = crate::models::ServerConfig {
            hostname: "127.0.0.1".into(),
            port,
            security: crate::models::SecurityMode::None,
            username: "jim@example.com".into(),
            auth: crate::models::AuthMethod::Password,
        };
        send_text_with_auth(
            &account,
            &AuthMaterial::Password("test-password".into()),
            "Jane <jane@example.com>",
            &["Team <team@example.com>".into()],
            &["Archive <archive@example.com>".into()],
            "A socket-level note",
            "Hello from the SMTP integration fixture.",
            Some("<p>Hello from the <strong>SMTP</strong> integration fixture.</p>"),
            &[],
        )
        .expect("SMTP send");

        let capture = capture_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("SMTP capture");
        server
            .join()
            .expect("fake SMTP server thread")
            .expect("SMTP server");
        assert!(
            capture
                .recipients
                .iter()
                .any(|recipient| recipient.contains("archive@example.com"))
        );
        assert!(capture.message.contains("Subject: A socket-level note"));
        assert!(capture.message.contains("Hello from the SMTP"));
        assert!(!capture.message.contains("archive@example.com"));
    }

    #[derive(Debug)]
    struct SmtpCapture {
        recipients: Vec<String>,
        message: String,
    }

    fn run_fake_smtp_server(
        listener: TcpListener,
        capture_sender: Sender<SmtpCapture>,
    ) -> std::io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(b"220 Omarchy Mail test SMTP ready\r\n")?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut recipients = Vec::new();
        let mut message = Vec::new();
        let mut line = String::new();

        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let command = line.trim_end_matches(['\r', '\n']);
            let upper = command.to_ascii_uppercase();
            if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                stream
                    .write_all(b"250-omarchy.test\r\n250-8BITMIME\r\n250 AUTH PLAIN LOGIN\r\n")?;
            } else if upper.starts_with("AUTH ") {
                stream.write_all(b"235 2.7.0 Authentication successful\r\n")?;
            } else if upper.starts_with("MAIL FROM:") {
                stream.write_all(b"250 2.1.0 Sender OK\r\n")?;
            } else if upper.starts_with("RCPT TO:") {
                recipients.push(command.to_string());
                stream.write_all(b"250 2.1.5 Recipient OK\r\n")?;
            } else if upper == "DATA" {
                stream.write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")?;
                loop {
                    line.clear();
                    if reader.read_line(&mut line)? == 0 {
                        break;
                    }
                    if line == ".\r\n" || line == ".\n" {
                        break;
                    }
                    message.extend_from_slice(line.as_bytes());
                }
                stream.write_all(b"250 2.0.0 Message accepted\r\n")?;
            } else if upper == "QUIT" {
                stream.write_all(b"221 2.0.0 Closing connection\r\n")?;
                break;
            } else {
                stream.write_all(b"250 OK\r\n")?;
            }
        }

        let _ = capture_sender.send(SmtpCapture {
            recipients,
            message: String::from_utf8_lossy(&message).into_owned(),
        });
        Ok(())
    }
}
