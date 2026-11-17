//! Credentials never go to SQLite or the settings file, and are never sent
//! back to the frontend. Two backends:
//!
//! * the OS keychain (Keychain / Credential Manager / Secret Service), the
//!   default on a desktop;
//! * an encrypted file `secrets.enc` in the app folder (ChaCha20-Poly1305, key
//!   from the `KISSA_PASSPHRASE` environment variable via Argon2id), for
//!   machines that have no keychain, such as a server running headless.
//!
//! The backend is chosen once per run: `KISSA_SECRETS=file|keychain` forces
//! one; otherwise the keychain is probed, and the file is used when the
//! keychain is unusable and the app runs headless or `secrets.enc` already
//! exists. The file is private to this process (the instance lock allows only
//! one) and is rewritten atomically with mode 0600 on Unix.

use anyhow::{anyhow, Context, Result};
use argon2::Argon2;
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tracing::{info, warn};

const SERVICE: &str = "kissa";
const FILE_NAME: &str = "secrets.enc";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Keychain,
    File,
}

fn backend() -> Backend {
    static B: OnceLock<Backend> = OnceLock::new();
    *B.get_or_init(detect)
}

/// "OS keychain" or "encrypted file" (for status output).
pub fn backend_name() -> &'static str {
    match backend() {
        Backend::Keychain => "OS keychain",
        Backend::File => "encrypted file",
    }
}

fn file_path() -> Result<PathBuf> {
    Ok(crate::settings::setup_app_folders()?.root.join(FILE_NAME))
}

fn detect() -> Backend {
    match std::env::var("KISSA_SECRETS").ok().as_deref().map(str::trim) {
        Some("file") => {
            info!("Secrets: using the encrypted file (KISSA_SECRETS=file)");
            return Backend::File;
        }
        Some("keychain") => return Backend::Keychain,
        _ => {}
    }
    match Entry::new(SERVICE, "probe:availability").and_then(|e| e.get_password()) {
        Ok(_) | Err(keyring::Error::NoEntry) => Backend::Keychain,
        Err(e) => {
            let have_file = file_path().map(|p| p.exists()).unwrap_or(false);
            if crate::headless::is_headless() || have_file {
                warn!("OS keychain unavailable ({e}); secrets use the encrypted file {FILE_NAME}");
                Backend::File
            } else {
                Backend::Keychain
            }
        }
    }
}

// ------------------------------------------------------------- public API ---

pub fn set(account_id: &str, field: &str, value: &str) -> Result<()> {
    match backend() {
        Backend::Keychain => keychain_entry(account_id, field)?
            .set_password(value)
            .map_err(|e| anyhow!("could not save {field} to the OS keychain: {e}")),
        Backend::File => file_set(&name(account_id, field), Some(value)),
    }
}

pub fn get(account_id: &str, field: &str) -> Result<Option<String>> {
    match backend() {
        Backend::Keychain => match keychain_entry(account_id, field)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!("could not read {field} from the OS keychain: {e}")),
        },
        Backend::File => file_get(&name(account_id, field)),
    }
}

pub fn delete(account_id: &str, field: &str) -> Result<()> {
    match backend() {
        Backend::Keychain => match keychain_entry(account_id, field)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow!("could not delete {field} from the OS keychain: {e}")),
        },
        Backend::File => file_set(&name(account_id, field), None),
    }
}

fn name(account_id: &str, field: &str) -> String {
    format!("{account_id}:{field}")
}

fn keychain_entry(account_id: &str, field: &str) -> Result<Entry> {
    Entry::new(SERVICE, &name(account_id, field)).map_err(|e| anyhow!("keychain: {e}"))
}

// ---------------------------------------------------------- encrypted file ---

#[derive(Serialize, Deserialize)]
struct Envelope {
    v: u8,
    salt: String,
    nonce: String,
    data: String,
}

struct FileStore {
    salt: Vec<u8>,
    key: [u8; 32],
    map: BTreeMap<String, String>,
}

static STORE: Mutex<Option<FileStore>> = Mutex::new(None);

fn passphrase() -> Result<String> {
    std::env::var("KISSA_PASSPHRASE").ok().filter(|p| !p.is_empty()).ok_or_else(|| {
        anyhow!("Secrets are kept in the encrypted file {FILE_NAME}: set the KISSA_PASSPHRASE environment variable to unlock it")
    })
}

