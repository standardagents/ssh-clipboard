use std::collections::HashMap;
use std::fmt;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::read::GzDecoder;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{ensure_private_dir, paths};
use crate::deploy;

mod peer_binary;
pub(crate) use peer_binary::peer_binary;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const NPM_LATEST_URL: &str = "https://registry.npmjs.org/ssh-clipboard/latest";
const NPM_TARBALL_PREFIX: &str = "https://registry.npmjs.org/ssh-clipboard/-/";
const CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const WATCHDOG_SECONDS: u64 = 90;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StableVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl FromStr for StableVersion {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let mut parts = value.split('.');
        let major = parts.next().context("version is missing major")?.parse()?;
        let minor = parts.next().context("version is missing minor")?.parse()?;
        let patch = parts.next().context("version is missing patch")?.parse()?;
        if parts.next().is_some() {
            bail!("version must have exactly three numeric components");
        }
        Ok(Self { major, minor, patch })
    }
}

impl fmt::Display for StableVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct NpmRelease {
    version: String,
    dist: NpmDistribution,
}

#[derive(Clone, Debug, Deserialize)]
struct NpmDistribution {
    tarball: String,
    integrity: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestEntry {
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct UpdateState {
    desired_version: String,
}

pub(crate) fn initial_desired_version() -> String {
    let Ok(path) = paths().map(|paths| paths.state_dir.join("update.json")) else {
        return CURRENT_VERSION.to_owned();
    };
    let Ok(bytes) = std::fs::read(path) else {
        return CURRENT_VERSION.to_owned();
    };
    let Ok(state) = serde_json::from_slice::<UpdateState>(&bytes) else {
        return CURRENT_VERSION.to_owned();
    };
    if newer_version(CURRENT_VERSION, &state.desired_version) {
        state.desired_version
    } else {
        CURRENT_VERSION.to_owned()
    }
}

pub(crate) async fn run_auto_updates(
    desired: watch::Sender<String>,
    mut hints: mpsc::UnboundedReceiver<String>,
) -> String {
    if std::env::var_os("SSH_CLIPBOARD_DISABLE_AUTO_UPDATE").is_some() {
        info!("automatic updates disabled by environment");
        std::future::pending::<()>().await;
        unreachable!();
    }

    let client = update_client().expect("build update HTTP client");
    let mut checks = tokio::time::interval(CHECK_INTERVAL);
    checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let reason = tokio::select! {
            _ = checks.tick() => "scheduled check",
            hint = hints.recv() => {
                if let Some(version) = hint {
                    info!(%version, "peer announced an update");
                    "peer announcement"
                } else {
                    std::future::pending::<()>().await;
                    unreachable!();
                }
            }
        };
        match reconcile(&client, &desired).await {
            Ok(Some(version)) => return version,
            Ok(None) => {}
            Err(error) => warn!(%error, %reason, "automatic update check failed"),
        }
    }
}

pub async fn latest_version() -> Result<String> {
    Ok(fetch_latest(&update_client()?).await?.version)
}

pub async fn update_now() -> Result<Option<String>> {
    let (desired, _) = watch::channel(CURRENT_VERSION.to_owned());
    reconcile(&update_client()?, &desired).await
}

fn update_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!("ssh-clipboard/{CURRENT_VERSION}"))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("build update HTTP client")
}

async fn reconcile(client: &Client, desired: &watch::Sender<String>) -> Result<Option<String>> {
    let release = fetch_latest(client).await?;
    let current = StableVersion::from_str(CURRENT_VERSION).context("parse running version")?;
    let latest = StableVersion::from_str(&release.version).context("parse npm latest version")?;
    if latest <= current {
        return Ok(None);
    }

    persist_desired_version(&release.version).await?;
    desired.send_if_modified(|known| {
        let known_version = StableVersion::from_str(known).unwrap_or(current);
        if latest > known_version {
            known.clone_from(&release.version);
            true
        } else {
            false
        }
    });

    info!(current = %current, latest = %latest, "installing automatic update");
    let package = download_package(client, &release.dist).await?;
    let target = deploy::current_target_name()?;
    let version = release.version.clone();
    let binary = tokio::task::spawn_blocking(move || extract_binary(&package, &target))
        .await
        .context("update extraction task failed")??;
    install_verified_binary(&binary, &version).await?;
    Ok(Some(version))
}

