use crate::models::AuthMethod;
use keyring::Entry;
use thiserror::Error;

const SERVICE: &str = "org.omarchy.Mail";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMaterial {
    Password(String),
    OAuth2AccessToken(String),
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("the system keyring rejected the credential: {0}")]
    Keyring(#[from] keyring::Error),
}

fn entry(account_email: &str, protocol: &str) -> Result<Entry, CredentialError> {
    Ok(Entry::new(SERVICE, &format!("{protocol}:{account_email}"))?)
}

pub fn store_password(
    account_email: &str,
    protocol: &str,
    password: &str,
) -> Result<(), CredentialError> {
    entry(account_email, protocol)?.set_password(password)?;
    Ok(())
}

pub fn load_password(account_email: &str, protocol: &str) -> Result<String, CredentialError> {
    Ok(entry(account_email, protocol)?.get_password()?)
}

pub fn delete_password(account_email: &str, protocol: &str) -> Result<(), CredentialError> {
    entry(account_email, protocol)?.delete_credential()?;
    Ok(())
}

/// Stores provider tokens separately from legacy password entries. The
/// transport layers still use password login today; keeping this boundary in
/// the credential store means OAuth authorization can be added without ever
/// putting access tokens into the database or configuration files.
#[allow(dead_code)]
pub fn store_oauth2_token(
    account_email: &str,
    protocol: &str,
    token: &str,
) -> Result<(), CredentialError> {
    entry(account_email, &format!("oauth2:{protocol}"))?.set_password(token)?;
    Ok(())
}

pub fn load_oauth2_token(account_email: &str, protocol: &str) -> Result<String, CredentialError> {
    Ok(entry(account_email, &format!("oauth2:{protocol}"))?.get_password()?)
}

#[allow(dead_code)]
pub fn delete_oauth2_token(account_email: &str, protocol: &str) -> Result<(), CredentialError> {
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
            load_oauth2_token(account_email, protocol).map(AuthMaterial::OAuth2AccessToken)
        }
    }
}
