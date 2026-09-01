use keyring::Entry;
use thiserror::Error;

const SERVICE: &str = "org.omarchy.Mail";

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
