use ammonia::Builder;
use mailparse::{MailHeaderMap, ParsedMail, parse_mail};
use std::collections::HashSet;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MimeError {
    #[error("could not parse message: {0}")]
    Parse(#[from] mailparse::MailParseError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    pub sender: String,
    pub recipients: String,
    pub subject: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub body: String,
    pub is_html: bool,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

pub fn parse(raw: &[u8]) -> Result<ParsedMessage, MimeError> {
    let mail = parse_mail(raw)?;
    let body = find_body(&mail, false).unwrap_or_default();
    let is_html = find_body(&mail, true).is_some();
    let body = if is_html {
        sanitize_html(&find_body(&mail, true).unwrap_or_default())
    } else {
        body
    };
    let attachments = collect_attachments(&mail);
    let references = mail
        .headers
        .get_first_value("References")
        .unwrap_or_default()
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect();

    Ok(ParsedMessage {
        sender: mail.headers.get_first_value("From").unwrap_or_default(),
        recipients: mail.headers.get_first_value("To").unwrap_or_default(),
        subject: mail.headers.get_first_value("Subject").unwrap_or_default(),
        message_id: mail.headers.get_first_value("Message-ID"),
        in_reply_to: mail.headers.get_first_value("In-Reply-To"),
        references,
        body,
        is_html,
        attachments,
    })
}

pub fn sanitize_html(html: &str) -> String {
    Builder::default()
        .tags(HashSet::from([
            "a", "b", "br", "code", "div", "em", "i", "li", "ol", "p", "pre", "span", "strong",
            "u", "ul",
        ]))
        .generic_attributes(HashSet::from(["title"]))
        .link_rel(Some("noopener noreferrer"))
        .url_relative(ammonia::UrlRelative::PassThrough)
        .clean(html)
        .to_string()
}

fn find_body(mail: &ParsedMail<'_>, html: bool) -> Option<String> {
    let content_type = mail.ctype.mimetype.to_ascii_lowercase();
    if mail.subparts.is_empty() {
        if (html && content_type == "text/html") || (!html && content_type == "text/plain") {
            return mail.get_body().ok();
        }
        return None;
    }
    mail.subparts.iter().find_map(|part| find_body(part, html))
}

fn collect_attachments(mail: &ParsedMail<'_>) -> Vec<Attachment> {
    let mut attachments = Vec::new();
    collect_attachments_inner(mail, &mut attachments);
    attachments
}

fn collect_attachments_inner(mail: &ParsedMail<'_>, attachments: &mut Vec<Attachment>) {
    let filename = mail.ctype.params.get("name").cloned().or_else(|| {
        mail.headers
            .get_first_value("Content-Disposition")
            .and_then(|value| parse_filename(&value).map(ToOwned::to_owned))
    });
    if let Some(filename) = filename {
        if !mail.subparts.is_empty() {
            for part in &mail.subparts {
                collect_attachments_inner(part, attachments);
            }
            return;
        }
        if let Ok(bytes) = mail.get_body_raw() {
            attachments.push(Attachment {
                filename: safe_filename(&filename),
                content_type: mail.ctype.mimetype.clone(),
                bytes,
            });
        }
        return;
    }
    for part in &mail.subparts {
        collect_attachments_inner(part, attachments);
    }
}

fn parse_filename(disposition: &str) -> Option<&str> {
    disposition.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key.eq_ignore_ascii_case("filename") || key.eq_ignore_ascii_case("filename*"))
            .then(|| value.trim_matches('"'))
    })
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

    #[test]
    fn sanitizes_scripts_and_local_urls() {
        let safe = sanitize_html(
            r#"<p>Hello</p><script>alert('x')</script><img src="file:///etc/passwd">"#,
        );
        assert!(safe.contains("Hello"));
        assert!(!safe.contains("script"));
        assert!(!safe.contains("file:///"));
    }

    #[test]
    fn parses_utf8_multipart_and_attachment() {
        let raw = concat!(
            "From: Jane <jane@example.com>\r\n",
            "To: Jim <jim@example.com>\r\n",
            "Subject: =?UTF-8?B?V2VsY29tZSDwn5iA?=\r\n",
            "Message-ID: <one@example.com>\r\n",
            "Content-Type: multipart/mixed; boundary=mail-boundary\r\n\r\n",
            "--mail-boundary\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n\r\n",
            "Hello, café.\r\n",
            "--mail-boundary\r\n",
            "Content-Type: application/octet-stream; name=notes.txt\r\n",
            "Content-Disposition: attachment; filename=notes.txt\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "bm90ZXM=\r\n",
            "--mail-boundary--\r\n",
        );
        let parsed = parse(raw.as_bytes()).expect("mime parse");
        assert_eq!(parsed.sender, "Jane <jane@example.com>");
        assert_eq!(parsed.attachments[0].filename, "notes.txt");
        assert_eq!(parsed.attachments[0].bytes, b"notes");
    }
}
