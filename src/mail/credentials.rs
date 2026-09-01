use keyring::Entry;
use thiserror::Error;

const SERVICE: &str = "org.omarchy.Mail";

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("the system keyring rejected the credential: {0}")]
    Keyring(#[from] keyring::Error),
}

pub fn store_password(username: &str, password: &str) -> Result<(), CredentialError> {
    Entry::new(SERVICE, username)?.set_password(password)?;
    Ok(())
}

pub fn load_password(username: &str) -> Result<String, CredentialError> {
    Ok(Entry::new(SERVICE, username)?.get_password()?)
}

pub fn delete_password(username: &str) -> Result<(), CredentialError> {
    Entry::new(SERVICE, username)?.delete_credential()?;
    Ok(())
}
