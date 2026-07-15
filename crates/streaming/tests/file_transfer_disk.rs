use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use nexkvm_streaming::{
    DecodedChunk, TransferError, TransferFileReader, TransferId, TransferPartWriter,
    create_transfer_directory,
};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("nexkvm-streaming-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn decoded(file_index: u32, offset: u64, bytes: &'static [u8], final_chunk: bool) -> DecodedChunk {
    DecodedChunk {
        file_index,
        offset,
        bytes: Bytes::from_static(bytes),
        final_chunk_for_file: final_chunk,
    }
}

struct PartiallyFailingReader {
    inner: Cursor<Vec<u8>>,
    fail_once: bool,
}

impl Read for PartiallyFailingReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.fail_once && self.inner.position() >= 2 {
            self.fail_once = false;
            return Err(io::Error::other("injected read failure"));
        }
        let max = if self.fail_once {
            output.len().min(2)
        } else {
            output.len()
        };
        self.inner.read(&mut output[..max])
    }
}

impl Seek for PartiallyFailingReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

#[test]
fn seek_reader_streams_bounded_raw_chunks_without_whole_file_buffer() {
    let bytes = b"abcdefghij".to_vec();
    let mut reader =
        TransferFileReader::new(Cursor::new(bytes), TransferId::generate(), 4, 10, 4).unwrap();

    let first = reader.next_chunk().unwrap().unwrap();
    let second = reader.next_chunk().unwrap().unwrap();
    let third = reader.next_chunk().unwrap().unwrap();
    assert_eq!(&first.payload[..], b"abcd");
    assert_eq!(&second.payload[..], b"efgh");
    assert_eq!(&third.payload[..], b"ij");
    assert_eq!((first.offset, second.offset, third.offset), (0, 4, 8));
    assert!(!first.final_chunk_for_file);
    assert!(third.final_chunk_for_file);
    assert!(reader.next_chunk().unwrap().is_none());
}

#[test]
fn seek_reader_resumes_at_offset_and_emits_empty_file_marker() {
    let id = TransferId::generate();
    let mut resumed =
        TransferFileReader::resume(Cursor::new(b"abcdef".to_vec()), id, 2, 6, 3, 3).unwrap();
    let chunk = resumed.next_chunk().unwrap().unwrap();
    assert_eq!(chunk.offset, 3);
    assert_eq!(&chunk.payload[..], b"def");
    assert!(chunk.final_chunk_for_file);

    let mut empty = TransferFileReader::new(Cursor::new(Vec::new()), id, 3, 0, 1024).unwrap();
    let empty_chunk = empty.next_chunk().unwrap().unwrap();
    assert_eq!(empty_chunk.plain_len, 0);
    assert!(empty_chunk.payload.is_empty());
    assert!(empty_chunk.final_chunk_for_file);
    assert!(empty.next_chunk().unwrap().is_none());
}

#[test]
fn seek_reader_rolls_back_underlying_position_after_partial_read_failure() {
    let source = PartiallyFailingReader {
        inner: Cursor::new(b"abcdef".to_vec()),
        fail_once: true,
    };
    let mut reader = TransferFileReader::new(source, TransferId::generate(), 0, 6, 4).unwrap();

    assert!(matches!(reader.next_chunk(), Err(TransferError::Io(_))));
    assert_eq!(reader.offset(), 0);
    let retried = reader.next_chunk().unwrap().unwrap();
    assert_eq!(retried.offset, 0);
    assert_eq!(&retried.payload[..], b"abcd");
}

#[test]
fn part_writer_accepts_outer_authenticated_raw_reader_chunks_without_cipher_adapter() {
    let root = TestDir::new();
    let id = TransferId::generate();
    let mut reader =
        TransferFileReader::new(Cursor::new(b"raw-data".to_vec()), id, 1, 8, 4).unwrap();
    let mut writer = TransferPartWriter::create(root.path(), "raw.bin", 1, 8).unwrap();

    while let Some(chunk) = reader.next_chunk().unwrap() {
        writer.write_raw_chunk(&chunk).unwrap();
    }
    let final_path = writer.finalize().unwrap();
    assert_eq!(fs::read(final_path).unwrap(), b"raw-data");
}

#[test]
fn raw_writer_rejects_compressed_or_length_mismatched_chunks_without_advancing() {
    let root = TestDir::new();
    let mut writer = TransferPartWriter::create(root.path(), "strict.bin", 0, 1).unwrap();
    let id = TransferId::generate();
    let mut chunk = nexkvm_streaming::TransferChunk {
        transfer_id: id,
        file_index: 0,
        offset: 0,
        plain_len: 1,
        compression: nexkvm_streaming::TransferCompression::Deflate,
        final_chunk_for_file: true,
        payload: Bytes::from_static(b"x"),
    };
    assert!(matches!(
        writer.write_raw_chunk(&chunk),
        Err(TransferError::Codec(_))
    ));
    chunk.compression = nexkvm_streaming::TransferCompression::None;
    chunk.plain_len = 2;
    assert!(matches!(
        writer.write_raw_chunk(&chunk),
        Err(TransferError::Codec(_))
    ));
    assert_eq!(writer.offset(), 0);
}

