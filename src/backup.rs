//! Encrypted, portable backups of an Omarchy Mail installation.
//!
//! File layout: a 43-byte header (magic, Argon2id parameters, salt, STREAM
//! nonce prefix) followed by a gzip-compressed tar archive encrypted with
//! ChaCha20-Poly1305 in 1 MiB STREAM chunks. Every chunk authenticates the
//! header, so a wrong passphrase, tampering, or truncation is detected.
//!
//! Restore is two-phase. `stage_restore` decrypts and validates into a sibling
//! staging directory while the app keeps running; `apply_pending_restore`
//! swaps the staged data in at the next start, before any database or worker
//! is opened. The replaced data is moved to `omarchy-mail-recovery/`.
use crate::database::{Database, SCHEMA_VERSION};
use crate::security;
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::aead::{OsRng, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const MAGIC: &[u8; 8] = b"OMAILBK\x01";
const HEADER_LEN: usize = 8 + 12 + 16 + 7;
const CHUNK: usize = 1 << 20;
const TAG: usize = 16;
pub const FORMAT: u32 = 1;
pub const EXTENSION: &str = "omailbackup";

#[cfg(not(test))]
const KDF: (u32, u32, u32) = (64 * 1024, 3, 1);
#[cfg(test)]
const KDF: (u32, u32, u32) = (64, 1, 1);

/// Data directories that make up one installation. Explicit so tests and a
/// future macOS port can supply their own locations.
#[derive(Debug, Clone)]
pub struct Locations {
    pub data: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
}

impl Locations {
    pub fn current() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let xdg = |name: &str, fallback: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
                .join("omarchy-mail")
        };
        Self {
            data: xdg("XDG_DATA_HOME", ".local/share"),
            config: xdg("XDG_CONFIG_HOME", ".config"),
            cache: xdg("XDG_CACHE_HOME", ".cache"),
        }
    }

    fn sibling(&self, name: &str) -> PathBuf {
        self.data
            .parent()
            .map(|parent| parent.join(name))
            .unwrap_or_else(|| PathBuf::from(name))
    }

    fn staging(&self) -> PathBuf {
        self.sibling("omarchy-mail-restore")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: u32,
    pub app_version: String,
    pub schema_version: i64,
    pub created_at: String,
    pub platform: String,
    pub data_dir: String,
    pub cache_dir: String,
    pub accounts: Vec<String>,
    pub includes_secrets: bool,
}

/// One keyring entry, addressed the same way `credentials::load_password`
/// addresses it (e.g. slot `imap`, `oauth2:smtp`, `icloud-calendar`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Secret {
    pub account: String,
    pub slot: String,
    pub value: String,
}

#[derive(Debug)]
pub struct Staged {
    pub manifest: Manifest,
    pub secrets: Vec<Secret>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

// ---------------------------------------------------------------- crypto

fn derive_key(passphrase: &str, salt: &[u8], (m, t, p): (u32, u32, u32)) -> io::Result<[u8; 32]> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| invalid(e.to_string()))?;
    let mut key = [0_u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| invalid(e.to_string()))?;
    Ok(key)
}

struct EncryptWriter<W: Write> {
    inner: W,
    encryptor: Option<EncryptorBE32<ChaCha20Poly1305>>,
    header: Vec<u8>,
    buffer: Vec<u8>,
}

