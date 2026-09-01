use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub id: Option<i64>,
    pub email: String,
    pub display_name: String,
    pub incoming: ServerConfig,
    pub outgoing: ServerConfig,
    pub enabled: bool,
    pub notify: bool,
}

impl Account {
    pub fn new(email: impl Into<String>, display_name: impl Into<String>) -> Self {
        let email = email.into();
        let domain = email.split('@').nth(1).unwrap_or("example.com");
        Self {
            id: None,
            display_name: display_name.into(),
            email: email.clone(),
            incoming: ServerConfig::imap_defaults(domain, &email),
            outgoing: ServerConfig::smtp_defaults(domain, &email),
            enabled: true,
            notify: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerConfig {
    pub hostname: String,
    pub port: u16,
    pub security: SecurityMode,
    pub username: String,
    #[serde(default)]
    pub auth: AuthMethod,
}

/// Authentication is modelled separately from transport security so adding a
/// provider OAuth flow does not require changing account storage again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum AuthMethod {
    #[default]
    Password,
    OAuth2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailFolder {
    pub account_id: i64,
    pub name: String,
    pub remote_name: String,
    pub kind: String,
    pub unread_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAction {
    pub id: i64,
    pub account_id: i64,
    pub message_id: Option<i64>,
    pub action: String,
    pub payload_json: String,
    pub folder: Option<String>,
    pub remote_uid: Option<u32>,
    pub uidvalidity: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingSend {
    pub id: i64,
    pub account_id: i64,
    pub to: String,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub body_html: Option<String>,
    pub attachments: Vec<OutgoingAttachment>,
    pub created_at: String,
    pub attempts: u32,
    pub retryable: bool,
    pub next_attempt_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentInfo {
    pub filename: String,
    pub content_type: String,
    pub size: u64,
    pub cache_path: String,
    #[serde(default)]
    pub content_id: Option<String>,
}

impl ServerConfig {
    pub fn imap_defaults(domain: &str, username: &str) -> Self {
        Self {
            hostname: format!("imap.{domain}"),
            port: 993,
            security: SecurityMode::Tls,
            username: username.to_string(),
            auth: AuthMethod::Password,
        }
    }

    pub fn smtp_defaults(domain: &str, username: &str) -> Self {
        Self {
            hostname: format!("smtp.{domain}"),
            port: 465,
            security: SecurityMode::Tls,
            username: username.to_string(),
            auth: AuthMethod::Password,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SecurityMode {
    Tls,
    StartTls,
    None,
}

impl SecurityMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Tls => "TLS",
            Self::StartTls => "STARTTLS",
            Self::None => "None",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub id: i64,
    pub account_id: Option<i64>,
    pub folder: String,
    pub remote_uid: Option<u32>,
    pub uidvalidity: Option<u32>,
    pub message_id: Option<String>,
    pub thread_key: Option<String>,
    pub sender_name: String,
    pub sender_email: String,
    pub recipients: String,
    pub subject: String,
    pub preview: String,
    pub body: String,
    #[serde(default)]
    pub body_html: Option<String>,
    pub received_at: String,
    pub unread: bool,
    pub starred: bool,
    pub has_attachments: bool,
    pub attachments: Vec<AttachmentInfo>,
    pub thread_size: u32,
}

impl Message {
    pub fn demo_messages() -> Vec<Self> {
        vec![
            Self {
                id: 1,
                account_id: None,
                folder: "Inbox".into(),
                remote_uid: None,
                uidvalidity: None,
                message_id: Some("<demo-1@example.com>".into()),
                thread_key: Some("demo-thread-1".into()),
                sender_name: "Jane Smith".into(),
                sender_email: "jane@example.com".into(),
                recipients: "jim@example.com".into(),
                subject: "A quiet afternoon in the garden".into(),
                preview: "The light is beautiful today. I thought you might enjoy the photos...".into(),
                body: "Hi Jim,\n\nThe light is beautiful today. I thought you might enjoy the photos from the garden. Let’s catch up next week when things are a little quieter.\n\nWarmly,\nJane".into(),
                body_html: None,
                received_at: "Today, 14:32".into(),
                unread: true,
                starred: true,
                has_attachments: true,
                attachments: Vec::new(),
                thread_size: 2,
            },
            Self {
                id: 2,
                account_id: None,
                folder: "Inbox".into(),
                remote_uid: None,
                uidvalidity: None,
                message_id: Some("<demo-2@example.com>".into()),
                thread_key: Some("demo-thread-2".into()),
                sender_name: "Omarchy Updates".into(),
                sender_email: "hello@omarchy.org".into(),
                recipients: "jim@example.com".into(),
                subject: "Welcome to the Osaka Jade release".into(),
                preview: "A small set of thoughtful improvements for your desktop...".into(),
                body: "Hello,\n\nA small set of thoughtful improvements for your desktop has arrived. Thank you for helping shape Omarchy.\n\nThe Omarchy team".into(),
                body_html: Some(
                    "<table width=\"620\" cellpadding=\"0\" cellspacing=\"0\"><tr><td style=\"padding:24px\"><p><strong>A small set of thoughtful improvements for your desktop has arrived.</strong></p><p>Thank you for helping shape Omarchy.</p><table width=\"540\"><tr><td style=\"padding:16px\"><img src=\"https://example.com/omarchy-mail-preview.png\" alt=\"Release preview\"></td></tr></table><p><a href=\"https://omarchy.org\">Read the release notes</a></p></td></tr></table>".into(),
                ),
                received_at: "Today, 11:08".into(),
                unread: true,
                starred: false,
                has_attachments: false,
                attachments: Vec::new(),
                thread_size: 1,
            },
            Self {
                id: 3,
                account_id: None,
                folder: "Inbox".into(),
                remote_uid: None,
                uidvalidity: None,
                message_id: Some("<demo-3@example.com>".into()),
                thread_key: Some("demo-thread-3".into()),
                sender_name: "Alex Morgan".into(),
                sender_email: "alex@example.net".into(),
                recipients: "jim@example.com".into(),
                subject: "Re: Saturday plans".into(),
                preview: "That sounds perfect. I’ll bring the coffee and the map...".into(),
                body: "That sounds perfect. I’ll bring the coffee and the map. See you Saturday!".into(),
                body_html: None,
                received_at: "Yesterday".into(),
                unread: false,
                starred: false,
                has_attachments: false,
                attachments: Vec::new(),
                thread_size: 3,
            },
            Self {
                id: 4,
                account_id: None,
                folder: "Inbox".into(),
                remote_uid: None,
                uidvalidity: None,
                message_id: Some("<demo-4@example.com>".into()),
                thread_key: Some("demo-thread-4".into()),
                sender_name: "Mina Patel".into(),
                sender_email: "mina@example.org".into(),
                recipients: "jim@example.com".into(),
                subject: "The notes you asked for".into(),
                preview: "I’ve attached the latest version, with the changes we discussed...".into(),
                body: "Hi Jim,\n\nI’ve attached the latest version, with the changes we discussed. Let me know if anything else would be useful.\n\nMina".into(),
                body_html: None,
                received_at: "Yesterday".into(),
                unread: false,
                starred: false,
                has_attachments: true,
                attachments: Vec::new(),
                thread_size: 1,
            },
            Self {
                id: 5,
                account_id: None,
                folder: "Inbox".into(),
                remote_uid: None,
                uidvalidity: None,
                message_id: Some("<demo-5@example.com>".into()),
                thread_key: Some("demo-thread-5".into()),
                sender_name: "Daniel Wu".into(),
                sender_email: "daniel@example.com".into(),
                recipients: "jim@example.com".into(),
                subject: "A small question".into(),
                preview: "When you have a moment, could you take a look at this?".into(),
                body: "When you have a moment, could you take a look at this? No rush at all.".into(),
                body_html: None,
                received_at: "Monday".into(),
                unread: false,
                starred: false,
                has_attachments: false,
                attachments: Vec::new(),
                thread_size: 1,
            },
        ]
    }
}
