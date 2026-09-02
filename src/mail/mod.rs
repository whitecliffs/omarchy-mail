pub mod credentials;
pub mod imap;
pub mod mime;
pub mod oauth;
pub mod outbox;
pub mod smtp;
pub mod sync;

pub fn valid_email(address: &str) -> bool {
    let address = address.trim();
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

/// Splits the compact address-field syntax used by the composer. Commas and
/// semicolons are both accepted because users commonly paste either form.
pub fn split_recipients(value: &str) -> Vec<String> {
    value
        .split([',', ';'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn validate_recipients(value: &str) -> Result<Vec<String>, String> {
    let recipients = split_recipients(value);
    for recipient in &recipients {
        recipient
            .parse::<lettre::message::Mailbox>()
            .map_err(|_| format!("Check this recipient: {recipient}"))?;
    }
    Ok(recipients)
}

pub fn domain_for(address: &str) -> Option<&str> {
    address.trim().split_once('@').map(|(_, domain)| domain)
}

pub fn discover_servers(address: &str) -> (String, u16, String, u16) {
    let domain = domain_for(address)
        .unwrap_or("example.com")
        .to_ascii_lowercase();
    match domain.as_str() {
        "gmail.com" | "googlemail.com" => {
            ("imap.gmail.com".into(), 993, "smtp.gmail.com".into(), 465)
        }
        "outlook.com" | "hotmail.com" | "live.com" => (
            "outlook.office365.com".into(),
            993,
            "smtp.office365.com".into(),
            587,
        ),
        "icloud.com" | "me.com" | "mac.com" => (
            "imap.mail.me.com".into(),
            993,
            "smtp.mail.me.com".into(),
            587,
        ),
        _ => (format!("imap.{domain}"), 993, format!("smtp.{domain}"), 465),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_normal_email_addresses() {
        assert!(valid_email("jim@example.com"));
        assert!(valid_email("name+tag@example.co.uk"));
        assert!(!valid_email("jim"));
        assert!(!valid_email("@example.com"));
        assert!(!valid_email("jim@example"));
    }

    #[test]
    fn splits_and_validates_pasted_recipient_lists() {
        let recipients = split_recipients("jane@example.com; Team <team@example.com>");
        assert_eq!(recipients, ["jane@example.com", "Team <team@example.com>"]);
        assert_eq!(
            validate_recipients("jane@example.com,team@example.com").expect("recipients"),
            ["jane@example.com", "team@example.com"]
        );
        assert!(validate_recipients("not-an-address").is_err());
    }

    #[test]
    fn discovers_common_provider_endpoints_without_special_casing_the_ui() {
        assert_eq!(discover_servers("jim@gmail.com").0, "imap.gmail.com");
        assert_eq!(discover_servers("jim@outlook.com").2, "smtp.office365.com");
        assert_eq!(
            discover_servers("jim@my-domain.test").0,
            "imap.my-domain.test"
        );
    }
}
