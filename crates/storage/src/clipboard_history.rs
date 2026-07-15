//! Authenticated, bounded clipboard-history persistence.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use fs2::FileExt;
use nexkvm_clipboard::{ClipboardSnapshot, ContentFingerprint};
use nexkvm_core::DeviceId;
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

const MAGIC: &[u8; 8] = b"NXCLPH01";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + NONCE_LEN;
const ENTRY_FIXED_LEN: usize = 16 + 8 + 1 + 4;

/// Limits applied to the local encrypted clipboard archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardHistoryArchiveConfig {
    /// Maximum retained selections.
    pub capacity: usize,
    /// Maximum compact snapshot size for one selection.
    pub max_entry_bytes: usize,
    /// Maximum plaintext archive size.
    pub max_archive_bytes: usize,
}

impl Default for ClipboardHistoryArchiveConfig {
    fn default() -> Self {
        Self {
            capacity: 50,
            max_entry_bytes: 2 * 1024 * 1024,
            max_archive_bytes: 32 * 1024 * 1024,
        }
    }
}

/// One clipboard selection retained across daemon restarts.
#[derive(Clone, PartialEq, Eq)]
pub struct ArchivedClipboardEntry {
    /// Multi-format clipboard content.
    pub snapshot: ClipboardSnapshot,
    /// Device that originated the selection.
    pub origin: DeviceId,
    /// Wall-clock timestamp in milliseconds.
    pub at_millis: u64,
    /// Whether capacity pruning must preserve this entry.
    pub pinned: bool,
}

impl std::fmt::Debug for ArchivedClipboardEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArchivedClipboardEntry")
            .field("fingerprint", &self.fingerprint())
            .field("origin", &self.origin)
            .field("at_millis", &self.at_millis)
            .field("pinned", &self.pinned)
            .field("bytes", &self.snapshot.total_len())
            .field("formats", &self.snapshot.formats().len())
            .finish()
    }
}

impl ArchivedClipboardEntry {
    /// Content fingerprint used for deduplication and restore selection.
    #[must_use]
    pub fn fingerprint(&self) -> ContentFingerprint {
        self.snapshot.fingerprint()
    }
}

/// Clipboard-history persistence failures.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClipboardHistoryStoreError {
    /// Filesystem access failed.
    #[error("clipboard history I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The encrypted file was malformed or exceeded configured bounds.
    #[error("clipboard history codec error: {0}")]
    Codec(String),
    /// Ciphertext authentication failed.
    #[error("clipboard history authentication failed")]
    Authentication,
    /// Random-key or nonce generation failed.
    #[error("clipboard history random source failed: {0}")]
    Random(String),
    /// Encryption failed.
    #[error("clipboard history encryption failed")]
    Encryption,
    /// Archive limits were invalid.
    #[error("invalid clipboard history configuration: {0}")]
    InvalidConfig(&'static str),
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
struct ArchiveKey([u8; 32]);

/// In-memory view backed by an authenticated encrypted file.
pub struct ClipboardHistoryArchive {
    path: PathBuf,
    key_path: PathBuf,
    key: ArchiveKey,
    config: ClipboardHistoryArchiveConfig,
    entries: VecDeque<ArchivedClipboardEntry>,
    // Closing this file releases the cross-process advisory lock. Ordinary
    // read-only opens leave it as `None`; read-modify-write callers use
    // `open_exclusive` and retain the lock through `persist`.
    _exclusive_lock: Option<File>,
}

impl std::fmt::Debug for ClipboardHistoryArchive {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardHistoryArchive")
            .field("path", &self.path)
            .field("key_path", &self.key_path)
            .field("config", &self.config)
            .field("entry_count", &self.entries.len())
            .field("exclusively_locked", &self._exclusive_lock.is_some())
            .finish_non_exhaustive()
    }
}

impl ClipboardHistoryArchive {
    /// Open an archive, creating its owner-only key when needed.
    ///
    /// # Errors
    /// Returns an error for invalid limits, inaccessible files, malformed data,
    /// or ciphertext that does not authenticate.
    pub fn open(
        path: impl AsRef<Path>,
        config: ClipboardHistoryArchiveConfig,
    ) -> Result<Self, ClipboardHistoryStoreError> {
        Self::open_inner(path.as_ref(), config, None)
    }

