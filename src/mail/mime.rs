use crate::models::AttachmentInfo;
use ammonia::Builder;
use base64::Engine;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlFragment {
    Markup(String),
    Image { src: String, alt: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlNode {
    Document(Vec<HtmlNode>),
    Text(String),
    Element {
        name: String,
        attributes: Vec<(String, String)>,
        children: Vec<HtmlNode>,
    },
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
    let mut builder = Builder::default();
    builder
        .tags(HashSet::from([
            "a",
            "abbr",
            "address",
            "article",
            "aside",
            "b",
            "bdi",
            "bdo",
            "button",
            "blockquote",
            "br",
            "caption",
            "center",
            "cite",
            "col",
            "colgroup",
            "code",
            "data",
            "dd",
            "del",
            "div",
            "dl",
            "em",
            "figure",
            "figcaption",
            "font",
            "footer",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "hr",
            "i",
            "img",
            "ins",
            "li",
            "main",
            "mark",
            "meta",
            "nav",
            "ol",
            "p",
            "picture",
            "pre",
            "q",
            "span",
            "samp",
            "strong",
            "strike",
            "table",
            "tbody",
            "td",
            "tfoot",
            "th",
            "thead",
            "tr",
            "tt",
            "u",
            "ul",
            "s",
            "section",
            "small",
            "source",
            "style",
            "sub",
            "sup",
            "time",
            "wbr",
        ]))
        // Ammonia strips the contents of <style> by default. Keep style rules
        // for WebKit layout, while script contents are removed below.
        .clean_content_tags(HashSet::from(["script"]))
        .generic_attributes(HashSet::from([
            "align",
            "bgcolor",
            "border",
            "background",
            "cellpadding",
            "cellspacing",
            "class",
            "color",
            "colspan",
            "dir",
            "face",
            "height",
            "id",
            "lang",
            "media",
            "role",
            "rowspan",
            "size",
            "style",
            "title",
            "valign",
            "width",
        ]))
        .add_tag_attributes("a", ["href", "target"])
        .add_tag_attributes("img", ["src", "srcset", "alt", "width", "height"])
        .add_tag_attributes("source", ["src", "srcset", "type", "media"])
        .link_rel(Some("noopener noreferrer"))
        .url_relative(ammonia::UrlRelative::PassThrough)
        .url_schemes(HashSet::from(["cid", "data", "http", "https", "mailto"]))
        .clean(&html)
        .to_string()
}

/// Parses already-sanitised HTML into a small structural tree used to rewrite
/// resources before WebKit receives the document.
pub fn html_document(html: &str) -> HtmlNode {
    let safe = sanitize_html(html);
    let mut cursor = 0;
    HtmlNode::Document(parse_html_nodes(&safe, &mut cursor, None))
}

/// Prepares sanitized HTML for the native WebKitGTK reader. The browser
/// engine is responsible for layout; this layer only rewrites resources so
/// inline CID images work offline and blocked remote images remain in place.
pub fn prepare_html_for_webview(
    html: &str,
    attachments: &[AttachmentInfo],
    remote_images_allowed: bool,
    default_foreground: &str,
) -> String {
    let document = html_document(html);
    let mut body = String::with_capacity(html.len() + 256);
    serialize_webview_node(
        &document,
        &mut body,
        attachments,
        remote_images_allowed,
        false,
    );
    let default_foreground = valid_css_color(default_foreground).unwrap_or("#c1c497");
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><style>:root{{color-scheme:light dark;}}html,body{{margin:0;padding:0;background:transparent;}}body{{color:{default_foreground};font-family:system-ui,sans-serif;font-size:15px;line-height:1.45;overflow-wrap:anywhere;word-wrap:break-word;}}img{{max-width:100%;height:auto;}}table{{max-width:100%;}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;}}</style></head><body>{body}</body></html>"
    )
}

