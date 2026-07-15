use bytes::Bytes;
use nexkvm_core::DeviceId;
use nexkvm_streaming::{
    FILE_TRANSFER_WIRE_VERSION, FileTransferMessage, MAX_FILE_TRANSFER_WIRE_BYTES,
    MAX_TRANSFER_MANIFEST_ENTRIES, MAX_TRANSFER_TOTAL_BYTES, TransferCheckpoint, TransferChunk,
    TransferCompression, TransferEntry, TransferError, TransferId, TransferManifest,
    TransferManifestCodec, TransferSource,
};

const WIRE_HEADER_LEN: usize = 4 + 2 + 1 + 1 + 4;
const DATA_DIGEST: [u8; 32] = [0x5a; 32];

fn manifest() -> TransferManifest {
    TransferManifest::new(
        TransferId::generate(),
        DeviceId::generate(),
        None,
        TransferSource::DragDrop,
        vec![
            TransferEntry::dir("folder").unwrap(),
            TransferEntry::file("folder/data.bin", 7, DATA_DIGEST).unwrap(),
        ],
    )
    .unwrap()
}

#[test]
fn manifest_requires_file_digests_and_forbids_directory_digests() {
    let file = TransferEntry::file("data.bin", 7, DATA_DIGEST).unwrap();
    assert_eq!(file.sha256, Some(DATA_DIGEST));
    assert_eq!(TransferEntry::dir("folder").unwrap().sha256, None);

    let mut missing = manifest();
    missing.entries[1].sha256 = None;
    assert!(matches!(
        TransferManifestCodec::encode(&missing),
        Err(TransferError::Codec(_))
    ));

    let mut directory_digest = manifest();
    directory_digest.entries[0].sha256 = Some(DATA_DIGEST);
    assert!(matches!(
        TransferManifestCodec::encode(&directory_digest),
        Err(TransferError::Codec(_))
    ));
}

#[test]
fn legacy_digestless_wire_version_is_rejected_fail_closed() {
    let mut encoded = FileTransferMessage::Offer(manifest())
        .encode()
        .unwrap()
        .to_vec();
    encoded[4..6].copy_from_slice(&1u16.to_be_bytes());

    let error = FileTransferMessage::decode(Bytes::from(encoded)).unwrap_err();
    assert!(error.to_string().contains("lacks required file digests"));
}

#[test]
fn offer_and_manifest_codec_round_trip() {
    let manifest = manifest();

    let encoded_manifest = TransferManifestCodec::encode(&manifest).unwrap();
    let decoded_manifest = TransferManifestCodec::decode(encoded_manifest).unwrap();
    assert_eq!(decoded_manifest, manifest);

    let message = FileTransferMessage::Offer(manifest);
    let encoded = message.encode().unwrap();
    assert_eq!(
        u16::from_be_bytes([encoded[4], encoded[5]]),
        FILE_TRANSFER_WIRE_VERSION
    );
    assert_eq!(FileTransferMessage::decode(encoded).unwrap(), message);
}

#[test]
fn wire_budget_fits_the_authenticated_transport_frame() {
    let configured_limit = std::hint::black_box(MAX_FILE_TRANSFER_WIRE_BYTES);
    assert_eq!(configured_limit + 14 + 6 + 16, 16 * 1024 * 1024);
}

#[test]
fn every_file_transfer_message_round_trips() {
    let id = TransferId::generate();
    let checkpoint = TransferCheckpoint {
        id,
        file_index: 3,
        offset: 1024,
        transferred_bytes: 4096,
    };
    let chunk = TransferChunk {
        transfer_id: id,
        file_index: 3,
        offset: 1024,
        plain_len: 4,
        compression: TransferCompression::None,
        final_chunk_for_file: true,
        payload: Bytes::from_static(b"data"),
    };
    let messages = vec![
        FileTransferMessage::Accept {
            transfer_id: id,
            checkpoint: None,
        },
        FileTransferMessage::Accept {
            transfer_id: id,
            checkpoint: Some(checkpoint),
        },
        FileTransferMessage::Reject {
            transfer_id: id,
            reason: "policy denied".into(),
        },
        FileTransferMessage::Chunk(chunk),
        FileTransferMessage::Checkpoint(checkpoint),
        FileTransferMessage::Ack(checkpoint),
        FileTransferMessage::Complete {
            transfer_id: id,
            transferred_bytes: 4096,
        },
        FileTransferMessage::Cancel {
            transfer_id: id,
            reason: "user canceled".into(),
        },
    ];

    for message in messages {
        let encoded = message.encode().unwrap();
        assert_eq!(FileTransferMessage::decode(encoded).unwrap(), message);
    }
}

