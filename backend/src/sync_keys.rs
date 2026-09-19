//! This device's long-term Noise static keypair for sync.
//!
//! Pairing exchanges these keys inside a handshake the pairing code
//! protects, and every later sync connection proves both sides by them (see
//! [`crate::sync_net`]). The private key is the one secret sync keeps: it
//! lives in `sync_static_key` in the config directory, beside `device_id`
//! and for the same reason, so a copied data folder carries no identity. It
//! is created on first need.
//!
//! The file holds the 32-byte X25519 private key as 64 lowercase hex digits
//! and a newline; the public key is derived from it on every load, so the two
//! can never disagree. On Unix the file is created with mode 0600 through a
//! temporary file and a rename, so a crash leaves no key or a whole key.
//!
//! The private key never appears in a log line, an error, or an API
//! response: [`StaticKeypair`] implements `Debug` by hand and prints the
//! fingerprint only, and no error here quotes the file's contents.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use snow::params::{DHChoice, HashChoice};
use snow::resolvers::{CryptoResolver, DefaultResolver};

use crate::config::get_config_dir;
use crate::error::{Error, Result};

const KEY_FILE_NAME: &str = "sync_static_key";

/// Length of an X25519 private or public key, in bytes.
pub const KEY_LEN: usize = 32;

/// Serializes the check and creation of the key file inside this process,
/// so two first uses at once cannot write two different keys.
static KEY_FILE_LOCK: Mutex<()> = Mutex::new(());

/// A Noise static keypair: X25519, as the Noise patterns in
/// [`crate::sync_net`] use it.
pub struct StaticKeypair {
    private: [u8; KEY_LEN],
    public: [u8; KEY_LEN],
}

impl StaticKeypair {
    /// A fresh keypair from the operating system's random generator.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the random generator or the X25519 implementation
    /// is unavailable.
    pub fn generate() -> Result<StaticKeypair> {
        let mut private = [0u8; KEY_LEN];
        fill_random(&mut private)?;
        let keypair = StaticKeypair::from_private(private);
        wipe(&mut private);
        keypair
    }

    /// The keypair whose private key is `private`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the X25519 implementation is unavailable.
    pub fn from_private(private: [u8; KEY_LEN]) -> Result<StaticKeypair> {
        let mut dh = DefaultResolver
            .resolve_dh(&DHChoice::Curve25519)
            .ok_or_else(|| crypto_unavailable("X25519"))?;
        dh.set(&private);
        let mut public = [0u8; KEY_LEN];
        public.copy_from_slice(dh.pubkey());
        Ok(StaticKeypair { private, public })
    }

    /// The public key, which is safe to send and to store on a peer.
    pub fn public(&self) -> &[u8; KEY_LEN] {
        &self.public
    }

    /// The private key, for the Noise handshake only.
    pub(crate) fn private(&self) -> &[u8; KEY_LEN] {
        &self.private
    }

    /// The fingerprint of the public key; see [`fingerprint`].
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }
}

impl fmt::Debug for StaticKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StaticKeypair {{ fingerprint: {} }}", self.fingerprint())
    }
}

impl Drop for StaticKeypair {
    fn drop(&mut self) {
        wipe(&mut self.private);
    }
}

/// Overwrite `bytes` with zeros in a way the compiler does not remove as a
/// dead store, so a freed key does not linger in memory the allocator hands
/// out again.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a valid, aligned, exclusive reference.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

/// Fill `bytes` from the operating system's random generator, through the
/// generator snow itself uses for its keys.
///
/// # Errors
///
/// [`Error::Io`] when the generator is unavailable or fails.
pub(crate) fn fill_random(bytes: &mut [u8]) -> Result<()> {
    let mut rng = DefaultResolver
        .resolve_rng()
        .ok_or_else(|| crypto_unavailable("the random generator"))?;
    rng.try_fill_bytes(bytes)
        .map_err(|_| Error::Io(std::io::Error::other("the random generator failed")))
}