#[test]
fn part_writer_validates_offsets_flushes_and_atomically_finalizes() {
    let root = TestDir::new();
    fs::create_dir(root.path().join("nested")).unwrap();
    let mut writer = TransferPartWriter::create(root.path(), "nested/data.bin", 7, 6).unwrap();
    assert!(writer.part_path().ends_with("data.bin.part"));

    writer.write_chunk(&decoded(7, 0, b"abc", false)).unwrap();
    let wrong_offset = writer.write_chunk(&decoded(7, 1, b"x", false));
    assert!(matches!(
        wrong_offset,
        Err(TransferError::UnexpectedOffset {
            expected: 3,
            actual: 1
        })
    ));
    assert_eq!(writer.offset(), 3, "rejected chunk must not advance state");

    writer.write_chunk(&decoded(7, 3, b"def", true)).unwrap();
    writer.flush().unwrap();
    let part_path = writer.part_path().to_path_buf();
    let final_path = writer.finalize().unwrap();

    assert_eq!(fs::read(final_path).unwrap(), b"abcdef");
    assert!(!part_path.exists());
}

#[test]
fn part_writer_resumes_existing_part_without_rewriting_prefix() {
    let root = TestDir::new();
    let mut writer = TransferPartWriter::create(root.path(), "resume.bin", 2, 6).unwrap();
    writer.write_chunk(&decoded(2, 0, b"abc", false)).unwrap();
    writer.flush().unwrap();
    drop(writer);

    let mut resumed = TransferPartWriter::resume(root.path(), "resume.bin", 2, 6, 3).unwrap();
    resumed.write_chunk(&decoded(2, 3, b"def", true)).unwrap();
    let final_path = resumed.finalize().unwrap();
    assert_eq!(fs::read(final_path).unwrap(), b"abcdef");
}

#[test]
fn finalize_never_overwrites_a_destination_created_during_transfer() {
    let root = TestDir::new();
    let mut writer = TransferPartWriter::create(root.path(), "race.bin", 0, 3).unwrap();
    writer.write_chunk(&decoded(0, 0, b"new", true)).unwrap();
    fs::write(root.path().join("race.bin"), b"existing").unwrap();
    let part_path = writer.part_path().to_path_buf();

    assert!(matches!(
        writer.finalize(),
        Err(TransferError::DestinationExists(_))
    ));
    assert_eq!(fs::read(root.path().join("race.bin")).unwrap(), b"existing");
    assert!(
        part_path.exists(),
        "failed finalize must preserve resumable data"
    );
}

#[cfg(unix)]
#[test]
fn writer_rejects_symlinked_destination_ancestors() {
    use std::os::unix::fs::symlink;

    let root = TestDir::new();
    fs::create_dir(root.path().join("real")).unwrap();
    symlink(root.path().join("real"), root.path().join("linked")).unwrap();

    assert!(matches!(
        TransferPartWriter::create(root.path(), "linked/escape.bin", 0, 1),
        Err(TransferError::UnsafeDestination(_))
    ));
}

#[test]
fn writer_rejects_traversal_before_touching_disk() {
    let root = TestDir::new();
    let outside = root.path().parent().unwrap().join("escape.bin.part");
    let _ = fs::remove_file(&outside);

    assert!(matches!(
        TransferPartWriter::create(root.path(), "../escape.bin", 0, 1),
        Err(TransferError::InvalidPath(_))
    ));
    assert!(!outside.exists());
}

#[test]
fn transfer_directory_creation_is_nested_idempotent_and_never_overwrites_files() {
    let root = TestDir::new();
    let created = create_transfer_directory(root.path(), "empty/nested").unwrap();
    assert!(created.is_dir());
    assert_eq!(
        create_transfer_directory(root.path(), "empty/nested").unwrap(),
        created
    );

    fs::write(root.path().join("occupied"), b"file").unwrap();
    assert!(matches!(
        create_transfer_directory(root.path(), "occupied/child"),
        Err(TransferError::DestinationExists(_))
    ));
    assert_eq!(fs::read(root.path().join("occupied")).unwrap(), b"file");
}

#[cfg(unix)]
#[test]
fn transfer_directory_creation_rejects_symlink_ancestors_and_traversal() {
    use std::os::unix::fs::symlink;

    let root = TestDir::new();
    fs::create_dir(root.path().join("real")).unwrap();
    symlink(root.path().join("real"), root.path().join("linked-dir")).unwrap();
    assert!(matches!(
        create_transfer_directory(root.path(), "linked-dir/child"),
        Err(TransferError::UnsafeDestination(_))
    ));
    assert!(matches!(
        create_transfer_directory(root.path(), "../outside"),
        Err(TransferError::InvalidPath(_))
    ));
}