fn serialize_webview_node(
    node: &HtmlNode,
    output: &mut String,
    attachments: &[AttachmentInfo],
    remote_images_allowed: bool,
    inside_style: bool,
) {
    match node {
        HtmlNode::Document(children) => {
            for child in children {
                serialize_webview_node(
                    child,
                    output,
                    attachments,
                    remote_images_allowed,
                    inside_style,
                );
            }
        }
        HtmlNode::Text(text) => {
            if inside_style {
                output.push_str(&sanitize_css_resources(text, remote_images_allowed));
            } else {
                output.push_str(text);
            }
        }
        HtmlNode::Element {
            name,
            attributes,
            children,
        } => {
            output.push('<');
            output.push_str(name);
            for (attribute, value) in attributes {
                if !remote_images_allowed
                    && matches!(name.as_str(), "img" | "source")
                    && matches!(attribute.as_str(), "srcset" | "sizes")
                {
                    continue;
                }
                let value = if name == "img" && attribute == "src" {
                    let alt = attributes
                        .iter()
                        .find(|(known, _)| known == "alt")
                        .map(|(_, value)| value.as_str())
                        .unwrap_or_default();
                    webview_image_source(value, alt, attachments, remote_images_allowed)
                        .unwrap_or_else(|| value.clone())
                } else if name == "source" && attribute == "src" {
                    webview_background_source(value, attachments, remote_images_allowed)
                        .unwrap_or_default()
                } else if attribute == "style" {
                    sanitize_css_resources(value, remote_images_allowed)
                } else if attribute == "background" {
                    webview_background_source(value, attachments, remote_images_allowed)
                        .unwrap_or_default()
                } else {
                    value.clone()
                };
                output.push(' ');
                output.push_str(attribute);
                output.push_str("=\"");
                output.push_str(&escape_html_attribute(&value));
                output.push('"');
            }
            if matches!(
                name.as_str(),
                "area" | "br" | "hr" | "img" | "meta" | "source"
            ) {
                output.push('>');
                return;
            }
            output.push('>');
            let style = inside_style || name == "style";
            for child in children {
                serialize_webview_node(child, output, attachments, remote_images_allowed, style);
            }
            output.push_str("</");
            output.push_str(name);
            output.push('>');
        }
    }
}

fn webview_image_source(
    source: &str,
    alt: &str,
    attachments: &[AttachmentInfo],
    remote_images_allowed: bool,
) -> Option<String> {
    if let Some(content_id) = source.strip_prefix("cid:") {
        let content_id = content_id.trim_matches(['<', '>']);
        if let Some(attachment) = attachments.iter().find(|attachment| {
            attachment.content_id.as_deref().is_some_and(|known| {
                known
                    .trim_matches(['<', '>'])
                    .eq_ignore_ascii_case(content_id)
            })
        }) {
            if !attachment.cache_path.is_empty()
                && let Ok(bytes) = fs::read(&attachment.cache_path)
            {
                return Some(format!(
                    "data:{};base64,{}",
                    attachment.content_type,
                    base64::engine::general_purpose::STANDARD.encode(bytes)
                ));
            }
        }
        return Some(blocked_image_data_uri("Inline image unavailable", alt));
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        if remote_images_allowed {
            return Some(source.to_string());
        }
        return Some(blocked_image_data_uri("Remote image blocked", alt));
    }
    if source.starts_with("data:image/") && source.len() <= 8 * 1024 * 1024 {
        return Some(source.to_string());
    }
    Some(blocked_image_data_uri("Image unavailable", alt))
}

fn webview_background_source(
    source: &str,
    attachments: &[AttachmentInfo],
    remote_images_allowed: bool,
) -> Option<String> {
    if let Some(content_id) = source.strip_prefix("cid:") {
        let content_id = content_id.trim_matches(['<', '>']);
        let attachment = attachments.iter().find(|attachment| {
            attachment.content_id.as_deref().is_some_and(|known| {
                known
                    .trim_matches(['<', '>'])
                    .eq_ignore_ascii_case(content_id)
            })
        })?;
        let bytes = fs::read(&attachment.cache_path).ok()?;
        return Some(format!(
            "data:{};base64,{}",
            attachment.content_type,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ));
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        return remote_images_allowed.then(|| source.to_string());
    }
    if source.starts_with("data:image/") && source.len() <= 8 * 1024 * 1024 {
        return Some(source.to_string());
    }
    None
}