    /// Open the latest on-disk archive while holding an exclusive
    /// cross-process lock for this value's lifetime.
    ///
    /// Use this constructor for every read-modify-write operation. Keeping the
    /// returned archive alive through [`Self::persist`] prevents daemon and CLI
    /// processes from overwriting each other's changes or resurrecting an old
    /// in-memory snapshot.
    ///
    /// # Errors
    /// Returns an error when the lock, key, or encrypted archive cannot be
    /// opened safely, or when archive validation/decryption fails.
    pub fn open_exclusive(
        path: impl AsRef<Path>,
        config: ClipboardHistoryArchiveConfig,
    ) -> Result<Self, ClipboardHistoryStoreError> {
        validate_config(config)?;
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock = open_lock_file(&lock_path_for(&path))?;
        FileExt::lock_exclusive(&lock)?;
        Self::open_inner(&path, config, Some(lock))
    }

    fn open_inner(
        path: &Path,
        config: ClipboardHistoryArchiveConfig,
        exclusive_lock: Option<File>,
    ) -> Result<Self, ClipboardHistoryStoreError> {
        validate_config(config)?;
        let path = path.to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let archive_exists = regular_file_exists(&path)?;
        let key_path = key_path_for(&path);
        let key = load_or_create_key(&key_path, archive_exists)?;
        let entries = if archive_exists {
            decrypt_entries(&path, &key, config)?
        } else {
            VecDeque::new()
        };
        let mut archive = Self {
            path,
            key_path,
            key,
            config,
            entries,
            _exclusive_lock: exclusive_lock,
        };
        archive.prune_to_limits();
        Ok(archive)
    }

    /// Encrypted archive path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Owner-only key path.
    #[must_use]
    pub fn key_path(&self) -> &Path {
        &self.key_path
    }

    /// Entries in most-recent-first order.
    pub fn entries(&self) -> impl Iterator<Item = &ArchivedClipboardEntry> {
        self.entries.iter()
    }

    /// Record a non-empty, non-concealed selection within configured limits.
    /// Returns whether the selection was retained.
    pub fn record(
        &mut self,
        snapshot: ClipboardSnapshot,
        origin: DeviceId,
        at_millis: u64,
    ) -> bool {
        if snapshot.is_empty() || snapshot.is_concealed() {
            return false;
        }
        let Ok(encoded) = snapshot.encode() else {
            return false;
        };
        if encoded.len() > self.config.max_entry_bytes {
            return false;
        }

        let fingerprint = snapshot.fingerprint();
        let pinned = self
            .entries
            .iter()
            .position(|entry| entry.fingerprint() == fingerprint)
            .and_then(|position| self.entries.remove(position))
            .is_some_and(|entry| entry.pinned);
        self.entries.push_front(ArchivedClipboardEntry {
            snapshot,
            origin,
            at_millis,
            pinned,
        });
        self.prune_to_limits();
        self.entries
            .iter()
            .any(|entry| entry.fingerprint() == fingerprint)
    }

    /// Set the pinned state for an entry selected by fingerprint.
    pub fn set_pinned(&mut self, fingerprint: ContentFingerprint, pinned: bool) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.fingerprint() == fingerprint)
        else {
            return false;
        };
        entry.pinned = pinned;
        true
    }

    /// Remove all unpinned entries.
    pub fn clear_unpinned(&mut self) {
        self.entries.retain(|entry| entry.pinned);
    }

    /// Atomically encrypt and persist the current archive.
    ///
    /// # Errors
    /// Returns an error if serialization, random generation, encryption, or
    /// filesystem persistence fails.
    pub fn persist(&self) -> Result<(), ClipboardHistoryStoreError> {
        let plaintext = encode_entries(&self.entries, self.config.max_archive_bytes)?;
        let cipher = ChaCha20Poly1305::new_from_slice(&self.key.0)
            .map_err(|_| ClipboardHistoryStoreError::Encryption)?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce)
            .map_err(|error| ClipboardHistoryStoreError::Random(error.to_string()))?;
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext,
                    aad: MAGIC,
                },
            )
            .map_err(|_| ClipboardHistoryStoreError::Encryption)?;

        let mut contents = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        contents.extend_from_slice(MAGIC);
        contents.extend_from_slice(&nonce);
        contents.extend_from_slice(&ciphertext);
        atomic_write_owner_only(&self.path, &contents)
    }

    fn prune_to_limits(&mut self) {
        loop {
            let encoded_len = encoded_entries_len(&self.entries);
            if self.entries.len() <= self.config.capacity
                && encoded_len <= self.config.max_archive_bytes
            {
                break;
            }
            let victim = self.entries.iter().rposition(|entry| !entry.pinned);
            let Some(victim) = victim else {
                break;
            };
            self.entries.remove(victim);
        }
    }
}