#[test]
fn decoder_rejects_version_tag_reserved_bits_and_trailing_bytes() {
    let encoded = FileTransferMessage::Offer(manifest()).encode().unwrap();

    let mut bad_version = encoded.to_vec();
    bad_version[4..6].copy_from_slice(&(FILE_TRANSFER_WIRE_VERSION + 1).to_be_bytes());
    assert!(matches!(
        FileTransferMessage::decode(Bytes::from(bad_version)),
        Err(TransferError::Codec(_))
    ));

    let mut bad_tag = encoded.to_vec();
    bad_tag[6] = u8::MAX;
    assert!(matches!(
        FileTransferMessage::decode(Bytes::from(bad_tag)),
        Err(TransferError::Codec(_))
    ));

    let mut bad_reserved = encoded.to_vec();
    bad_reserved[7] = 1;
    assert!(matches!(
        FileTransferMessage::decode(Bytes::from(bad_reserved)),
        Err(TransferError::Codec(_))
    ));

    let mut trailing = encoded.to_vec();
    trailing.push(0);
    assert!(matches!(
        FileTransferMessage::decode(Bytes::from(trailing)),
        Err(TransferError::Codec(_))
    ));

    let mut body_length_bomb = encoded.to_vec();
    body_length_bomb[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(
        FileTransferMessage::decode(Bytes::from(body_length_bomb)),
        Err(TransferError::TooLarge { .. })
    ));
}

#[test]
fn manifest_decoder_rejects_count_allocation_bomb_before_allocating() {
    let one_file = TransferManifest::new(
        TransferId::generate(),
        DeviceId::generate(),
        None,
        TransferSource::Picker,
        vec![TransferEntry::file("safe", 1, DATA_DIGEST).unwrap()],
    )
    .unwrap();
    let mut encoded = FileTransferMessage::Offer(one_file)
        .encode()
        .unwrap()
        .to_vec();

    // Top-level header + id + from + absent-target + source + total bytes.
    let entry_count_offset = WIRE_HEADER_LEN + 16 + 16 + 1 + 1 + 8;
    encoded[entry_count_offset..entry_count_offset + 4]
        .copy_from_slice(&((MAX_TRANSFER_MANIFEST_ENTRIES as u32) + 1).to_be_bytes());

    assert!(matches!(
        FileTransferMessage::decode(Bytes::from(encoded)),
        Err(TransferError::TooLarge { .. })
    ));
}

#[test]
fn manifest_decoder_rejects_path_traversal_and_declared_path_bomb() {
    let one_file = TransferManifest::new(
        TransferId::generate(),
        DeviceId::generate(),
        None,
        TransferSource::Picker,
        vec![TransferEntry::file("safe", 1, DATA_DIGEST).unwrap()],
    )
    .unwrap();
    let encoded = FileTransferMessage::Offer(one_file).encode().unwrap();
    let entry_offset = WIRE_HEADER_LEN + 16 + 16 + 1 + 1 + 8 + 4;
    let path_len_offset = entry_offset + 1;
    let path_offset = path_len_offset + 2 + 8 + DATA_DIGEST.len();

    let mut traversal = encoded.to_vec();
    traversal[path_offset..path_offset + 4].copy_from_slice(b"../x");
    assert!(matches!(
        FileTransferMessage::decode(Bytes::from(traversal)),
        Err(TransferError::InvalidPath(_))
    ));

    let mut path_bomb = encoded.to_vec();
    path_bomb[path_len_offset..path_len_offset + 2].copy_from_slice(&u16::MAX.to_be_bytes());
    assert!(matches!(
        FileTransferMessage::decode(Bytes::from(path_bomb)),
        Err(TransferError::TooLarge { .. })
    ));
}