fn blocked_image_data_uri(label: &str, alt: &str) -> String {
    let alt = alt.trim();
    let text = if alt.is_empty() {
        label.to_string()
    } else {
        format!("{label}: {alt}")
    };
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"320\" height=\"64\" viewBox=\"0 0 320 64\"><rect x=\"1\" y=\"1\" width=\"318\" height=\"62\" rx=\"8\" fill=\"#e8e8e8\" stroke=\"#9a9a9a\"/><text x=\"160\" y=\"36\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"13\" fill=\"#555\">{}</text></svg>",
        escape_xml_text(&text)
    );
    format!(
        "data:image/svg+xml;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(svg)
    )
}

fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn valid_css_color(value: &str) -> Option<&str> {
    let value = value.trim();
    (value.starts_with('#')
        && matches!(value.len(), 4 | 7 | 9)
        && value[1..]
            .chars()
            .all(|character| character.is_ascii_hexdigit()))
    .then_some(value)
}

fn sanitize_css_resources(css: &str, remote_images_allowed: bool) -> String {
    let css = strip_css_imports(css);
    let mut output = String::with_capacity(css.len());
    let mut cursor = 0;
    let lower = css.to_ascii_lowercase();
    while cursor < css.len() {
        let Some(relative) = lower[cursor..].find("url(") else {
            output.push_str(&css[cursor..]);
            break;
        };
        let start = cursor + relative;
        output.push_str(&css[cursor..start]);
        let Some(end_relative) = css[start + 4..].find(')') else {
            output.push_str("none");
            break;
        };
        let resource = css[start + 4..start + 4 + end_relative]
            .trim()
            .trim_matches(['\'', '"'])
            .trim();
        if ((resource.starts_with("http://") || resource.starts_with("https://"))
            && remote_images_allowed)
            || (resource.starts_with("data:image/") && resource.len() <= 8 * 1024 * 1024)
        {
            output.push_str("url(\"");
            output.push_str(&escape_css_url(resource));
            output.push_str("\")");
        } else {
            output.push_str("none");
        }
        cursor = start + 4 + end_relative + 1;
    }
    output
}

fn strip_css_imports(css: &str) -> String {
    let lower = css.to_ascii_lowercase();
    let mut output = String::with_capacity(css.len());
    let mut cursor = 0;
    while cursor < css.len() {
        let Some(relative) = lower[cursor..].find("@import") else {
            output.push_str(&css[cursor..]);
            break;
        };
        let start = cursor + relative;
        output.push_str(&css[cursor..start]);
        let Some(end_relative) = css[start..].find(';') else {
            break;
        };
        cursor = start + end_relative + 1;
    }
    output
}

