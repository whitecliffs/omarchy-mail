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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailFolder {
    pub account_id: i64,
    pub name: String,
    pub remote_name: String,
    pub kind: String,
    pub unread_count: u32,
}

impl ServerConfig {
    pub fn imap_defaults(domain: &str, username: &str) -> Self {
        Self {
            hostname: format!("imap.{domain}"),
            port: 993,
            security: SecurityMode::Tls,
            username: username.to_string(),
        }
    }

    pub fn smtp_defaults(domain: &str, username: &str) -> Self {
        Self {
            hostname: format!("smtp.{domain}"),
            port: 465,
            security: SecurityMode::Tls,
            username: username.to_string(),
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
    pub received_at: String,
    pub unread: bool,
    pub starred: bool,
    pub has_attachments: bool,
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
                received_at: "Today, 14:32".into(),
                unread: true,
                starred: true,
                has_attachments: true,
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
                received_at: "Today, 11:08".into(),
                unread: true,
                starred: false,
                has_attachments: false,
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
                received_at: "Yesterday".into(),
                unread: false,
                starred: false,
                has_attachments: false,
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
                received_at: "Yesterday".into(),
                unread: false,
                starred: false,
                has_attachments: true,
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
                received_at: "Monday".into(),
                unread: false,
                starred: false,
                has_attachments: false,
                thread_size: 1,
            },
        ]
    }
}