fn derive(pass: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(pass.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(anyhow!("bad hex"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| anyhow!("bad hex")))
        .collect()
}

fn open_store(path: &Path) -> Result<FileStore> {
    let pass = passphrase()?;
    if !path.exists() {
        let mut salt = vec![0u8; 16];
        OsRng.fill_bytes(&mut salt);
        let key = derive(&pass, &salt)?;
        return Ok(FileStore { salt, key, map: BTreeMap::new() });
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let env: Envelope = serde_json::from_str(&text).map_err(|e| anyhow!("{FILE_NAME} is damaged: {e}"))?;
    if env.v != 1 {
        return Err(anyhow!("{FILE_NAME} has an unknown version ({})", env.v));
    }
    let salt = unhex(&env.salt)?;
    let nonce = unhex(&env.nonce)?;
    let data = unhex(&env.data)?;
    if nonce.len() != 12 {
        return Err(anyhow!("{FILE_NAME} is damaged (nonce)"));
    }
    let key = derive(&pass, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce), data.as_ref())
        .map_err(|_| anyhow!("could not decrypt {FILE_NAME}: wrong KISSA_PASSPHRASE or damaged file"))?;
    let map: BTreeMap<String, String> =
        serde_json::from_slice(&plain).map_err(|e| anyhow!("{FILE_NAME} is damaged: {e}"))?;
    Ok(FileStore { salt, key, map })
}

fn persist(st: &FileStore, path: &Path) -> Result<()> {
    let plain = serde_json::to_vec(&st.map)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&st.key));
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let data = cipher.encrypt(&nonce, plain.as_ref()).map_err(|_| anyhow!("encryption failed"))?;
    let env = Envelope { v: 1, salt: hex(&st.salt), nonce: hex(nonce.as_slice()), data: hex(&data) };
    let bytes = serde_json::to_vec(&env)?;

    let tmp = path.with_extension("enc.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        use std::io::Write;
        let mut f = opts.open(&tmp).with_context(|| format!("could not write {}", tmp.display()))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

fn with_store<T>(f: impl FnOnce(&mut FileStore, &Path) -> Result<T>) -> Result<T> {
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let path = file_path()?;
    if guard.is_none() {
        *guard = Some(open_store(&path)?);
    }
    match guard.as_mut() {
        Some(st) => f(st, &path),
        None => Err(anyhow!("secrets store unavailable")),
    }
}

fn file_get(key: &str) -> Result<Option<String>> {
    with_store(|st, _| Ok(st.map.get(key).cloned()))
}

/// `Some(value)` stores, `None` deletes.
fn file_set(key: &str, value: Option<&str>) -> Result<()> {
    with_store(|st, path| {
        let changed = match value {
            Some(v) => st.map.insert(key.to_string(), v.to_string()).as_deref() != Some(v),
            None => st.map.remove(key).is_some(),
        };
        if changed { persist(st, path) } else { Ok(()) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let b = [0u8, 1, 15, 16, 254, 255];
        assert_eq!(unhex(&hex(&b)).unwrap(), b);
        assert!(unhex("abc").is_err());
        assert!(unhex("zz").is_err());
    }

    #[test]
    fn file_round_trip_and_wrong_passphrase() {
        let dir = std::env::temp_dir().join(format!("kissa-secrets-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);

        let mut salt = vec![0u8; 16];
        OsRng.fill_bytes(&mut salt);
        let mut st = FileStore { key: derive("pw one", &salt).unwrap(), salt, map: BTreeMap::new() };
        st.map.insert("a:token".into(), "s3cret".into());
        persist(&st, &path).unwrap();

        // Read back with the right key.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("s3cret"), "the value must not be stored in clear");
        let env: Envelope = serde_json::from_str(&text).unwrap();
        let key = derive("pw one", &unhex(&env.salt).unwrap()).unwrap();
        let plain = ChaCha20Poly1305::new(Key::from_slice(&key))
            .decrypt(Nonce::from_slice(&unhex(&env.nonce).unwrap()), unhex(&env.data).unwrap().as_ref())
            .unwrap();
        let map: BTreeMap<String, String> = serde_json::from_slice(&plain).unwrap();
        assert_eq!(map.get("a:token").map(String::as_str), Some("s3cret"));

        // A different passphrase fails to decrypt.
        let bad = derive("pw two", &unhex(&env.salt).unwrap()).unwrap();
        assert!(ChaCha20Poly1305::new(Key::from_slice(&bad))
            .decrypt(Nonce::from_slice(&unhex(&env.nonce).unwrap()), unhex(&env.data).unwrap().as_ref())
            .is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