fn crypto_unavailable(what: &str) -> Error {
    Error::Io(std::io::Error::other(format!(
        "the sync crypto has no implementation of {what}"
    )))
}

/// A short, readable fingerprint of a public key: the first 8 bytes of its
/// BLAKE2s hash, as four groups of four hex digits, such as
/// `1a2b-3c4d-5e6f-7a8b`. Two devices show the same fingerprint for the same
/// key, so the user can compare them by eye. It is for display only; nothing
/// authenticates by it.
pub fn fingerprint(public: &[u8]) -> String {
    let Some(mut hash) = DefaultResolver.resolve_hash(&HashChoice::Blake2s) else {
        return "unavailable".to_string();
    };
    hash.input(public);
    let mut digest = [0u8; 32];
    hash.result(&mut digest);
    let hex = to_hex(&digest[..8]);
    [&hex[0..4], &hex[4..8], &hex[8..12], &hex[12..16]].join("-")
}

/// `bytes` as lowercase hex digits.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// A key of [`KEY_LEN`] bytes from 64 hex digits, either case. `None` for
/// any other text, so a caller builds an error that does not quote it.
pub fn key_from_hex(text: &str) -> Option<[u8; KEY_LEN]> {
    let text = text.as_bytes();
    if text.len() != KEY_LEN * 2 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut key = [0u8; KEY_LEN];
    for (index, pair) in text.chunks(2).enumerate() {
        key[index] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(key)
}

/// This device's keypair, created and stored on the first call.
///
/// # Errors
///
/// [`Error::SyncRefused`] when the key file exists and is not a key: the
/// message names the file, says to remove it and pair again, and quotes
/// nothing. Replacing it silently would break every pairing without a word.
/// [`Error::Io`] when the file cannot be read, written, flushed, or renamed.
pub fn local_keypair() -> Result<StaticKeypair> {
    let _guard = KEY_FILE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = get_config_dir()?.join(KEY_FILE_NAME);
    if let Ok(metadata) = std::fs::symlink_metadata(&path)
        && metadata.file_type().is_symlink()
    {
        return Err(Error::SyncRefused(format!(
            "the sync key path {} is a symlink; replace it with a regular key file",
            path.display()
        )));
    }
    match std::fs::read(&path) {
        Ok(bytes) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let metadata = std::fs::metadata(&path)?;
                if metadata.permissions().mode() & 0o077 != 0 {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                }
            }
            let text = String::from_utf8_lossy(&bytes);
            let Some(mut private) = key_from_hex(text.trim()) else {
                return Err(Error::SyncRefused(format!(
                    "the sync key file {KEY_FILE_NAME} in this device's config folder is not a \
                     key. Remove that file and pair this device again with every device it \
                     syncs with."
                )));
            };
            let keypair = StaticKeypair::from_private(private);
            wipe(&mut private);
            keypair
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let keypair = StaticKeypair::generate()?;
            write_key_file(&path, &keypair)?;
            Ok(keypair)
        }
        Err(err) => Err(err.into()),
    }
}

