use crate::models::{Account, SecurityMode};
use lettre::message::{Mailbox, header::ContentType};
use lettre::{Message as LettreMessage, SmtpTransport, Transport};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SmtpError {
    #[error("could not create the message: {0}")]
    Message(#[from] lettre::error::Error),
    #[error("could not establish an SMTP connection: {0}")]
    Transport(#[from] lettre::transport::smtp::Error),
}

pub fn send_text(
    account: &Account,
    password: &str,
    to: &str,
    cc: &[String],
    subject: &str,
    body: &str,
) -> Result<(), SmtpError> {
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
    let message = builder.body(body.to_string())?;
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
