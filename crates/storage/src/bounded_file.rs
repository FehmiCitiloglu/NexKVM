use std::fs::{self, File, Metadata};
use std::io::{Error, ErrorKind, Read};
use std::path::Path;

/// Read a sensitive regular file without ever buffering more than
/// `maximum + 1` bytes. Unix targets also verify that the path and opened file
/// retain the same device/inode identity throughout the read.
pub(crate) fn read_owner_only_bounded_regular_file(
    path: &Path,
    maximum: u64,
) -> Result<Vec<u8>, Error> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_metadata(&path_metadata, maximum)?;

    let mut file = File::open(path)?;
    let opened_metadata = file.metadata()?;
    validate_metadata(&opened_metadata, maximum)?;
    ensure_same_file(&path_metadata, &opened_metadata)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    revalidate_path(path, &opened_metadata, maximum)?;
    let bytes = read_open_file_bounded(&mut file, maximum)?;

    let final_metadata = file.metadata()?;
    validate_metadata(&final_metadata, maximum)?;
    ensure_same_file(&opened_metadata, &final_metadata)?;
    revalidate_path(path, &opened_metadata, maximum)?;
    Ok(bytes)
}

fn read_open_file_bounded(file: &mut File, maximum: u64) -> Result<Vec<u8>, Error> {
    let read_limit = maximum
        .checked_add(1)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "file size limit is invalid"))?;
    let initial_len = file.metadata()?.len().min(maximum);
    let capacity = usize::try_from(initial_len)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "file exceeds configured size limit"))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref().take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(too_large_error());
    }
    Ok(bytes)
}

fn validate_metadata(metadata: &Metadata, maximum: u64) -> Result<(), Error> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "path is not a regular file",
        ));
    }
    if metadata.len() > maximum {
        return Err(too_large_error());
    }
    Ok(())
}

fn revalidate_path(path: &Path, opened: &Metadata, maximum: u64) -> Result<(), Error> {
    let current = fs::symlink_metadata(path)?;
    validate_metadata(&current, maximum)?;
    ensure_same_file(opened, &current)
}

#[cfg(unix)]
fn ensure_same_file(left: &Metadata, right: &Metadata) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;

    if left.dev() != right.dev() || left.ino() != right.ino() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "path changed while opening file",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_file(_left: &Metadata, _right: &Metadata) -> Result<(), Error> {
    Ok(())
}

fn too_large_error() -> Error {
    Error::new(ErrorKind::InvalidData, "file exceeds configured size limit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn descriptor_reader_rejects_growth_after_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("growing");
        fs::write(&path, b"ok").unwrap();
        let mut opened = File::open(&path).unwrap();
        let mut writer = fs::OpenOptions::new().append(true).open(path).unwrap();
        writer.write_all(b"-too-large").unwrap();

        let error = read_open_file_bounded(&mut opened, 4).expect_err("growth must be bounded");

        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }
}
