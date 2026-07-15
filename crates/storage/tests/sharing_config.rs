use nexkvm_storage::Config;

#[test]
fn clipboard_history_has_privacy_preserving_bounded_defaults() {
    let config = Config::default();

    assert!(!config.clipboard.history_enabled);
    assert_eq!(config.clipboard.history_capacity, 50);
    assert_eq!(config.clipboard.history_max_entry_bytes, 2 * 1024 * 1024);
    assert!(!config.clipboard.sync_enabled);
}

#[test]
fn file_transfer_is_opt_in_and_round_trips() {
    let text = r#"
[file_transfer]
enabled = true
download_dir = "/tmp/NexKVM Downloads"
max_transfer_bytes = 1048576
"#;

    let config: Config = toml::from_str(text).unwrap();
    assert!(config.file_transfer.enabled);
    assert_eq!(
        config.file_transfer.download_dir.as_deref(),
        Some("/tmp/NexKVM Downloads")
    );
    assert_eq!(config.file_transfer.max_transfer_bytes, 1_048_576);

    let rendered = toml::to_string(&config).unwrap();
    assert!(rendered.contains("[file_transfer]"));
}