#[test]
fn manifest_decoder_rejects_mismatched_total_and_manifest_trailing_bytes() {
    let manifest = manifest();
    let mut encoded = TransferManifestCodec::encode(&manifest).unwrap().to_vec();
    let total_offset = 16 + 16 + 1 + 1;
    encoded[total_offset..total_offset + 8].copy_from_slice(&99u64.to_be_bytes());
    assert!(matches!(
        TransferManifestCodec::decode(Bytes::from(encoded)),
        Err(TransferError::Codec(_))
    ));

    let mut trailing = TransferManifestCodec::encode(&manifest).unwrap().to_vec();
    trailing.push(0);
    assert!(matches!(
        TransferManifestCodec::decode(Bytes::from(trailing)),
        Err(TransferError::Codec(_))
    ));
}

#[test]
fn cross_platform_traversal_spellings_are_rejected_at_manifest_boundary() {
    assert!(matches!(
        TransferEntry::file("..\\secret.txt", 1, DATA_DIGEST),
        Err(TransferError::InvalidPath(_))
    ));
    assert!(matches!(
        TransferEntry::file("C:/Windows/system.ini", 1, DATA_DIGEST),
        Err(TransferError::InvalidPath(_))
    ));
    assert!(matches!(
        TransferEntry::file("folder//file", 1, DATA_DIGEST),
        Err(TransferError::InvalidPath(_))
    ));
}

#[test]
fn manifest_total_limit_and_duplicate_paths_are_rejected() {
    assert!(matches!(
        TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            None,
            TransferSource::Picker,
            vec![
                TransferEntry::file("huge.bin", MAX_TRANSFER_TOTAL_BYTES + 1, DATA_DIGEST,)
                    .unwrap()
            ],
        ),
        Err(TransferError::TooLarge { .. })
    ));

    assert!(matches!(
        TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            None,
            TransferSource::Picker,
            vec![
                TransferEntry::file("same.bin", 1, DATA_DIGEST).unwrap(),
                TransferEntry::file("same.bin", 1, DATA_DIGEST).unwrap(),
            ],
        ),
        Err(TransferError::InvalidPath(_))
    ));

    assert!(matches!(
        TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            None,
            TransferSource::Picker,
            vec![
                TransferEntry::file("Case.bin", 1, DATA_DIGEST).unwrap(),
                TransferEntry::file("case.bin", 1, DATA_DIGEST).unwrap(),
            ],
        ),
        Err(TransferError::InvalidPath(_))
    ));
}

#[test]
fn decoder_rejects_unknown_chunk_flags_and_accept_id_mismatch() {
    let id = TransferId::generate();
    let chunk = TransferChunk {
        transfer_id: id,
        file_index: 0,
        offset: 0,
        plain_len: 1,
        compression: TransferCompression::None,
        final_chunk_for_file: true,
        payload: Bytes::from_static(b"x"),
    };
    let mut encoded = FileTransferMessage::Chunk(chunk).encode().unwrap().to_vec();
    let chunk_flags_offset = WIRE_HEADER_LEN + 16 + 4 + 8 + 4 + 1;
    encoded[chunk_flags_offset] = 0x80;
    assert!(matches!(
        FileTransferMessage::decode(Bytes::from(encoded)),
        Err(TransferError::Codec(_))
    ));

    let checkpoint = TransferCheckpoint {
        id: TransferId::generate(),
        file_index: 0,
        offset: 0,
        transferred_bytes: 0,
    };
    assert!(matches!(
        FileTransferMessage::Accept {
            transfer_id: id,
            checkpoint: Some(checkpoint),
        }
        .encode(),
        Err(TransferError::Codec(_))
    ));

    let impossible_checkpoint = TransferCheckpoint {
        id,
        file_index: 0,
        offset: 10,
        transferred_bytes: 9,
    };
    assert!(matches!(
        FileTransferMessage::Checkpoint(impossible_checkpoint).encode(),
        Err(TransferError::Codec(_))
    ));
}
