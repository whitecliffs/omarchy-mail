use crate::mail::oauth::{self, OAuthTokens};
use crate::models::AuthMethod;
use keyring::Entry;
use std::sync::{Mutex, OnceLock};
use thiserror::Error;

const SERVICE: &str = "org.omarchy.Mail";
static KEYRING_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMaterial {
    Password(String),
    OAuth2AccessToken(String),
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("the system keyring rejected the credential: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("the system keyring did not retain the newly stored credential")]
    VerificationFailed,
}

fn entry(account_email: &str, protocol: &str) -> Result<Entry, CredentialError> {
    Ok(Entry::new(SERVICE, &format!("{protocol}:{account_email}"))?)
}

fn keyring_lock() -> std::sync::MutexGuard<'static, ()> {
    KEYRING_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn store_password(
    account_email: &str,
    protocol: &str,
    password: &str,
) -> Result<(), CredentialError> {
    let _lock = keyring_lock();
    let entry = entry(account_email, protocol)?;
    entry.set_password(password)?;
    if entry.get_password()? != password {
        return Err(CredentialError::VerificationFailed);
    }
    Ok(())
}

pub fn store_auth_material(
    account_email: &str,
    protocol: &str,
    method: &AuthMethod,
    secret: &str,
) -> Result<(), CredentialError> {
    match method {
        AuthMethod::Password => store_password(account_email, protocol, secret),
        AuthMethod::OAuth2 => store_oauth2_token(account_email, protocol, secret),
    }
}

pub fn load_password(account_email: &str, protocol: &str) -> Result<String, CredentialError> {
    let _lock = keyring_lock();
    Ok(entry(account_email, protocol)?.get_password()?)
}

pub fn delete_password(account_email: &str, protocol: &str) -> Result<(), CredentialError> {
    let _lock = keyring_lock();
    entry(account_email, protocol)?.delete_credential()?;
    Ok(())
}

pub fn delete_auth_materials(account_email: &str, protocol: &str) -> Result<(), CredentialError> {
    for result in [
        delete_password(account_email, protocol),
        delete_oauth2_token(account_email, protocol),
    ] {
        match result {
            Ok(()) | Err(CredentialError::Keyring(keyring::Error::NoEntry)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Stores provider tokens separately from password entries. Keeping this
/// boundary in the credential store means OAuth authorization can evolve
/// without ever putting access tokens into the database or configuration files.
#[allow(dead_code)]
pub fn store_oauth2_token(
    account_email: &str,
    protocol: &str,
    token: &str,
) -> Result<(), CredentialError> {
    let _lock = keyring_lock();
    let entry = entry(account_email, &format!("oauth2:{protocol}"))?;
    entry.set_password(token)?;
    if entry.get_password()? != token {
        return Err(CredentialError::VerificationFailed);
    }
    Ok(())
}

pub fn load_oauth2_token(account_email: &str, protocol: &str) -> Result<String, CredentialError> {
    let _lock = keyring_lock();
    Ok(entry(account_email, &format!("oauth2:{protocol}"))?.get_password()?)
}

pub fn store_oauth2_tokens(
    account_email: &str,
    protocol: &str,
    tokens: &OAuthTokens,
) -> Result<(), CredentialError> {
    let serialized =
        serde_json::to_string(tokens).map_err(|_error| CredentialError::VerificationFailed)?;
    store_oauth2_token(account_email, protocol, &serialized)
}

pub fn load_oauth2_tokens(
    account_email: &str,
    protocol: &str,
) -> Result<OAuthTokens, CredentialError> {
    let stored = load_oauth2_token(account_email, protocol)?;
    if let Ok(tokens) = serde_json::from_str::<OAuthTokens>(&stored) {
        return Ok(tokens);
    }
    Ok(OAuthTokens {
        access_token: stored,
        refresh_token: None,
        expires_at: None,
    })
}

#[allow(dead_code)]
pub fn delete_oauth2_token(account_email: &str, protocol: &str) -> Result<(), CredentialError> {
    let _lock = keyring_lock();
    entry(account_email, &format!("oauth2:{protocol}"))?.delete_credential()?;
    Ok(())
}

pub fn load_auth_material(
    account_email: &str,
    protocol: &str,
    method: &AuthMethod,
) -> Result<AuthMaterial, CredentialError> {
    match method {
        AuthMethod::Password => load_password(account_email, protocol).map(AuthMaterial::Password),
        AuthMethod::OAuth2 => {
            let mut tokens = load_oauth2_tokens(account_email, protocol)?;
            if tokens.is_expired()
                && let (Some(refresh_token), Some(provider)) = (
                    tokens.refresh_token.as_deref(),
                    oauth::provider_for_email(account_email),
                )
                && let Ok(client_id) = oauth::configured_client_id(provider)
                && let Ok(mut refreshed) =
                    oauth::refresh_access_token(provider, &client_id, refresh_token)
            {
                if refreshed.refresh_token.is_none() {
                    refreshed.refresh_token = tokens.refresh_token.clone();
                }
                let _ = store_oauth2_tokens(account_email, protocol, &refreshed);
                tokens = refreshed;
            }
            Ok(AuthMaterial::OAuth2AccessToken(tokens.access_token))
        }
    }
}

pub fn friendly_load_error(protocol: &str, method: &AuthMethod, error: &CredentialError) -> String {
    if matches!(error, CredentialError::Keyring(keyring::Error::NoEntry)) {
        return match method {
            AuthMethod::Password => format!("No {protocol} password is stored for this account."),
            AuthMethod::OAuth2 => {
                format!("No {protocol} OAuth2 access token is stored for this account.")
            }
        };
    }
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_missing_oauth_tokens_without_exposing_secret_data() {
        let error = CredentialError::Keyring(keyring::Error::NoEntry);
        let message = friendly_load_error("IMAP", &AuthMethod::OAuth2, &error);
        assert_eq!(
            message,
            "No IMAP OAuth2 access token is stored for this account."
        );
        assert!(!message.contains("token-value"));
    }
}