fn validate_config(
    config: ClipboardHistoryArchiveConfig,
) -> Result<(), ClipboardHistoryStoreError> {
    if config.capacity == 0 {
        return Err(ClipboardHistoryStoreError::InvalidConfig(
            "capacity must be positive",
        ));
    }
    if config.max_entry_bytes == 0 {
        return Err(ClipboardHistoryStoreError::InvalidConfig(
            "max_entry_bytes must be positive",
        ));
    }
    if config.max_archive_bytes < 4 {
        return Err(ClipboardHistoryStoreError::InvalidConfig(
            "max_archive_bytes is too small",
        ));
    }
    Ok(())
}

fn key_path_for(path: &Path) -> PathBuf {
    path.with_extension("key")
}

fn lock_path_for(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

fn open_lock_file(path: &Path) -> Result<File, ClipboardHistoryStoreError> {
    loop {
        if regular_file_exists(path)? {
            secure_existing_file(path)?;
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            if !file.metadata()?.is_file() {
                return Err(ClipboardHistoryStoreError::Codec(
                    "history lock path must be a regular file".into(),
                ));
            }
            return Ok(file);
        }

        match open_new_owner_only(path) {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn load_or_create_key(
    path: &Path,
    archive_exists: bool,
) -> Result<ArchiveKey, ClipboardHistoryStoreError> {
    let key_exists = regular_file_exists(path)?;
    if archive_exists && !key_exists {
        return Err(ClipboardHistoryStoreError::Codec(
            "encrypted history exists without its key".into(),
        ));
    }
    if !key_exists {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key)
            .map_err(|error| ClipboardHistoryStoreError::Random(error.to_string()))?;
        create_owner_only(path, &key)?;
        return Ok(ArchiveKey(key));
    }
    match File::open(path) {
        Ok(mut file) => {
            let mut key = [0u8; 32];
            file.read_exact(&mut key)?;
            let mut trailing = [0u8; 1];
            if file.read(&mut trailing)? != 0 {
                key.zeroize();
                return Err(ClipboardHistoryStoreError::Codec(
                    "history key has an invalid length".into(),
                ));
            }
            secure_existing_file(path)?;
            Ok(ArchiveKey(key))
        }
        Err(error) => Err(error.into()),
    }
}

fn regular_file_exists(path: &Path) -> Result<bool, ClipboardHistoryStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(
            ClipboardHistoryStoreError::Codec("history path must not be a symlink".into()),
        ),
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(ClipboardHistoryStoreError::Codec(
            "history path must be a regular file".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn decrypt_entries(
    path: &Path,
    key: &ArchiveKey,
    config: ClipboardHistoryArchiveConfig,
) -> Result<VecDeque<ArchivedClipboardEntry>, ClipboardHistoryStoreError> {
    let maximum = HEADER_LEN
        .saturating_add(config.max_archive_bytes)
        .saturating_add(TAG_LEN) as u64;
    let contents = match crate::bounded_file::read_owner_only_bounded_regular_file(path, maximum) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Err(ClipboardHistoryStoreError::Codec(
                "encrypted archive exceeds configured limit".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if contents.len() < HEADER_LEN + TAG_LEN || &contents[..MAGIC.len()] != MAGIC {
        return Err(ClipboardHistoryStoreError::Codec(
            "invalid encrypted archive header".into(),
        ));
    }
    let nonce = &contents[MAGIC.len()..HEADER_LEN];
    let cipher = ChaCha20Poly1305::new_from_slice(&key.0)
        .map_err(|_| ClipboardHistoryStoreError::Authentication)?;
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: &contents[HEADER_LEN..],
                aad: MAGIC,
            },
        )
        .map_err(|_| ClipboardHistoryStoreError::Authentication)?;
    decode_entries(&plaintext, config)
}

fn encode_entries(
    entries: &VecDeque<ArchivedClipboardEntry>,
    maximum: usize,
) -> Result<Vec<u8>, ClipboardHistoryStoreError> {
    let capacity = encoded_entries_len(entries);
    if capacity > maximum {
        return Err(ClipboardHistoryStoreError::Codec(
            "plaintext archive exceeds configured limit".into(),
        ));
    }
    let count = u32::try_from(entries.len())
        .map_err(|_| ClipboardHistoryStoreError::Codec("too many history entries".into()))?;
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(&count.to_be_bytes());
    for entry in entries {
        let snapshot = entry
            .snapshot
            .encode()
            .map_err(|error| ClipboardHistoryStoreError::Codec(error.to_string()))?;
        let snapshot_len = u32::try_from(snapshot.len()).map_err(|_| {
            ClipboardHistoryStoreError::Codec("history entry length exceeds u32".into())
        })?;
        output.extend_from_slice(entry.origin.0.as_bytes());
        output.extend_from_slice(&entry.at_millis.to_be_bytes());
        output.push(u8::from(entry.pinned));
        output.extend_from_slice(&snapshot_len.to_be_bytes());
        output.extend_from_slice(&snapshot);
    }
    Ok(output)
}

fn decode_entries(
    plaintext: &[u8],
    config: ClipboardHistoryArchiveConfig,
) -> Result<VecDeque<ArchivedClipboardEntry>, ClipboardHistoryStoreError> {
    let mut cursor = Cursor::new(plaintext);
    let count = cursor.read_u32()? as usize;
    if count > config.capacity.saturating_mul(4).max(config.capacity) {
        return Err(ClipboardHistoryStoreError::Codec(
            "history entry count exceeds safety limit".into(),
        ));
    }
    let mut entries = VecDeque::with_capacity(count.min(config.capacity));
    for _ in 0..count {
        let origin = DeviceId(uuid::Uuid::from_bytes(cursor.read_array()?));
        let at_millis = cursor.read_u64()?;
        let pinned = match cursor.read_u8()? {
            0 => false,
            1 => true,
            _ => {
                return Err(ClipboardHistoryStoreError::Codec(
                    "invalid pinned flag".into(),
                ));
            }
        };
        let snapshot_len = cursor.read_u32()? as usize;
        if snapshot_len > config.max_entry_bytes {
            return Err(ClipboardHistoryStoreError::Codec(
                "history entry exceeds configured limit".into(),
            ));
        }
        let snapshot = ClipboardSnapshot::decode(bytes::Bytes::copy_from_slice(
            cursor.read_exact(snapshot_len)?,
        ))
        .map_err(|error| ClipboardHistoryStoreError::Codec(error.to_string()))?;
        if snapshot.is_empty() || snapshot.is_concealed() {
            return Err(ClipboardHistoryStoreError::Codec(
                "archive contains a forbidden clipboard entry".into(),
            ));
        }
        entries.push_back(ArchivedClipboardEntry {
            snapshot,
            origin,
            at_millis,
            pinned,
        });
    }
    if !cursor.is_empty() {
        return Err(ClipboardHistoryStoreError::Codec(
            "trailing clipboard history bytes".into(),
        ));
    }
    Ok(entries)
}

fn encoded_entries_len(entries: &VecDeque<ArchivedClipboardEntry>) -> usize {
    4usize.saturating_add(entries.iter().fold(0usize, |total, entry| {
        total.saturating_add(ENTRY_FIXED_LEN).saturating_add(
            entry
                .snapshot
                .encode()
                .map_or(usize::MAX, |bytes| bytes.len()),
        )
    }))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read_exact(&mut self, length: usize) -> Result<&'a [u8], ClipboardHistoryStoreError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| ClipboardHistoryStoreError::Codec("history length overflow".into()))?;
        let output = self.bytes.get(self.position..end).ok_or_else(|| {
            ClipboardHistoryStoreError::Codec("truncated clipboard history".into())
        })?;
        self.position = end;
        Ok(output)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], ClipboardHistoryStoreError> {
        self.read_exact(N)?
            .try_into()
            .map_err(|_| ClipboardHistoryStoreError::Codec("invalid fixed field".into()))
    }

    fn read_u8(&mut self) -> Result<u8, ClipboardHistoryStoreError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, ClipboardHistoryStoreError> {
        Ok(u32::from_be_bytes(self.read_array()?))
    }

    fn read_u64(&mut self) -> Result<u64, ClipboardHistoryStoreError> {
        Ok(u64::from_be_bytes(self.read_array()?))
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
}

fn atomic_write_owner_only(path: &Path, contents: &[u8]) -> Result<(), ClipboardHistoryStoreError> {
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix)
        .map_err(|error| ClipboardHistoryStoreError::Random(error.to_string()))?;
    let temp_path = path.with_extension(format!("tmp-{}", hex(&suffix)));
    let result = (|| {
        let mut file = open_new_owner_only(&temp_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
        secure_existing_file(path)?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn create_owner_only(path: &Path, contents: &[u8]) -> Result<(), ClipboardHistoryStoreError> {
    let mut file = open_new_owner_only(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    secure_existing_file(path)
}

fn open_new_owner_only(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn secure_existing_file(path: &Path) -> Result<(), ClipboardHistoryStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}