fn escape_css_url(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Serializes a sanitized node back to HTML after resource rewriting.
pub fn html_node_markup(node: &HtmlNode) -> String {
    match node {
        HtmlNode::Document(children) => children.iter().map(html_node_markup).collect(),
        HtmlNode::Text(text) => text.clone(),
        HtmlNode::Element {
            name,
            attributes,
            children,
        } => {
            let mut markup = format!("<{name}");
            for (attribute, value) in attributes {
                markup.push(' ');
                markup.push_str(attribute);
                markup.push_str("=\"");
                markup.push_str(&escape_html_attribute(value));
                markup.push_str("\"");
            }
            if matches!(
                name.as_str(),
                "area" | "br" | "hr" | "img" | "meta" | "source"
            ) {
                markup.push_str(">");
            } else {
                markup.push('>');
                for child in children {
                    markup.push_str(&html_node_markup(child));
                }
                markup.push_str("</");
                markup.push_str(name);
                markup.push('>');
            }
            markup
        }
    }
}

fn parse_html_nodes(input: &str, cursor: &mut usize, closing: Option<&str>) -> Vec<HtmlNode> {
    let mut nodes = Vec::new();
    while *cursor < input.len() {
        if input.as_bytes().get(*cursor) == Some(&b'<') {
            let Some(relative_end) = input[*cursor..].find('>') else {
                nodes.push(HtmlNode::Text(input[*cursor..].to_string()));
                *cursor = input.len();
                break;
            };
            let end = *cursor + relative_end;
            let raw_tag = &input[*cursor + 1..end];
            let tag = raw_tag.trim();
            if tag.starts_with('/') {
                let name = tag
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                *cursor = end + 1;
                if closing.is_some_and(|known| known == name) {
                    break;
                }
                continue;
            }
            let (name, attributes, self_closing) = parse_html_start_tag(tag);
            *cursor = end + 1;
            if name.is_empty() {
                continue;
            }
            let children = if self_closing
                || matches!(
                    name.as_str(),
                    "area" | "br" | "hr" | "img" | "meta" | "source"
                ) {
                Vec::new()
            } else {
                parse_html_nodes(input, cursor, Some(&name))
            };
            nodes.push(HtmlNode::Element {
                name,
                attributes,
                children,
            });
            continue;
        }

        let end = input[*cursor..]
            .find('<')
            .map(|offset| *cursor + offset)
            .unwrap_or(input.len());
        if end > *cursor {
            nodes.push(HtmlNode::Text(input[*cursor..end].to_string()));
        }
        *cursor = end;
    }
    nodes
}

fn parse_html_start_tag(tag: &str) -> (String, Vec<(String, String)>, bool) {
    let self_closing = tag.trim_end().ends_with('/');
    let tag = tag.trim_end_matches('/').trim();
    let mut parts = tag.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default().to_ascii_lowercase();
    let mut attributes = Vec::new();
    let mut rest = parts.next().unwrap_or_default().trim();
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let end_name = rest
            .find(|character: char| character.is_whitespace() || character == '=')
            .unwrap_or(rest.len());
        let attribute = rest[..end_name].to_ascii_lowercase();
        rest = &rest[end_name..];
        rest = rest.trim_start();
        let mut value = String::new();
        if let Some(after_equals) = rest.strip_prefix('=') {
            rest = after_equals.trim_start();
            if let Some(after_quote) = rest.strip_prefix('"') {
                if let Some((quoted, remaining)) = after_quote.split_once('"') {
                    value = decode_entities(quoted);
                    rest = remaining;
                } else {
                    value = decode_entities(after_quote);
                    rest = "";
                }
            } else if let Some(after_quote) = rest.strip_prefix('\'') {
                if let Some((quoted, remaining)) = after_quote.split_once('\'') {
                    value = decode_entities(quoted);
                    rest = remaining;
                } else {
                    value = decode_entities(after_quote);
                    rest = "";
                }
            } else {
                let end_value = rest.find(char::is_whitespace).unwrap_or(rest.len());
                value = decode_entities(&rest[..end_value]);
                rest = &rest[end_value..];
            }
        }
        if !attribute.is_empty() {
            attributes.push((attribute, value));
        }
    }
    (name, attributes, self_closing)
}