/// Write `keypair`'s private key to `path` through a temporary file created
/// with mode 0600 on Unix, a flush, and a rename, then flush the folder.
fn write_key_file(path: &Path, keypair: &StaticKeypair) -> Result<()> {
    let temp_path = path.with_file_name(format!(".{KEY_FILE_NAME}.tmp"));
    // A leftover temporary file keeps its old mode when opened again, so it
    // goes first and the file below is always created fresh with 0600.
    match std::fs::remove_file(&temp_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let written = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path)?;
        let mut text = to_hex(keypair.private()).into_bytes();
        text.push(b'\n');
        let result = file.write_all(&text);
        wipe(&mut text);
        result?;
        file.sync_all()?;
        std::fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if written.is_err() {
        // The write error is the one to report.
        let _ = std::fs::remove_file(&temp_path);
    }
    written?;
    if let Some(folder) = path.parent() {
        crate::df_operations::sync_dir(folder)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` at a fresh temp
    /// dir, so no test reads or writes the real config.
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is not thread-safe; callers hold
    /// `#[serial_test::serial]`.
    fn with_temp_env() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            std::env::set_var("HOME", dir.path());
        }
        dir
    }

    /// The first call creates the key file and later calls return the same
    /// key; on Unix the file is private to the account.
    #[test]
    #[serial_test::serial]
    fn the_keypair_is_created_once_and_kept() {
        let _temp = with_temp_env();
        let first = local_keypair().unwrap();
        let second = local_keypair().unwrap();
        assert_eq!(first.public(), second.public());
        assert_eq!(first.private(), second.private());

        let path = get_config_dir().unwrap().join(KEY_FILE_NAME);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(!path.with_file_name(".sync_static_key.tmp").exists());
    }

    /// The public key is derived from the private one, so loading a stored
    /// private key gives the same public key it had when generated.
    #[test]
    fn the_public_key_follows_from_the_private_key() {
        let generated = StaticKeypair::generate().unwrap();
        let loaded = StaticKeypair::from_private(*generated.private()).unwrap();
        assert_eq!(generated.public(), loaded.public());
        assert_ne!(generated.public(), &[0u8; KEY_LEN]);
    }

    /// `Debug` shows the fingerprint and never the key bytes.
    #[test]
    fn debug_shows_no_key() {
        let keypair = StaticKeypair::from_private([7u8; KEY_LEN]).unwrap();
        let shown = format!("{keypair:?}");
        assert!(shown.contains(&keypair.fingerprint()), "{shown}");
        assert!(!shown.contains(&to_hex(keypair.private())));
        assert!(!shown.contains(&to_hex(keypair.public())));
        assert_eq!(keypair.fingerprint().len(), 19);
    }

    /// A key file that is not a key is refused with a message that quotes
    /// nothing, and is not replaced.
    #[test]
    #[serial_test::serial]
    fn a_damaged_key_file_is_refused_and_kept() {
        let _temp = with_temp_env();
        let path = get_config_dir().unwrap().join(KEY_FILE_NAME);
        std::fs::write(&path, b"secret-looking-text").unwrap();
        let err = local_keypair().expect_err("refused");
        assert!(matches!(err, Error::SyncRefused(_)), "{err}");
        assert!(!err.to_string().contains("secret-looking-text"));
        assert_eq!(std::fs::read(&path).unwrap(), b"secret-looking-text");
    }

    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn an_existing_key_file_is_made_private() {
        use std::os::unix::fs::PermissionsExt;

        let _temp = with_temp_env();
        let first = local_keypair().unwrap();
        let path = get_config_dir().unwrap().join(KEY_FILE_NAME);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let loaded = local_keypair().unwrap();
        assert_eq!(first.public(), loaded.public());
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn a_symlink_key_path_is_refused() {
        use std::os::unix::fs::symlink;

        let _temp = with_temp_env();
        let target = get_config_dir().unwrap().join("target-key");
        std::fs::write(&target, b"target contents").unwrap();
        let path = get_config_dir().unwrap().join(KEY_FILE_NAME);
        symlink(&target, &path).unwrap();

        let err = local_keypair().expect_err("symlink refused");
        assert!(matches!(err, Error::SyncRefused(_)), "{err}");
        assert!(err.to_string().contains(KEY_FILE_NAME), "{err}");
        assert_eq!(std::fs::read(target).unwrap(), b"target contents");
    }

    #[test]
    fn hex_round_trips() {
        let key = [0xabu8; KEY_LEN];
        assert_eq!(key_from_hex(&to_hex(&key)), Some(key));
        assert_eq!(key_from_hex(&to_hex(&key).to_uppercase()), Some(key));
        assert_eq!(key_from_hex("abc"), None);
        assert_eq!(key_from_hex(&"zz".repeat(KEY_LEN)), None);
    }
}