impl<W: Write> EncryptWriter<W> {
    fn new(mut inner: W, passphrase: &str) -> io::Result<Self> {
        let mut salt = [0_u8; 16];
        let mut prefix = [0_u8; 7];
        OsRng.fill_bytes(&mut salt);
        OsRng.fill_bytes(&mut prefix);
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        for value in [KDF.0, KDF.1, KDF.2] {
            header.extend_from_slice(&value.to_le_bytes());
        }
        header.extend_from_slice(&salt);
        header.extend_from_slice(&prefix);
        inner.write_all(&header)?;
        let key = derive_key(passphrase, &salt, KDF)?;
        let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(&key));
        Ok(Self {
            inner,
            encryptor: Some(EncryptorBE32::from_aead(
                cipher,
                GenericArray::from_slice(&prefix),
            )),
            header,
            buffer: Vec::with_capacity(CHUNK * 2),
        })
    }

    fn seal_full_chunks(&mut self) -> io::Result<()> {
        // Keep at least one byte back so the final chunk is never ambiguous.
        while self.buffer.len() > CHUNK {
            let rest = self.buffer.split_off(CHUNK);
            let chunk = std::mem::replace(&mut self.buffer, rest);
            let sealed = self
                .encryptor
                .as_mut()
                .ok_or_else(|| invalid("backup already finished"))?
                .encrypt_next(Payload {
                    msg: &chunk,
                    aad: &self.header,
                })
                .map_err(|_| invalid("encryption failed"))?;
            self.inner.write_all(&sealed)?;
        }
        Ok(())
    }

    fn finish(mut self) -> io::Result<W> {
        self.seal_full_chunks()?;
        let sealed = self
            .encryptor
            .take()
            .ok_or_else(|| invalid("backup already finished"))?
            .encrypt_last(Payload {
                msg: &self.buffer,
                aad: &self.header,
            })
            .map_err(|_| invalid("encryption failed"))?;
        self.inner.write_all(&sealed)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for EncryptWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(data);
        self.seal_full_chunks()?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DecryptReader<R: Read> {
    inner: R,
    decryptor: Option<DecryptorBE32<ChaCha20Poly1305>>,
    header: Vec<u8>,
    plain: Vec<u8>,
    position: usize,
    lookahead: Option<u8>,
}

fn read_full(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

impl<R: Read> DecryptReader<R> {
    fn new(mut inner: R, passphrase: &str) -> io::Result<Self> {
        let mut header = vec![0_u8; HEADER_LEN];
        if read_full(&mut inner, &mut header)? != HEADER_LEN || &header[..8] != MAGIC {
            return Err(invalid("This is not an Omarchy Mail backup file."));
        }
        let number =
            |offset: usize| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
        let params = (number(8), number(12), number(16));
        // Refuse parameters that would let a crafted file exhaust memory or time.
        if params.0 > 1024 * 1024 || params.1 > 16 || params.2 > 16 {
            return Err(invalid("The backup uses unsupported encryption settings."));
        }
        let key = derive_key(passphrase, &header[20..36], params)?;
        let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(&key));
        let decryptor = DecryptorBE32::from_aead(cipher, GenericArray::from_slice(&header[36..43]));
        Ok(Self {
            inner,
            decryptor: Some(decryptor),
            header,
            plain: Vec::new(),
            position: 0,
            lookahead: None,
        })
    }

    fn refill(&mut self) -> io::Result<()> {
        if self.decryptor.is_none() {
            self.plain.clear();
            self.position = 0;
            return Ok(());
        }
        let mut sealed = Vec::with_capacity(CHUNK + TAG + 1);
        if let Some(byte) = self.lookahead.take() {
            sealed.push(byte);
        }
        let start = sealed.len();
        sealed.resize(CHUNK + TAG, 0);
        let read = read_full(&mut self.inner, &mut sealed[start..])?;
        sealed.truncate(start + read);
        let mut next = [0_u8; 1];
        let last = read_full(&mut self.inner, &mut next)? == 0;
        let failure = || invalid("Wrong passphrase, or the backup file is damaged.");
        self.plain = if last {
            self.decryptor
                .take()
                .ok_or_else(failure)?
                .decrypt_last(Payload {
                    msg: &sealed,
                    aad: &self.header,
                })
                .map_err(|_| failure())?
        } else {
            self.lookahead = Some(next[0]);
            self.decryptor
                .as_mut()
                .ok_or_else(failure)?
                .decrypt_next(Payload {
                    msg: &sealed,
                    aad: &self.header,
                })
                .map_err(|_| failure())?
        };
        self.position = 0;
        Ok(())
    }
}

impl<R: Read> Read for DecryptReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.plain.len() {
            self.refill()?;
            if self.plain.is_empty() {
                return Ok(0);
            }
        }
        let count = output.len().min(self.plain.len() - self.position);
        output[..count].copy_from_slice(&self.plain[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

// ---------------------------------------------------------------- create

/// Reads every keyring entry this installation uses. Missing entries are skipped.
pub fn collect_secrets(
    database: &Database,
    preferences: &crate::preferences::Preferences,
) -> Vec<Secret> {
    let mut wanted = Vec::new();
    for account in database.load_accounts().unwrap_or_default() {
        for slot in ["imap", "smtp", "oauth2:imap", "oauth2:smtp"] {
            wanted.push((account.email.clone(), slot.to_string()));
        }
    }
    if let Some(icloud) = &preferences.icloud_calendar {
        wanted.push((icloud.username.clone(), "icloud-calendar".into()));
    }
    if preferences.auto_backup.is_some() {
        wanted.push(("automatic".into(), "backup-passphrase".into()));
    }
    if let Some(google) = &preferences.google_calendar {
        wanted.push((
            google.email.clone(),
            format!("oauth2:{}", crate::google_calendar::KEYRING_PROTOCOL),
        ));
    }
    wanted
        .into_iter()
        .filter_map(|(account, slot)| {
            crate::mail::credentials::load_password(&account, &slot)
                .ok()
                .map(|value| Secret {
                    account,
                    slot,
                    value,
                })
        })
        .collect()
}

pub fn store_secrets(secrets: &[Secret]) -> Result<(), String> {
    let mut failed = 0;
    for secret in secrets {
        if crate::mail::credentials::store_password(&secret.account, &secret.slot, &secret.value)
            .is_err()
        {
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(format!(
            "{failed} saved password(s) could not be written to the system keyring; re-enter them in Settings."
        ));
    }
    Ok(())
}

fn append_bytes<W: Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    bytes: &[u8],
) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(chrono::Utc::now().timestamp().max(0) as u64);
    header.set_cksum();
    builder.append_data(&mut header, name, bytes)
}

pub fn create_backup(
    locations: &Locations,
    destination: &Path,
    passphrase: &str,
    secrets: &[Secret],
    include_secrets: bool,
) -> io::Result<Manifest> {
    if passphrase.chars().count() < 8 {
        return Err(invalid("Use a passphrase of at least 8 characters."));
    }
    let database = locations.data.join("mail.db");
    if !database.is_file() {
        return Err(invalid("There is no mail data to back up yet."));
    }
    // A private scratch copy gives a consistent snapshot while workers write.
    let scratch = locations
        .data
        .join(format!(".backup-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    security::ensure_private_dir(&scratch)?;
    let result = (|| {
        let snapshot = scratch.join("mail.db");
        let connection =
            rusqlite::Connection::open(&database).map_err(|e| invalid(e.to_string()))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(10))
            .map_err(|e| invalid(e.to_string()))?;
        connection
            .execute("VACUUM INTO ?1", [snapshot.to_string_lossy()])
            .map_err(|e| invalid(format!("Could not snapshot the mail database: {e}")))?;
        drop(connection);
        let accounts = Database::open(&scratch)
            .and_then(|db| db.load_accounts())
            .map_err(|e| invalid(e.to_string()))?
            .into_iter()
            .map(|account| account.email)
            .collect();
        let manifest = Manifest {
            format: FORMAT,
            app_version: env!("CARGO_PKG_VERSION").into(),
            schema_version: SCHEMA_VERSION,
            created_at: chrono::Local::now().to_rfc3339(),
            platform: std::env::consts::OS.into(),
            data_dir: locations.data.to_string_lossy().into(),
            cache_dir: locations.cache.to_string_lossy().into(),
            accounts,
            includes_secrets: include_secrets,
        };

        let temporary = destination.with_extension(format!("{}.partial", std::process::id()));
        let file = File::create(&temporary)?;
        security::set_private_file_permissions(&temporary)?;
        let writer = EncryptWriter::new(BufWriter::new(file), passphrase)?;
        let mut builder = tar::Builder::new(GzEncoder::new(writer, Compression::default()));
        builder.follow_symlinks(false);
        append_bytes(
            &mut builder,
            "manifest.json",
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        builder.append_path_with_name(&snapshot, "data/mail.db")?;
        for folder in ["drafts", "outbox", "subscriptions"] {
            let path = locations.data.join(folder);
            if path.is_dir() {
                builder.append_dir_all(format!("data/{folder}"), &path)?;
            }
        }
        for file in ["preferences.json", "oauth.json"] {
            let path = locations.config.join(file);
            if path.is_file() {
                builder.append_path_with_name(&path, format!("config/{file}"))?;
            }
        }
        if include_secrets {
            append_bytes(&mut builder, "secrets.json", &serde_json::to_vec(secrets)?)?;
        }
        let writer = builder.into_inner()?.finish()?;
        writer
            .finish()?
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        fs::rename(&temporary, destination).inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })?;
        Ok(manifest)
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}

// ---------------------------------------------------------------- restore

fn allowed_entry(path: &Path) -> bool {
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return false;
    }
    let text = path.to_string_lossy();
    matches!(
        text.as_ref(),
        "manifest.json"
            | "secrets.json"
            | "data"
            | "config"
            | "config/preferences.json"
            | "config/oauth.json"
    ) || text.starts_with("data/")
}

/// Decrypts and validates a backup into the staging directory. Nothing in the
/// live installation is touched; call `apply_pending_restore` on next start.
pub fn stage_restore(locations: &Locations, source: &Path, passphrase: &str) -> io::Result<Staged> {
    // Work in a private partial directory so a failed attempt never disturbs
    // a restore that was already verified and is waiting for a restart.
    let target = locations.staging();
    let staging = locations.sibling(&format!(
        "omarchy-mail-restore.partial-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&staging);
    security::ensure_private_dir(&staging)?;
    let result = (|| {
        let reader = DecryptReader::new(BufReader::new(File::open(source)?), passphrase)?;
        let mut archive = tar::Archive::new(GzDecoder::new(reader));
        let mut manifest: Option<Manifest> = None;
        let mut secrets = Vec::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if !allowed_entry(&path) {
                return Err(invalid(
                    "The backup contains an unexpected file and was not restored.",
                ));
            }
            match path.to_string_lossy().as_ref() {
                "manifest.json" => {
                    let mut bytes = Vec::new();
                    entry.by_ref().take(1 << 20).read_to_end(&mut bytes)?;
                    manifest = Some(serde_json::from_slice(&bytes)?);
                }
                "secrets.json" => {
                    // Kept in memory only; never written to the staging directory.
                    let mut bytes = Vec::new();
                    entry.by_ref().take(16 << 20).read_to_end(&mut bytes)?;
                    secrets = serde_json::from_slice(&bytes)?;
                }
                _ => {
                    let kind = entry.header().entry_type();
                    if !(kind.is_file() || kind.is_dir()) {
                        return Err(invalid("The backup contains an unsupported file type."));
                    }
                    if !entry.unpack_in(&staging)? {
                        return Err(invalid("The backup contains an unsafe path."));
                    }
                }
            }
        }
        let manifest = manifest.ok_or_else(|| invalid("The backup has no manifest."))?;
        if manifest.format != FORMAT {
            return Err(invalid(
                "This backup was made by an incompatible version of Omarchy Mail.",
            ));
        }
        if manifest.schema_version > SCHEMA_VERSION {
            return Err(invalid(
                "This backup is from a newer Omarchy Mail. Update the app, then restore.",
            ));
        }
        let database = staging.join("data/mail.db");
        let connection = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| invalid("The backup has no mail database."))?;
        let check: String = connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|e| invalid(e.to_string()))?;
        if check != "ok" {
            return Err(invalid("The mail database inside the backup is damaged."));
        }
        drop(connection);
        fs::write(
            staging.join("manifest.json"),
            serde_json::to_vec(&manifest)?,
        )?;
        // Written last: only a complete, verified staging directory is applied.
        fs::write(staging.join("READY"), b"1")?;
        let _ = fs::remove_dir_all(&target);
        fs::rename(&staging, &target)?;
        Ok(Staged { manifest, secrets })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Different filesystems (e.g. config on another mount): copy then remove.
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            move_path(&entry.path(), &to.join(entry.file_name()))?;
        }
        fs::remove_dir(from)
    } else {
        fs::copy(from, to)?;
        fs::remove_file(from)
    }
}

/// Rewrites stored absolute attachment paths from the backed-up machine's
/// directories to this machine's, so drafts and the outbox keep their files.
fn rewrite_paths(database: &Path, manifest: &Manifest, locations: &Locations) -> io::Result<()> {
    let connection = rusqlite::Connection::open(database).map_err(|e| invalid(e.to_string()))?;
    let pairs = [
        (
            &manifest.data_dir,
            locations.data.to_string_lossy().to_string(),
        ),
        (
            &manifest.cache_dir,
            locations.cache.to_string_lossy().to_string(),
        ),
    ];
    for (old, new) in pairs {
        if *old == new || old.is_empty() {
            continue;
        }
        let (old, new) = (format!("{old}/"), format!("{new}/"));
        for table in ["messages", "pending_sends"] {
            connection
                .execute(
                    &format!(
                        "UPDATE {table} SET attachments_json = REPLACE(attachments_json, ?1, ?2) WHERE instr(attachments_json, ?1) > 0"
                    ),
                    [&old, &new],
                )
                .map_err(|e| invalid(e.to_string()))?;
        }
    }
    Ok(())
}

static RESTORE_NOTICE: Mutex<Option<String>> = Mutex::new(None);

pub fn set_restore_notice(notice: String) {
    if let Ok(mut slot) = RESTORE_NOTICE.lock() {
        *slot = Some(notice);
    }
}

pub fn take_restore_notice() -> Option<String> {
    RESTORE_NOTICE.lock().ok()?.take()
}

/// Applies a verified staged restore. Must run before the database is opened.
pub fn apply_pending_restore(locations: &Locations) -> io::Result<Option<String>> {
    let staging = locations.staging();
    if !staging.join("READY").is_file() {
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        return Ok(None);
    }
    let manifest: Manifest = serde_json::from_slice(&fs::read(staging.join("manifest.json"))?)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let recovery = locations
        .sibling("omarchy-mail-recovery")
        .join(format!("before-restore-{stamp}"));
    security::ensure_private_dir(&recovery)?;
    if locations.data.exists() {
        move_path(&locations.data, &recovery.join("data"))?;
    }
    for file in ["preferences.json", "oauth.json"] {
        let live = locations.config.join(file);
        if live.exists() {
            move_path(&live, &recovery.join("config").join(file))?;
        }
    }
    move_path(&staging.join("data"), &locations.data)?;
    security::ensure_private_dir(&locations.data)?;
    if staging.join("config").is_dir() {
        security::ensure_private_dir(&locations.config)?;
        for file in ["preferences.json", "oauth.json"] {
            let staged = staging.join("config").join(file);
            if staged.is_file() {
                let live = locations.config.join(file);
                move_path(&staged, &live)?;
                security::set_private_file_permissions(&live)?;
            }
        }
    }
    rewrite_paths(&locations.data.join("mail.db"), &manifest, locations)?;
    let _ = fs::remove_dir_all(&staging);
    let notice = format!(
        "Restored backup from {} ({} account(s)). Previous data kept in {}",
        manifest
            .created_at
            .get(..10)
            .unwrap_or(&manifest.created_at),
        manifest.accounts.len(),
        recovery.display()
    );
    set_restore_notice(notice.clone());
    Ok(Some(notice))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Account;
    use tempfile::tempdir;

    fn locations(root: &Path, name: &str) -> Locations {
        Locations {
            data: root.join(name).join("share/omarchy-mail"),
            config: root.join(name).join("config/omarchy-mail"),
            cache: root.join(name).join("cache/omarchy-mail"),
        }
    }

    #[test]
    fn stream_round_trips_and_detects_damage() {
        for size in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK + 7] {
            let plain: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut writer = EncryptWriter::new(Vec::new(), "correct horse").unwrap();
            writer.write_all(&plain).unwrap();
            let sealed = writer.finish().unwrap();
            let mut out = Vec::new();
            DecryptReader::new(&sealed[..], "correct horse")
                .unwrap()
                .read_to_end(&mut out)
                .unwrap();
            assert_eq!(out, plain, "size {size}");

            let mut wrong = Vec::new();
            assert!(
                DecryptReader::new(&sealed[..], "wrong horse")
                    .unwrap()
                    .read_to_end(&mut wrong)
                    .is_err()
            );
            let mut tampered = sealed.clone();
            let last = tampered.len() - 1;
            tampered[last] ^= 1;
            assert!(
                DecryptReader::new(&tampered[..], "correct horse")
                    .unwrap()
                    .read_to_end(&mut Vec::new())
                    .is_err()
            );
            if size > CHUNK {
                // Dropping the final chunk must not look like a complete file.
                let truncated = &sealed[..HEADER_LEN + CHUNK + TAG];
                assert!(
                    DecryptReader::new(truncated, "correct horse")
                        .unwrap()
                        .read_to_end(&mut Vec::new())
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn rejects_files_that_are_not_backups() {
        assert!(DecryptReader::new(&b"not a backup at all, definitely not"[..], "x").is_err());
    }

    #[test]
    fn backup_restores_into_a_different_installation() {
        let root = tempdir().unwrap();
        let source = locations(root.path(), "old");
        let database = Database::open(&source.data).unwrap();
        let mut account = Account::new("jim@example.com", "Jim");
        account.id = Some(database.save_account(&account).unwrap());
        let attachment = source.data.join("drafts/draft-1/notes.txt");
        fs::create_dir_all(attachment.parent().unwrap()).unwrap();
        fs::write(&attachment, b"draft attachment").unwrap();
        let cached = source.cache.join("attachments/1/a.pdf");
        let attachments = serde_json::to_string(&serde_json::json!([
            {"filename":"notes.txt","content_type":"text/plain","size":16,"cache_path":attachment,"content_id":null},
            {"filename":"a.pdf","content_type":"application/pdf","size":1,"cache_path":cached,"content_id":null}
        ]))
        .unwrap();
        rusqlite::Connection::open(source.data.join("mail.db"))
            .unwrap()
            .execute(
                "INSERT INTO messages(account_id, folder, sender_name, sender_email, recipients, subject, preview, body, received_at, attachments_json)
                 VALUES (?1, 'Drafts', '', '', '', 'Draft', '', '', '2026-01-01', ?2)",
                rusqlite::params![account.id, attachments],
            )
            .unwrap();
        fs::create_dir_all(&source.config).unwrap();
        fs::write(
            source.config.join("preferences.json"),
            b"{\"conversation_view\":false}",
        )
        .unwrap();

        let file = root.path().join("mail.omailbackup");
        let secrets = vec![Secret {
            account: "jim@example.com".into(),
            slot: "imap".into(),
            value: "s3cret".into(),
        }];
        let manifest = create_backup(&source, &file, "long passphrase", &secrets, true).unwrap();
        assert_eq!(manifest.accounts, vec!["jim@example.com".to_string()]);
        assert!(!fs::read(&file).unwrap().windows(6).any(|w| w == b"s3cret"));

        let target = locations(root.path(), "new");
        Database::open(&target.data).unwrap();
        fs::create_dir_all(&target.config).unwrap();
        fs::write(target.config.join("preferences.json"), b"{}").unwrap();

        assert!(stage_restore(&target, &file, "wrong passphrase").is_err());
        assert!(!target.staging().exists());
        let staged = stage_restore(&target, &file, "long passphrase").unwrap();
        // A later failed attempt leaves the verified restore in place.
        assert!(stage_restore(&target, &file, "wrong passphrase").is_err());
        assert!(target.staging().join("READY").is_file());
        assert_eq!(staged.secrets, secrets);
        assert!(!target.staging().join("secrets.json").exists());
        // Nothing live changes until the next start applies the restore.
        assert!(
            Database::open(&target.data)
                .unwrap()
                .load_accounts()
                .unwrap()
                .is_empty()
        );

        let notice = apply_pending_restore(&target).unwrap().unwrap();
        assert!(notice.contains("1 account"));
        assert!(!target.staging().exists());
        let restored = Database::open(&target.data).unwrap();
        assert_eq!(
            restored.load_accounts().unwrap()[0].email,
            "jim@example.com"
        );
        assert_eq!(
            fs::read(target.data.join("drafts/draft-1/notes.txt")).unwrap(),
            b"draft attachment"
        );
        assert_eq!(
            fs::read(target.config.join("preferences.json")).unwrap(),
            b"{\"conversation_view\":false}"
        );
        let stored: String = rusqlite::Connection::open(target.data.join("mail.db"))
            .unwrap()
            .query_row("SELECT attachments_json FROM messages", [], |r| r.get(0))
            .unwrap();
        assert!(stored.contains(&*target.data.to_string_lossy()));
        assert!(stored.contains(&*target.cache.to_string_lossy()));
        assert!(!stored.contains("/old/"));
        // The previous installation is preserved for recovery.
        let recovery = target.sibling("omarchy-mail-recovery");
        let kept = fs::read_dir(&recovery)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(kept.join("data/mail.db").is_file());
        assert_eq!(
            fs::read(kept.join("config/preferences.json")).unwrap(),
            b"{}"
        );
        assert!(apply_pending_restore(&target).unwrap().is_none());
    }

    #[test]
    fn refuses_short_passphrases_and_unsafe_entries() {
        let root = tempdir().unwrap();
        let source = locations(root.path(), "old");
        Database::open(&source.data).unwrap();
        assert!(create_backup(&source, &root.path().join("x"), "short", &[], false).is_err());
        assert!(!allowed_entry(Path::new("../etc/passwd")));
        assert!(!allowed_entry(Path::new("/etc/passwd")));
        assert!(!allowed_entry(Path::new("config/other.json")));
        assert!(allowed_entry(Path::new("data/drafts/a/b.txt")));
    }
}