fn escape_html_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Splits sanitized HTML into Pango-safe markup and image placeholders while
/// retaining basic inline formatting across image boundaries. GTK's text
/// buffer can replace each placeholder with a paintable, which keeps inline
/// images in the same position as the email authored them.
pub fn html_fragments(html: &str) -> Vec<HtmlFragment> {
    let safe = sanitize_html(html);
    let mut fragments = Vec::new();
    let mut text = String::new();
    let mut active = Vec::<(String, String, String)>::new();
    let mut cursor = 0;

    while cursor < safe.len() {
        if safe.as_bytes().get(cursor) == Some(&b'<') {
            let Some(end_offset) = safe[cursor..].find('>') else {
                text.push_str(&safe[cursor..]);
                break;
            };
            let end = cursor + end_offset;
            flush_fragment_text(&mut fragments, &mut text, &active);
            let raw_tag = &safe[cursor + 1..end];
            let tag = raw_tag.trim();
            let closing = tag.starts_with('/');
            let name = tag
                .trim_start_matches('/')
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();

            if !closing && name == "img" {
                let src = attribute_value(&safe[cursor..=end], "src")
                    .map(|value| decode_entities(&value))
                    .unwrap_or_default();
                let alt = attribute_value(&safe[cursor..=end], "alt")
                    .map(|value| decode_entities(&value))
                    .unwrap_or_default();
                if image_source_allowed(&src) {
                    fragments.push(HtmlFragment::Image { src, alt });
                } else if !alt.is_empty() {
                    text.push_str(&alt);
                }
            } else {
                append_html_tag(&mut fragments, &mut active, tag, &name, closing);
            }
            cursor = end + 1;
            continue;
        }

        let end = safe[cursor..]
            .find('<')
            .map(|offset| cursor + offset)
            .unwrap_or(safe.len());
        text.push_str(&safe[cursor..end]);
        cursor = end;
    }
    flush_fragment_text(&mut fragments, &mut text, &active);
    fragments
}

fn image_source_allowed(src: &str) -> bool {
    (src.starts_with("http://") || src.starts_with("https://") || src.starts_with("cid:"))
        && src.len() <= 4096
        && !src.chars().any(char::is_whitespace)
}

fn flush_fragment_text(
    fragments: &mut Vec<HtmlFragment>,
    text: &mut String,
    active: &[(String, String, String)],
) {
    if text.is_empty() {
        return;
    }
    let mut markup = String::new();
    for (_, opening, _) in active {
        markup.push_str(opening);
    }
    append_pango_text(&mut markup, text);
    for (_, _, closing) in active.iter().rev() {
        markup.push_str(closing);
    }
    push_markup_fragment(fragments, markup);
    text.clear();
}

fn push_markup_fragment(fragments: &mut Vec<HtmlFragment>, markup: String) {
    if markup.is_empty() {
        return;
    }
    if let Some(HtmlFragment::Markup(previous)) = fragments.last_mut() {
        previous.push_str(&markup);
    } else {
        fragments.push(HtmlFragment::Markup(markup));
    }
}

fn push_block_break(fragments: &mut Vec<HtmlFragment>) {
    if fragments.last().is_some_and(
        |fragment| matches!(fragment, HtmlFragment::Markup(markup) if markup.ends_with('\n')),
    ) {
        return;
    }
    push_markup_fragment(fragments, "\n".into());
}

fn append_html_tag(
    fragments: &mut Vec<HtmlFragment>,
    active: &mut Vec<(String, String, String)>,
    tag: &str,
    name: &str,
    closing: bool,
) {
    if closing {
        if matches!(
            name,
            "a" | "b"
                | "strong"
                | "i"
                | "em"
                | "u"
                | "code"
                | "pre"
                | "span"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
        ) && let Some(index) = active.iter().rposition(|(known, _, _)| known == name)
        {
            active.remove(index);
        }
        if matches!(
            name,
            "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
        ) {
            push_block_break(fragments);
        } else if name == "td" || name == "th" {
            push_markup_fragment(fragments, "  ".into());
        }
        return;
    }

    match name {
        "br" => push_markup_fragment(fragments, "\n".into()),
        "hr" => push_markup_fragment(fragments, "\n────────\n".into()),
        "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            if !fragments.is_empty() {
                push_block_break(fragments);
            }
            if matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
                active.push((name.to_string(), "<b>".into(), "</b>".into()));
            }
        }
        "blockquote" => push_markup_fragment(fragments, "\n│ ".into()),
        "td" | "th" => {
            if !fragments.is_empty() {
                push_markup_fragment(fragments, "  ".into());
            }
        }
        "b" | "strong" => active.push((name.into(), "<b>".into(), "</b>".into())),
        "i" | "em" => active.push((name.into(), "<i>".into(), "</i>".into())),
        "u" => active.push((name.into(), "<u>".into(), "</u>".into())),
        "code" | "pre" => active.push((name.into(), "<tt>".into(), "</tt>".into())),
        "span" => {
            if let Some(markup) = pango_style_markup(tag) {
                active.push((name.into(), markup, "</span>".into()));
            }
        }
        "a" => {
            if href_from_tag(tag).is_some() {
                active.push((name.into(), "<u>".into(), "</u>".into()));
            }
        }
        _ => {}
    }
}

