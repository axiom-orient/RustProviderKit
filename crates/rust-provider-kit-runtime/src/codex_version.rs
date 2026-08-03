use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rust_provider_kit_core::{ProviderFailure, ProviderFailureCode, ProviderJsonValue};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::watch;

use crate::secure_file::SecureRegularFileReader;

#[derive(Debug, Default)]
struct VersionCache {
    ready: HashMap<PathBuf, String>,
    loading: HashMap<PathBuf, Arc<VersionLoad>>,
}

#[derive(Debug)]
struct VersionLoad {
    result: watch::Sender<Option<Result<String, ProviderFailure>>>,
}

impl VersionLoad {
    fn new() -> Self {
        let (result, _receiver) = watch::channel(None);
        Self { result }
    }

    async fn wait(&self) -> Result<String, ProviderFailure> {
        let mut receiver = self.result.subscribe();
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }
            if receiver.changed().await.is_err() {
                return Err(unavailable());
            }
        }
    }
}

#[derive(Debug)]
enum CacheDecision {
    Ready(String),
    Wait(Arc<VersionLoad>),
    Load(Arc<VersionLoad>),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CodexClientVersion {
    cache: Arc<Mutex<VersionCache>>,
}

impl CodexClientVersion {
    pub(crate) async fn resolve(
        &self,
        auth_path: &Path,
        executable: Option<&Path>,
    ) -> Result<String, ProviderFailure> {
        if let Some(version) = managed_version(auth_path).await? {
            return Ok(version);
        }

        let executable = executable.ok_or_else(unavailable)?;
        let path = executable.to_path_buf();
        let decision = {
            let mut cache = self.cache.lock();
            if let Some(version) = cache.ready.get(&path) {
                CacheDecision::Ready(version.clone())
            } else if let Some(load) = cache.loading.get(&path) {
                CacheDecision::Wait(Arc::clone(load))
            } else {
                let load = Arc::new(VersionLoad::new());
                cache.loading.insert(path.clone(), Arc::clone(&load));
                CacheDecision::Load(load)
            }
        };

        match decision {
            CacheDecision::Ready(version) => Ok(version),
            CacheDecision::Wait(load) => load.wait().await,
            CacheDecision::Load(load) => {
                let mut owner =
                    VersionLoadOwner::new(Arc::clone(&self.cache), path, Arc::clone(&load));
                let result = resolve_executable_version(executable).await;
                owner.finish(&result);
                result
            }
        }
    }
}

async fn managed_version(auth_path: &Path) -> Result<Option<String>, ProviderFailure> {
    let auth_path = auth_path.to_path_buf();
    tokio::task::spawn_blocking(move || managed_version_blocking(&auth_path))
        .await
        .map_err(|_| unavailable())?
}

fn managed_version_blocking(auth_path: &Path) -> Result<Option<String>, ProviderFailure> {
    let Some(parent) = auth_path.parent() else {
        return Ok(None);
    };
    let metadata = parent.join("version.json");
    match std::fs::symlink_metadata(&metadata) {
        Ok(_) => {
            let data = SecureRegularFileReader::read(&metadata, 64 * 1_024)?;
            let root = ProviderJsonValue::decode(&data).map_err(|_| unavailable())?;
            let value = root
                .at(&["latest_version"])
                .and_then(ProviderJsonValue::as_str)
                .filter(|value| valid_version(value))
                .ok_or_else(unavailable)?;
            Ok(Some(value.to_owned()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(unavailable()),
    }
}

async fn resolve_executable_version(executable: &Path) -> Result<String, ProviderFailure> {
    let executable = executable.to_path_buf();
    let validation_path = executable.clone();
    tokio::task::spawn_blocking(move || validate_executable(&validation_path))
        .await
        .map_err(|_| unavailable())??;
    let mut command = tokio::process::Command::new(&executable);
    command
        .arg("--version")
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| unavailable())?;
    let stdout = child.stdout.take().ok_or_else(unavailable)?;
    let output_task = tokio::spawn(read_bounded(stdout, 16 * 1_024));
    let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(result) => result.map_err(|_| unavailable())?,
        Err(_) => {
            let _result = child.kill().await;
            let _result = child.wait().await;
            output_task.abort();
            let _result = output_task.await;
            return Err(unavailable());
        }
    };
    let output = output_task
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    if !status.success() {
        return Err(unavailable());
    }
    let text = std::str::from_utf8(&output).map_err(|_| unavailable())?;
    let mut fields = text.split_whitespace();
    let product = fields.next();
    let version = fields.next();
    if product != Some("codex-cli") || fields.next().is_some() {
        return Err(unavailable());
    }
    version
        .filter(|value| valid_version(value))
        .map(str::to_owned)
        .ok_or_else(unavailable)
}

struct VersionLoadOwner {
    cache: Arc<Mutex<VersionCache>>,
    path: PathBuf,
    load: Arc<VersionLoad>,
    completed: bool,
}

impl VersionLoadOwner {
    fn new(cache: Arc<Mutex<VersionCache>>, path: PathBuf, load: Arc<VersionLoad>) -> Self {
        Self {
            cache,
            path,
            load,
            completed: false,
        }
    }

    fn finish(&mut self, result: &Result<String, ProviderFailure>) {
        let _previous = self.load.result.send_replace(Some(result.clone()));
        {
            let mut cache = self.cache.lock();
            if cache
                .loading
                .get(&self.path)
                .is_some_and(|value| Arc::ptr_eq(value, &self.load))
            {
                cache.loading.remove(&self.path);
            }
            if let Ok(version) = &result {
                cache.ready.insert(self.path.clone(), version.clone());
            }
        }
        self.completed = true;
    }
}

impl Drop for VersionLoadOwner {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let _previous = self.load.result.send_replace(Some(Err(unavailable())));
        let mut cache = self.cache.lock();
        if cache
            .loading
            .get(&self.path)
            .is_some_and(|value| Arc::ptr_eq(value, &self.load))
        {
            cache.loading.remove(&self.path);
        }
        drop(cache);
    }
}

async fn read_bounded<R>(mut reader: R, maximum_bytes: usize) -> Result<Vec<u8>, ()>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0u8; 4 * 1_024];
    loop {
        let count = reader.read(&mut buffer).await.map_err(|_| ())?;
        if count == 0 {
            return Ok(output);
        }
        let next = output.len().checked_add(count).ok_or(())?;
        if next > maximum_bytes {
            return Err(());
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

pub(crate) fn is_qualified_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

fn validate_executable(path: &Path) -> Result<(), ProviderFailure> {
    if !is_qualified_executable(path) {
        return Err(unavailable());
    }
    Ok(())
}

fn unavailable() -> ProviderFailure {
    ProviderFailure::new(
        ProviderFailureCode::AuthenticationFailed,
        "Codex client version is unavailable; provide version.json beside auth.json or install Codex",
    )
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use tempfile::tempdir;

    use super::{CodexClientVersion, is_qualified_executable};

    #[tokio::test]
    async fn completed_single_flight_releases_late_waiters() {
        use std::time::Duration;

        let load = super::VersionLoad::new();
        let _previous = load.result.send_replace(Some(Ok("1.2.3".to_owned())));
        let result = tokio::time::timeout(Duration::from_millis(250), load.wait()).await;
        assert!(matches!(result, Ok(Ok(version)) if version == "1.2.3"));
    }

    #[tokio::test]
    async fn managed_version_metadata_is_preferred_and_bounded() -> Result<(), Box<dyn Error>> {
        let directory = tempdir()?;
        let auth = directory.path().join("auth.json");
        fs::write(&auth, b"{}")?;
        fs::write(
            directory.path().join("version.json"),
            br#"{"latest_version":"1.2.3"}"#,
        )?;
        let resolver = CodexClientVersion::default();
        assert_eq!(resolver.resolve(&auth, None).await?, "1.2.3");
        fs::write(
            directory.path().join("version.json"),
            vec![b'x'; 64 * 1_024 + 1],
        )?;
        assert!(resolver.resolve(&auth, None).await.is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn executable_qualification_rejects_non_executable_files_and_symlinks()
    -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir()?;
        let executable = directory.path().join("codex");
        fs::write(&executable, b"#!/bin/sh\n")?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o644))?;
        assert!(!is_qualified_executable(&executable));

        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        assert!(is_qualified_executable(&executable));

        let link = directory.path().join("codex-link");
        symlink(&executable, &link)?;
        assert!(!is_qualified_executable(&link));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_resolution_starts_one_executable_probe() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir()?;
        let auth = directory.path().join("auth.json");
        let executable = directory.path().join("codex");
        let counter = directory.path().join("count");
        fs::write(&auth, b"{}")?;
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nsleep 0.1\nprintf 'codex-cli 9.9.9\\n'\n",
                counter.display(),
            ),
        )?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;

        let resolver = CodexClientVersion::default();
        let (left, right) = tokio::join!(
            resolver.resolve(&auth, Some(&executable)),
            resolver.resolve(&auth, Some(&executable)),
        );
        assert_eq!(left?, "9.9.9");
        assert_eq!(right?, "9.9.9");
        assert_eq!(fs::read_to_string(counter)?, "x");
        Ok(())
    }
}
