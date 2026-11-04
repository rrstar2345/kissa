//! Credentials live only in the OS keychain (Keychain / Credential Manager /
//! Secret Service). Nothing secret is written to SQLite or the settings file,
//! and secrets are never sent back to the frontend.

use anyhow::{anyhow, Result};
use keyring::Entry;

const SERVICE: &str = "kissa";

fn entry(account_id: &str, field: &str) -> Result<Entry> {
    Entry::new(SERVICE, &format!("{account_id}:{field}")).map_err(|e| anyhow!("keychain: {e}"))
}

pub fn set(account_id: &str, field: &str, value: &str) -> Result<()> {
    entry(account_id, field)?
        .set_password(value)
        .map_err(|e| anyhow!("could not save {field} to the OS keychain: {e}"))
}

pub fn get(account_id: &str, field: &str) -> Result<Option<String>> {
    match entry(account_id, field)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow!("could not read {field} from the OS keychain: {e}")),
    }
}

pub fn delete(account_id: &str, field: &str) -> Result<()> {
    match entry(account_id, field)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow!("could not delete {field} from the OS keychain: {e}")),
    }
}
