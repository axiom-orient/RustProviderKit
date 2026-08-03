use std::fs::File;
use std::io::Read;
use std::path::Path;

use rust_provider_kit_core::{ProviderFailure, ProviderFailureCode};

#[derive(Debug, Clone, Copy)]
pub(crate) struct SecureRegularFileReader;
impl SecureRegularFileReader {
    pub(crate) async fn read_async(
        path: &Path,
        maximum_bytes: usize,
    ) -> Result<Vec<u8>, ProviderFailure> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::read(&path, maximum_bytes))
            .await
            .map_err(|_| failure("secure file reader terminated unexpectedly"))?
    }

    pub(crate) fn read(path: &Path, maximum_bytes: usize) -> Result<Vec<u8>, ProviderFailure> {
        if maximum_bytes == 0 {
            return Err(failure("secure file byte bound is invalid"));
        }
        let before =
            std::fs::symlink_metadata(path).map_err(|_| failure("secure file is unavailable"))?;
        if before.file_type().is_symlink() || !before.is_file() {
            return Err(failure("secure file must be a regular non-symlink file"));
        }
        let before_size = usize::try_from(before.len())
            .map_err(|_| failure("secure file size is not representable"))?;
        if before_size > maximum_bytes {
            return Err(failure("secure file exceeds byte limit"));
        }
        let mut file = File::open(path).map_err(|_| failure("secure file cannot be opened"))?;
        let opened = file
            .metadata()
            .map_err(|_| failure("secure file metadata is unavailable"))?;
        if !opened.is_file() || !same_identity(&before, &opened) {
            return Err(failure("secure file changed during open"));
        }
        let opened_size = usize::try_from(opened.len())
            .map_err(|_| failure("secure file size is not representable"))?;
        let limit = u64::try_from(maximum_bytes)
            .map_err(|_| failure("secure file byte bound is not representable"))?
            .checked_add(1)
            .ok_or_else(|| failure("secure file byte bound overflow"))?;
        let mut result = Vec::with_capacity(opened_size.min(maximum_bytes));
        let mut limited = (&mut file).take(limit);
        limited
            .read_to_end(&mut result)
            .map_err(|_| failure("secure file read failed"))?;
        if result.len() > maximum_bytes {
            return Err(failure("secure file exceeds byte limit"));
        }
        let after = file
            .metadata()
            .map_err(|_| failure("secure file metadata is unavailable after read"))?;
        if !same_identity(&opened, &after) || after.len() != opened.len() {
            return Err(failure("secure file changed during read"));
        }
        Ok(result)
    }
}
#[cfg(unix)]
fn same_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}
#[cfg(not(unix))]
fn same_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    if left.len() != right.len() {
        return false;
    }
    match (left.modified(), right.modified()) {
        (Ok(left_modified), Ok(right_modified)) => left_modified == right_modified,
        _ => false,
    }
}
fn failure(message: &str) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::AuthenticationFailed, message)
}