async fn persist_desired_version(version: &str) -> Result<()> {
    let state_dir = paths()?.state_dir;
    ensure_private_dir(&state_dir)?;
    let path = state_dir.join("update.json");
    let temporary = state_dir.join(".update.json.new");
    let mut bytes = serde_json::to_vec_pretty(&UpdateState {
        desired_version: version.to_owned(),
    })?;
    bytes.push(b'\n');
    tokio::fs::write(&temporary, bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

async fn fetch_latest(client: &Client) -> Result<NpmRelease> {
    client
        .get(NPM_LATEST_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("decode npm latest release")
}

async fn download_package(client: &Client, distribution: &NpmDistribution) -> Result<Vec<u8>> {
    let tarball = Url::parse(&distribution.tarball).context("parse npm tarball URL")?;
    if !tarball.as_str().starts_with(NPM_TARBALL_PREFIX) {
        bail!("npm returned an untrusted tarball URL");
    }
    let response = client.get(tarball).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PACKAGE_BYTES)
    {
        bail!("npm package exceeds update size limit");
    }
    let bytes = response.bytes().await?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PACKAGE_BYTES {
        bail!("npm package exceeds update size limit");
    }
    verify_integrity(&bytes, &distribution.integrity)?;
    Ok(bytes.to_vec())
}

fn verify_integrity(bytes: &[u8], integrity: &str) -> Result<()> {
    let encoded = integrity
        .split_whitespace()
        .find_map(|digest| digest.strip_prefix("sha512-"))
        .context("npm release has no SHA-512 integrity digest")?;
    let expected = STANDARD.decode(encoded).context("decode npm integrity digest")?;
    let actual = Sha512::digest(bytes);
    if actual.as_slice() != expected {
        bail!("npm package integrity check failed");
    }
    Ok(())
}

fn extract_binary(package: &[u8], target: &str) -> Result<Vec<u8>> {
    let manifest_path = "package/vendor/manifest.json";
    let binary_path = format!("package/vendor/{target}/ssh-clipboard");
    let mut manifest = None;
    let mut binary = None;
    let decoder = GzDecoder::new(Cursor::new(package));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("read npm archive")? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if path == manifest_path {
            let mut bytes = Vec::new();
            entry.by_ref().take(1024 * 1024).read_to_end(&mut bytes)?;
            manifest = Some(serde_json::from_slice::<HashMap<String, ManifestEntry>>(&bytes)?);
        } else if path == binary_path {
            if entry.size() > MAX_BINARY_BYTES {
                bail!("update binary exceeds size limit");
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            binary = Some(bytes);
        }
    }
    let manifest = manifest.context("npm package is missing its binary manifest")?;
    let binary = binary.with_context(|| format!("npm package is missing {target}"))?;
    let metadata = manifest
        .get(target)
        .with_context(|| format!("npm manifest is missing {target}"))?;
    if metadata.bytes != u64::try_from(binary.len()).unwrap_or(u64::MAX)
        || metadata.sha256 != hex_digest(&binary)
    {
        bail!("update binary does not match the npm manifest");
    }
    validate_executable_target(&binary, target)?;
    Ok(binary)
}

async fn install_verified_binary(binary: &[u8], version: &str) -> Result<()> {
    let update_dir = paths()?.state_dir.join("updates");
    ensure_private_dir(&update_dir)?;
    let staged = update_dir.join(format!("ssh-clipboard-{version}-{}", Uuid::new_v4().simple()));
    tokio::fs::write(&staged, binary).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let result = async {
        verify_staged_version(&staged, version).await?;
        let mut watchdog = spawn_watchdog(version)?;
        if let Err(error) = deploy::install_local_binary(Some(&staged)).await {
            let _ = watchdog.kill().await;
            return Err(error);
        }
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&staged).await;
    result
}

fn spawn_watchdog(version: &str) -> Result<tokio::process::Child> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.args(["update-watchdog", version]);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.kill_on_drop(false);
    command.spawn().context("start update rollback watchdog")
}

pub async fn mark_healthy() -> Result<()> {
    let state_dir = paths()?.state_dir;
    ensure_private_dir(&state_dir)?;
    let path = state_dir.join("healthy-version");
    let temporary = state_dir.join(".healthy-version.new");
    tokio::fs::write(&temporary, format!("{CURRENT_VERSION}\n")).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

pub async fn watchdog(version: &str) -> Result<()> {
    StableVersion::from_str(version).context("parse watchdog version")?;
    let health = paths()?.state_dir.join("healthy-version");
    for _ in 0..WATCHDOG_SECONDS {
        if tokio::fs::read_to_string(&health)
            .await
            .is_ok_and(|healthy| healthy.trim() == version)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    deploy::restore_previous_binary().await?;
    crate::service::control(crate::service::Action::Restart)
        .await
        .context("restart service after update rollback")
}

async fn verify_staged_version(binary: &PathBuf, version: &str) -> Result<()> {
    let output = Command::new(binary).arg("--version").output().await?;
    if !output.status.success() {
        bail!("staged update failed its version check");
    }
    let reported = String::from_utf8(output.stdout)?;
    if reported.trim() != format!("ssh-clipboard {version}") {
        bail!("staged update reported an unexpected version");
    }
    Ok(())
}

fn validate_executable_target(binary: &[u8], target: &str) -> Result<()> {
    if binary.len() < 20 {
        bail!("update executable header is truncated");
    }
    let detected = if binary.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
        match u32::from_le_bytes(binary[4..8].try_into().expect("four-byte CPU type")) {
            0x0100_000c => "darwin-arm64",
            0x0100_0007 => "darwin-amd64",
            _ => bail!("update has an unsupported Mach-O CPU type"),
        }
    } else if binary.starts_with(b"\x7fELF") && binary[4] == 2 && binary[5] == 1 {
        match u16::from_le_bytes(binary[18..20].try_into().expect("two-byte ELF machine")) {
            183 => "linux-arm64",
            62 => "linux-amd64",
            _ => bail!("update has an unsupported ELF machine"),
        }
    } else {
        bail!("update has an unexpected executable format");
    };
    if detected != target {
        bail!("update contains {detected}, expected {target}");
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = fmt::Write::write_fmt(&mut encoded, format_args!("{byte:02x}"));
    }
    encoded
}

#[must_use]
pub fn newer_version(current: &str, candidate: &str) -> bool {
    StableVersion::from_str(candidate)
        .and_then(|candidate| Ok(candidate > StableVersion::from_str(current)?))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    #[test]
    fn stable_versions_are_ordered_and_prereleases_are_rejected() {
        assert!(newer_version("0.2.9", "0.3.0"));
        assert!(!newer_version("1.0.0", "0.99.99"));
        assert!(!newer_version("1.0.0", "1.1.0-dev.abcdef0"));
    }

    #[test]
    fn automatic_updates_are_checked_every_fifteen_minutes() {
        assert_eq!(CHECK_INTERVAL, Duration::from_secs(15 * 60));
    }

    #[test]
    fn npm_integrity_must_match() {
        let bytes = b"release package";
        let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(bytes)));
        verify_integrity(bytes, &integrity).unwrap();
        assert!(verify_integrity(b"tampered", &integrity).is_err());
    }

    #[test]
    fn extracts_only_the_manifested_target_binary() {
        let target = if cfg!(target_os = "macos") {
            if cfg!(target_arch = "aarch64") {
                "darwin-arm64"
            } else {
                "darwin-amd64"
            }
        } else if cfg!(target_arch = "aarch64") {
            "linux-arm64"
        } else {
            "linux-amd64"
        };
        let binary = fake_binary(target);
        let manifest = serde_json::json!({
            target: {
                "bytes": binary.len(),
                "sha256": hex_digest(&binary),
            }
        });
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append(
            &mut archive,
            "package/vendor/manifest.json",
            manifest.to_string().as_bytes(),
        );
        append(
            &mut archive,
            &format!("package/vendor/{target}/ssh-clipboard"),
            &binary,
        );
        let encoder = archive.into_inner().unwrap();
        let package = encoder.finish().unwrap();

        assert_eq!(extract_binary(&package, target).unwrap(), binary);
    }

    fn append<W: Write>(archive: &mut tar::Builder<W>, path: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(bytes.len()).unwrap());
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, path, bytes).unwrap();
    }

    pub(super) fn fake_binary(target: &str) -> Vec<u8> {
        let mut bytes = vec![0; 32];
        match target {
            "darwin-arm64" => {
                bytes[0..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
                bytes[4..8].copy_from_slice(&0x0100_000c_u32.to_le_bytes());
            }
            "darwin-amd64" => {
                bytes[0..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
                bytes[4..8].copy_from_slice(&0x0100_0007_u32.to_le_bytes());
            }
            "linux-arm64" => {
                bytes[0..6].copy_from_slice(b"\x7fELF\x02\x01");
                bytes[18..20].copy_from_slice(&183_u16.to_le_bytes());
            }
            "linux-amd64" => {
                bytes[0..6].copy_from_slice(b"\x7fELF\x02\x01");
                bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
            }
            _ => unreachable!(),
        }
        bytes
    }
}