fn pango_style_markup(tag: &str) -> Option<String> {
    let style = attribute_value(tag, "style")?;
    let mut attributes = Vec::new();
    for declaration in style.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let property = property.trim().to_ascii_lowercase();
        let value = value.trim();
        match property.as_str() {
            "color" => {
                if let Some(color) = safe_css_color(value) {
                    attributes.push(format!("foreground=\"{color}\""));
                }
            }
            "background-color" => {
                if let Some(color) = safe_css_color(value) {
                    attributes.push(format!("background=\"{color}\""));
                }
            }
            "font-weight" if value.eq_ignore_ascii_case("bold") || value == "700" => {
                attributes.push("weight=\"bold\"".into());
            }
            "text-decoration" if value.eq_ignore_ascii_case("underline") => {
                attributes.push("underline=\"single\"".into());
            }
            _ => {}
        }
    }
    (!attributes.is_empty()).then(|| format!("<span {}>", attributes.join(" ")))
}

fn safe_css_color(value: &str) -> Option<String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let named = matches!(
        lower.as_str(),
        "black"
            | "blue"
            | "gray"
            | "green"
            | "grey"
            | "maroon"
            | "navy"
            | "olive"
            | "orange"
            | "purple"
            | "red"
            | "silver"
            | "teal"
            | "white"
            | "yellow"
    );
    let hex = value.starts_with('#')
        && matches!(value.len(), 4 | 7 | 9)
        && value[1..]
            .chars()
            .all(|character| character.is_ascii_hexdigit());
    (named || hex).then(|| value.to_string())
}

/// Returns only network image URLs from already-sanitised HTML. Local files,
/// data URLs, cid resources, and malformed values never leave the MIME layer.
pub fn remote_image_urls(html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    collect_remote_image_urls(&html_document(html), &mut urls, false);
    urls
}

fn collect_remote_image_urls(node: &HtmlNode, urls: &mut Vec<String>, inside_style: bool) {
    match node {
        HtmlNode::Document(children) => {
            for child in children {
                collect_remote_image_urls(child, urls, inside_style);
            }
        }
        HtmlNode::Text(text) => {
            if inside_style {
                for url in remote_css_urls(text) {
                    push_remote_image_url(urls, url);
                }
            }
        }
        HtmlNode::Element {
            name,
            attributes,
            children,
        } => {
            for (attribute, value) in attributes {
                if (name == "img" && attribute == "src") || attribute == "background" {
                    if is_remote_image_url(value) {
                        push_remote_image_url(urls, value.clone());
                    }
                } else if attribute == "style" {
                    for url in remote_css_urls(value) {
                        push_remote_image_url(urls, url);
                    }
                }
            }
            for child in children {
                collect_remote_image_urls(child, urls, inside_style || name == "style");
            }
        }
    }
}

fn remote_css_urls(css: &str) -> Vec<String> {
    let lower = css.to_ascii_lowercase();
    let mut urls = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find("url(") {
        let start = cursor + relative;
        let Some(end_relative) = css[start + 4..].find(')') else {
            break;
        };
        let value = css[start + 4..start + 4 + end_relative]
            .trim()
            .trim_matches(['\'', '"'])
            .trim();
        if is_remote_image_url(value) && !urls.iter().any(|known| known == value) {
            urls.push(value.to_string());
        }
        cursor = start + 4 + end_relative + 1;
    }
    urls
}

