use nexkvm_clipboard::{ClipboardContent, ClipboardSnapshot};
use nexkvm_core::DeviceId;
use nexkvm_storage::{
    ClipboardHistoryArchive, ClipboardHistoryArchiveConfig, ClipboardHistoryStoreError,
};

fn archive(path: &std::path::Path) -> ClipboardHistoryArchive {
    ClipboardHistoryArchive::open(
        path,
        ClipboardHistoryArchiveConfig {
            capacity: 2,
            max_entry_bytes: 1_024,
            max_archive_bytes: 4_096,
        },
    )
    .expect("open history archive")
}

#[test]
fn encrypted_archive_round_trips_without_plaintext_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clipboard-history.enc");
    let mut history = archive(&path);
    let origin = DeviceId::generate();

    assert!(history.record(
        ClipboardSnapshot::from_text("private clipboard text"),
        origin,
        7
    ));
    history.persist().unwrap();

    let ciphertext = std::fs::read(&path).unwrap();
    assert!(
        !ciphertext
            .windows(b"private clipboard text".len())
            .any(|window| window == b"private clipboard text")
    );

    let loaded = archive(&path);
    let entry = loaded.entries().next().unwrap();
    assert_eq!(entry.snapshot.best_text(), Some("private clipboard text"));
    assert_eq!(entry.origin, origin);
    assert_eq!(entry.at_millis, 7);
}

#[test]
fn archive_deduplicates_caps_and_never_records_concealed_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clipboard-history.enc");
    let mut history = archive(&path);
    let origin = DeviceId::generate();

    assert!(history.record(ClipboardSnapshot::from_text("one"), origin, 1));
    assert!(history.record(ClipboardSnapshot::from_text("two"), origin, 2));
    assert!(history.record(ClipboardSnapshot::from_text("one"), origin, 3));
    assert!(history.record(ClipboardSnapshot::from_text("three"), origin, 4));
    let concealed = ClipboardSnapshot::new(vec![ClipboardContent {
        mime: "x-kde-passwordManagerHint".into(),
        data: bytes::Bytes::from_static(b"never-store-this"),
    }]);
    assert!(!history.record(concealed, origin, 5));

    let texts: Vec<_> = history
        .entries()
        .filter_map(|entry| entry.snapshot.best_text())
        .collect();
    assert_eq!(texts, vec!["three", "one"]);
}

#[test]
fn exclusive_archive_serializes_read_modify_write_transactions() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clipboard-history.enc");
    let config = ClipboardHistoryArchiveConfig::default();
    let first = ClipboardHistoryArchive::open_exclusive(&path, config).unwrap();
    let second_path = path.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (opened_tx, opened_rx) = mpsc::channel();

    let contender = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let archive = ClipboardHistoryArchive::open_exclusive(second_path, config).unwrap();
        opened_tx.send(()).unwrap();
        archive
    });

    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        opened_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "a second writer must wait while the first transaction is alive"
    );

    drop(first);
    opened_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(contender.join().unwrap());
}

#[test]
fn tampering_is_rejected_instead_of_returning_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clipboard-history.enc");
    let mut history = archive(&path);
    history.record(
        ClipboardSnapshot::from_text("authenticated"),
        DeviceId::generate(),
        1,
    );
    history.persist().unwrap();

    let mut bytes = std::fs::read(&path).unwrap();
    *bytes.last_mut().unwrap() ^= 0x80;
    std::fs::write(&path, bytes).unwrap();

    assert!(matches!(
        ClipboardHistoryArchive::open(&path, ClipboardHistoryArchiveConfig::default()),
        Err(ClipboardHistoryStoreError::Authentication)
    ));
}

#[test]
fn oversized_encrypted_archive_is_rejected_before_decryption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clipboard-history.enc");
    let config = ClipboardHistoryArchiveConfig {
        capacity: 2,
        max_entry_bytes: 32,
        max_archive_bytes: 64,
    };
    drop(ClipboardHistoryArchive::open(&path, config).unwrap());
    // 8-byte magic + 12-byte nonce + plaintext limit + 16-byte AEAD tag.
    std::fs::write(&path, vec![0u8; 8 + 12 + 64 + 16 + 1]).unwrap();

    let error = ClipboardHistoryArchive::open(&path, config)
        .expect_err("oversized encrypted history must fail");

    assert!(matches!(error, ClipboardHistoryStoreError::Codec(_)));
}

#[cfg(unix)]
#[test]
fn key_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clipboard-history.enc");
    let history = archive(&path);
    let mode = std::fs::metadata(history.key_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o600);
}

#[cfg(unix)]
#[test]
fn archive_and_key_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, b"do not follow").unwrap();
    let archive_path = dir.path().join("clipboard-history.enc");
    symlink(&target, &archive_path).unwrap();
    assert!(
        ClipboardHistoryArchive::open(&archive_path, ClipboardHistoryArchiveConfig::default())
            .is_err()
    );

    std::fs::remove_file(&archive_path).unwrap();
    let key_path = archive_path.with_extension("key");
    symlink(&target, &key_path).unwrap();
    assert!(
        ClipboardHistoryArchive::open(&archive_path, ClipboardHistoryArchiveConfig::default())
            .is_err()
    );

    std::fs::remove_file(&key_path).unwrap();
    let lock_path = archive_path.with_extension("lock");
    symlink(&target, &lock_path).unwrap();
    assert!(
        ClipboardHistoryArchive::open_exclusive(
            &archive_path,
            ClipboardHistoryArchiveConfig::default()
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"do not follow");
}
