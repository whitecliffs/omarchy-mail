use crate::models::AttachmentInfo;
use ammonia::Builder;
use mailparse::{MailHeaderMap, ParsedMail, parse_mail};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
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
    pub content_id: Option<String>,
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
    let html = strip_dangerous_blocks(html);
    Builder::default()
        .tags(HashSet::from([
            "a", "b", "br", "code", "div", "em", "i", "li", "ol", "p", "pre", "span", "strong",
            "u", "ul",
        ]))
        .generic_attributes(HashSet::from(["title"]))
        .link_rel(Some("noopener noreferrer"))
        .url_relative(ammonia::UrlRelative::PassThrough)
        .clean(&html)
        .to_string()
}

fn strip_dangerous_blocks(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let dangerous = ["script", "style", "iframe", "object", "embed", "svg"];
    let mut output = String::with_capacity(html.len());
    let mut cursor = 0;
    while cursor < html.len() {
        let Some((start, name)) = dangerous
            .iter()
            .filter_map(|name| {
                lower[cursor..]
                    .find(&format!("<{name}"))
                    .map(|offset| (cursor + offset, *name))
            })
            .min_by_key(|(start, _)| *start)
        else {
            output.push_str(&html[cursor..]);
            break;
        };
        let after_name = start + 1 + name.len();
        let valid_boundary = lower
            .as_bytes()
            .get(after_name)
            .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'>');
        if !valid_boundary {
            output.push_str(&html[cursor..after_name]);
            cursor = after_name;
            continue;
        }
        output.push_str(&html[cursor..start]);
        let Some(open_end_relative) = lower[after_name..].find('>') else {
            break;
        };
        let content_start = after_name + open_end_relative + 1;
        let close_token = format!("</{name}");
        let Some(close_relative) = lower[content_start..].find(&close_token) else {
            break;
        };
        let close_start = content_start + close_relative;
        let Some(close_end_relative) = lower[close_start..].find('>') else {
            break;
        };
        cursor = close_start + close_end_relative + 1;
    }
    output
}

/// Converts already-sanitised HTML into readable text for the GTK reader.
/// Omarchy's base installation does not ship a GTK4 WebKit runtime, so v1
/// deliberately keeps the reader dependency-light and never executes markup.
pub fn html_to_text(html: &str) -> String {
    let safe = sanitize_html(html);
    let mut output = String::with_capacity(safe.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for character in safe.chars() {
        match (in_tag, character) {
            (false, '<') => {
                in_tag = true;
                tag.clear();
            }
            (true, '>') => {
                in_tag = false;
                let normalized = tag.trim().to_ascii_lowercase();
                if normalized == "br"
                    || normalized == "/p"
                    || normalized == "/div"
                    || normalized == "/li"
                {
                    output.push('\n');
                }
            }
            (true, character) => tag.push(character),
            (false, character) => output.push(character),
        }
    }
    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Converts sanitized HTML to the small, safe subset understood by Pango.
/// This provides useful emphasis, lists, line breaks, and links without
/// embedding a browser engine in the GTK4 application.
pub fn html_to_pango(html: &str) -> String {
    let safe = sanitize_html(html);
    let mut output = String::with_capacity(safe.len());
    let mut text = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    let mut link_open = false;

    for character in safe.chars() {
        match (in_tag, character) {
            (false, '<') => {
                append_pango_text(&mut output, &text);
                text.clear();
                in_tag = true;
                tag.clear();
            }
            (true, '>') => {
                in_tag = false;
                append_pango_tag(&mut output, &tag, &mut link_open);
            }
            (true, character) => tag.push(character),
            (false, character) => text.push(character),
        }
    }
    append_pango_text(&mut output, &text);
    output
}

/// Caches attachment bytes outside the database. The returned metadata is
/// still useful when the disposable cache is unavailable, and a later sync
/// can repopulate missing files.
pub fn cache_attachments(message_id: i64, attachments: &[Attachment]) -> Vec<AttachmentInfo> {
    let cache_home = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        });
    let directory = cache_home
        .join("omarchy-mail")
        .join("attachments")
        .join(message_id.to_string());
    let directory_available = fs::create_dir_all(&directory).is_ok();

    attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            let path = directory.join(format!(
                "{index:03}-{}",
                safe_filename(&attachment.filename)
            ));
            let cache_path = if directory_available && fs::write(&path, &attachment.bytes).is_ok() {
                path.to_string_lossy().into_owned()
            } else {
                String::new()
            };
            AttachmentInfo {
                filename: safe_filename(&attachment.filename),
                content_type: attachment.content_type.clone(),
                size: attachment.bytes.len() as u64,
                cache_path,
                content_id: attachment.content_id.clone(),
            }
        })
        .collect()
}