fn is_remote_image_url(value: &str) -> bool {
    (value.starts_with("https://") || value.starts_with("http://"))
        && value.len() <= 4096
        && !value.chars().any(char::is_whitespace)
}

fn push_remote_image_url(urls: &mut Vec<String>, url: String) {
    if !urls.iter().any(|known| known == &url) {
        urls.push(url);
    }
}

fn strip_dangerous_blocks(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let dangerous = ["script", "iframe", "object", "embed", "svg"];
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

/// Converts already-sanitised HTML into readable text for previews and search.
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

fn attribute_value(tag: &str, attribute: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let marker = format!("{attribute}=");
    let start = lower.find(&marker)? + marker.len();
    let value = tag[start..].trim_start();
    if let Some(value) = value.strip_prefix('"') {
        return value.split_once('"').map(|(value, _)| value.to_string());
    }
    if let Some(value) = value.strip_prefix('\'') {
        return value.split_once('\'').map(|(value, _)| value.to_string());
    }
    value.split_whitespace().next().map(ToOwned::to_owned)
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
    fn parses_the_multipart_fixture_with_unicode_headers_and_attachment() {
        let parsed = parse(include_bytes!("../../tests/fixtures/multipart-utf8.eml"))
            .expect("fixture MIME parse");
        assert!(parsed.sender.contains("Jane Smüth"));
        assert!(parsed.subject.contains("Welcome"));
        assert!(parsed.body.contains("café"));
        assert_eq!(parsed.references, vec!["<fixture-root@example.com>"]);
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename, "notes.txt");
        assert_eq!(parsed.attachments[0].bytes, b"notes from a fixture\n");
    }

    #[test]
    fn parses_related_fixture_without_exposing_dangerous_markup() {
        let parsed = parse(include_bytes!(
            "../../tests/fixtures/related-remote-image.eml"
        ))
        .expect("related fixture MIME parse");
        assert!(parsed.is_html);
        assert!(
            parsed
                .body
                .contains("https://images.example.test/header.png")
        );
        assert!(!parsed.body.contains("script"));
        assert_eq!(
            remote_image_urls(&parsed.body),
            vec!["https://images.example.test/header.png"]
        );
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(
            parsed.attachments[0].content_id.as_deref(),
            Some("logo@example.com")
        );
    }

    #[test]
    fn tolerates_a_truncated_multipart_fixture() {
        let parsed = parse(include_bytes!(
            "../../tests/fixtures/malformed-truncated.eml"
        ))
        .expect("truncated MIME should remain readable");
        assert!(parsed.body.contains("ends before its MIME boundary"));
    }

    #[test]
    fn preserves_inline_image_content_id_and_lists_only_safe_remote_images() {
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
        assert!(
            !remote_image_urls(&parsed.body)
                .iter()
                .any(|url| url.contains("remote"))
        );
        let safe = sanitize_html(
            r#"<p>Hello<img src="https://images.example/logo.png"><img src="file:///etc/passwd"><img src="data:image/png;base64,bad"></p>"#,
        );
        assert!(safe.contains("https://images.example/logo.png"));
        assert_eq!(
            remote_image_urls(&safe),
            vec!["https://images.example/logo.png"]
        );
        assert_eq!(
            remote_image_urls(
                r#"<style>.hero { background-image: url('https://images.example/hero.png'); }</style><div style="background:url(https://images.example/tile.png)">Mail</div>"#
            ),
            vec![
                "https://images.example/hero.png",
                "https://images.example/tile.png"
            ]
        );
    }

    #[test]
    fn keeps_safe_images_in_document_order_for_native_rendering() {
        let fragments = html_fragments(
            r#"<p>Hello <strong>there</strong><img src="https://images.example/logo.png" alt="Logo"> after</p>"#,
        );
        assert!(matches!(
            fragments.as_slice(),
            [
                HtmlFragment::Markup(before),
                HtmlFragment::Image { src, alt },
                HtmlFragment::Markup(after),
            ] if before.contains("Hello ")
                && before.contains("<b>there</b>")
                && src == "https://images.example/logo.png"
                && alt == "Logo"
                && after.contains("after")
        ));
    }

    #[test]
    fn keeps_cid_images_but_rejects_other_image_sources() {
        let fragments = html_fragments(
            r#"<p>Start<img src="cid:logo@example.com"><img src="file:///tmp/logo.png" alt="Local">End</p>"#,
        );
        assert!(fragments.iter().any(|fragment| matches!(
            fragment,
            HtmlFragment::Image { src, .. } if src == "cid:logo@example.com"
        )));
        assert!(fragments.iter().any(|fragment| matches!(
            fragment,
            HtmlFragment::Markup(markup) if markup.contains("Local")
        )));
        assert!(!fragments.iter().any(|fragment| matches!(
            fragment,
            HtmlFragment::Image { src, .. } if src.starts_with("file:")
        )));
    }

    #[test]
    fn preserves_safe_table_structure_and_layout_hints() {
        let document = html_document(
            r#"<table style="width: 640px; background-image: url(https://tracker.invalid/x)" width="640"><tr><td style="padding: 12px; text-align: center"><strong>Hello</strong></td><td>World</td></tr></table>"#,
        );
        let HtmlNode::Document(children) = document else {
            panic!("expected document node");
        };
        let HtmlNode::Element {
            name,
            attributes,
            children: rows,
        } = &children[0]
        else {
            panic!("expected table node");
        };
        assert_eq!(name, "table");
        assert!(
            attributes
                .iter()
                .any(|(name, value)| { name == "width" && value == "640" })
        );
        assert!(
            attributes
                .iter()
                .any(|(name, value)| { name == "style" && value.contains("background-image") })
        );
        let HtmlNode::Element {
            name: section_name,
            children: section_children,
            ..
        } = &rows[0]
        else {
            panic!("expected tbody node");
        };
        assert_eq!(section_name, "tbody");
        assert!(matches!(
            &section_children[0],
            HtmlNode::Element { name, children, .. }
                if name == "tr" && children.len() == 2
        ));
        assert!(html_node_markup(&section_children[0]).contains("<strong>Hello</strong>"));
    }

    #[test]
    fn prepares_real_world_html_for_webkit_without_destroying_layout_css() {
        let html = concat!(
            "<!doctype html><html><head>",
            "<style>",
            ".button { font-family: Arial, sans-serif; font-size: 18px; ",
            "color: #ffffff; background: #123456; padding: 12px 20px; }",
            "</style></head><body>",
            "<table width=\"640\" style=\"width:640px; background-color:#f5f5f5; ",
            "background-image:url(https://images.example.test/background.png); ",
            "font-family:Arial,sans-serif;\"><tr><td style=\"padding:24px;\">",
            "<a class=\"button\" href=\"https://example.test/read\">Read message</a>",
            "<img src=\"https://images.example.test/hero.png\" alt=\"Hero\" ",
            "width=\"600\" style=\"display:block;\"></td></tr></table>",
            "</body></html>"
        );

        let prepared = prepare_html_for_webview(html, &[], true, "#d6d5bc");
        assert_eq!(prepared.matches("<body>").count(), 1);
        assert!(prepared.contains("font-family: Arial"));
        assert!(prepared.contains("font-size: 18px"));
        assert!(prepared.contains("background: #123456"));
        assert!(prepared.contains("<table"));
        assert!(prepared.contains("<a class=\"button\""));
        assert!(prepared.contains("https://images.example.test/hero.png"));

        let blocked = prepare_html_for_webview(html, &[], false, "#d6d5bc");
        assert!(blocked.contains("font-family: Arial"));
        assert!(blocked.contains("<table"));
        assert!(blocked.contains("<a class=\"button\""));
        assert!(!blocked.contains("https://images.example.test/hero.png"));
        assert!(!blocked.contains("https://images.example.test/background.png"));
        assert!(blocked.contains("data:image/svg+xml;base64,"));
        assert!(!blocked.contains("@import"));
    }
}
