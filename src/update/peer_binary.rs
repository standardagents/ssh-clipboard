use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CURRENT_VERSION, NpmRelease, download_package, extract_binary, hex_digest, update_client,
    validate_executable_target,
};
use crate::{
    config::{ensure_private_dir, paths},
    deploy,
};

#[derive(Serialize, Deserialize)]
struct CachedBinary {
    version: String,
    target: String,
    sha256: String,
}

pub(crate) async fn peer_binary(os: &str, arch: &str) -> Result<PathBuf> {
    deploy::validate_target(os, arch)?;
    let target = format!("{os}-{arch}");
    resolve(
        &paths()?.state_dir.join("peer-binaries"),
        CURRENT_VERSION,
        &target,
        || download(CURRENT_VERSION, &target),
    )
    .await
}

async fn download(version: &str, target: &str) -> Result<Vec<u8>> {
    let client = update_client()?;
    let release = client
        .get(release_url(version)?)
        .send()
        .await?
        .error_for_status()?
        .json::<NpmRelease>()
        .await
        .context("read exact-version npm release")?;
    require_version(&release, version)?;
    let package = download_package(&client, &release.dist).await?;
    let target = target.to_owned();
    tokio::task::spawn_blocking(move || extract_binary(&package, &target)).await?
}

fn release_url(version: &str) -> Result<Url> {
    if !version.as_bytes().first().is_some_and(u8::is_ascii_digit)
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
    {
        bail!("invalid release version");
    }
    Ok(Url::parse(&format!(
        "https://registry.npmjs.org/ssh-clipboard/{version}"
    ))?)
}

fn require_version(release: &NpmRelease, version: &str) -> Result<()> {
    if release.version != version {
        bail!(
            "npm returned v{} instead of requested v{version}",
            release.version
        );
    }
    Ok(())
}

async fn resolve<F, Fut>(root: &Path, version: &str, target: &str, download: F) -> Result<PathBuf>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    release_url(version)?;
    if !matches!(
        target,
        "darwin-arm64" | "darwin-amd64" | "linux-arm64" | "linux-amd64"
    ) {
        bail!("unsupported peer target {target}");
    }
    let directory = root.join(version).join(target);
    let binary_path = directory.join("ssh-clipboard");
    let metadata_path = directory.join("verified.json");
    // Metadata and executable are checked on every use. Incomplete, stale,
    // or corrupt entries are cache misses, never a reason to upload bad bytes.
    if let (Ok(metadata), Ok(binary)) = (
        tokio::fs::read(&metadata_path).await,
        tokio::fs::read(&binary_path).await,
    ) && let Ok(metadata) = serde_json::from_slice::<CachedBinary>(&metadata)
        && metadata.version == version
        && metadata.target == target
        && metadata.sha256 == hex_digest(&binary)
        && validate_executable_target(&binary, target).is_ok()
    {
        return Ok(binary_path);
    }
    let binary = download().await?;
    validate_executable_target(&binary, target)?;
    ensure_private_dir(root)?;
    ensure_private_dir(&root.join(version))?;
    ensure_private_dir(&directory)?;
    let metadata = serde_json::to_vec(&CachedBinary {
        version: version.into(),
        target: target.into(),
        sha256: hex_digest(&binary),
    })?;
    write_atomic(&binary_path, &binary).await?;
    write_atomic(&metadata_path, &metadata).await?;
    Ok(binary_path)
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{}.new", Uuid::new_v4().simple()));
    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o700);
        let mut file = options.open(&temporary).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

#[cfg(test)]
mod tests;