fn append_pango_text(output: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    let decoded = decode_entities(text);
    output.push_str(&glib::markup_escape_text(&decoded));
}

fn append_pango_tag(output: &mut String, raw_tag: &str, link_open: &mut bool) {
    let tag = raw_tag.trim();
    let closing = tag.starts_with('/');
    let name = tag
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.is_empty() {
        return;
    }
    if closing {
        match name.as_str() {
            "b" | "strong" => output.push_str("</b>"),
            "i" | "em" => output.push_str("</i>"),
            "u" => output.push_str("</u>"),
            "code" | "pre" => output.push_str("</tt>"),
            "a" if *link_open => {
                output.push_str("</a>");
                *link_open = false;
            }
            "p" | "div" | "li" => output.push('\n'),
            _ => {}
        }
        return;
    }
    match name.as_str() {
        "b" | "strong" => output.push_str("<b>"),
        "i" | "em" => output.push_str("<i>"),
        "u" => output.push_str("<u>"),
        "code" | "pre" => output.push_str("<tt>"),
        "br" => output.push('\n'),
        "p" | "div" | "li" => {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
        }
        "a" => {
            if let Some(href) = href_from_tag(tag) {
                output.push_str("<a href=\"");
                output.push_str(&glib::markup_escape_text(&href));
                output.push_str("\">");
                *link_open = true;
            }
        }
        _ => {}
    }
}

fn href_from_tag(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let start = lower.find("href=")? + "href=".len();
    let value = tag[start..].trim_start();
    let value = if let Some(quoted) = value.strip_prefix('"') {
        quoted.split_once('"')?.0
    } else if let Some(quoted) = value.strip_prefix('\'') {
        quoted.split_once('\'')?.0
    } else {
        value.split_whitespace().next()?
    };
    let value = decode_entities(value);
    (value.starts_with("https://") || value.starts_with("http://") || value.starts_with("mailto:"))
        .then_some(value)
}

fn decode_entities(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
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
    let content_id = mail
        .headers
        .get_first_value("Content-ID")
        .and_then(|value| normalize_content_id(&value));
    let is_inline_image = content_id.is_some()
        && mail
            .ctype
            .mimetype
            .to_ascii_lowercase()
            .starts_with("image/");
    if filename.is_some() || is_inline_image {
        if !mail.subparts.is_empty() {
            for part in &mail.subparts {
                collect_attachments_inner(part, attachments);
            }
            return;
        }
        if let Ok(bytes) = mail.get_body_raw() {
            attachments.push(Attachment {
                filename: safe_filename(filename.as_deref().unwrap_or("inline-image")),
                content_type: mail.ctype.mimetype.clone(),
                bytes,
                content_id,
            });
        }
        return;
    }
    for part in &mail.subparts {
        collect_attachments_inner(part, attachments);
    }
}

fn normalize_content_id(value: &str) -> Option<String> {
    let value = value.trim().trim_matches('<').trim_matches('>').trim();
    (!value.is_empty() && value.len() <= 512).then(|| value.to_string())
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
        assert!(!safe.contains("alert"));
        assert!(!safe.contains("file:///"));
    }

    #[test]
    fn turns_safe_html_into_readable_text() {
        assert_eq!(
            html_to_text("<p>Hello <strong>world</strong>.</p><p>Next</p>"),
            "Hello world.\nNext"
        );
    }

    #[test]
    fn converts_safe_html_to_native_markup() {
        let markup = html_to_pango(
            r#"<p>Hello <strong>world</strong>. <a href="https://example.com">Read more</a></p>"#,
        );
        assert!(markup.contains("<b>world</b>"));
        assert!(markup.contains("<a href=\"https://example.com\">Read more</a>"));
        assert!(!markup.contains("script"));
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
        assert_eq!(parsed.attachments[0].content_id, None);
    }

    #[test]
    fn preserves_inline_image_content_id_without_allowing_remote_images() {
        let raw = concat!(
            "Content-Type: multipart/related; boundary=related\r\n\r\n",
            "--related\r\n",
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<p>Hello<img src=\"cid:logo@example.com\"></p>\r\n",
            "--related\r\n",
            "Content-Type: image/png\r\n",
            "Content-ID: <logo@example.com>\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "iVBORw0KGgo=\r\n",
            "--related--\r\n",
        );
        let parsed = parse(raw.as_bytes()).expect("mime parse");
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(
            parsed.attachments[0].content_id.as_deref(),
            Some("logo@example.com")
        );
        assert!(!parsed.body.contains("remote"));
        assert!(!sanitize_html(&parsed.body).contains("img"));
    }
}
