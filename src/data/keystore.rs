/// Thin wrapper around the system keyring (GNOME Secret Service on Linux).
///
/// API keys are stored here rather than in SQLite so that they are protected by
/// the user's login keyring and not readable by other processes via the plain DB file.
///
/// All functions are synchronous (the keyring crate performs synchronous D-Bus calls).
use anyhow::Result;

const SERVICE: &str = "io.github.rorynuijens.Cycle";

pub const KEY_ANTHROPIC: &str = "anthropic.api_key";
pub const KEY_INTERVALS_API: &str = "intervals.api_key";

/// Retrieve a secret. Returns `Ok(None)` when the key does not exist in the keyring.
pub fn get_secret(key: &str) -> Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE, key)?;
    match entry.get_password() {
        Ok(val) => Ok(Some(val)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("keyring get_secret({key}): {e}")),
    }
}

/// Store a secret. An empty string is treated as "delete the entry".
pub fn set_secret(key: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        delete_secret(key)?;
        return Ok(());
    }
    let entry = keyring::Entry::new(SERVICE, key)?;
    entry.set_password(value)?;
    Ok(())
}

/// Delete a secret. Succeeds silently if the entry does not exist.
pub fn delete_secret(key: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, key)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("keyring delete_secret({key}): {e}")),
    }
}
