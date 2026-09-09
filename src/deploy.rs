use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::config::{Config, paths};
use crate::service;
use crate::ssh::{self, ProbeResult};
use crate::update::{self, CURRENT_VERSION};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installation {
    pub version: Option<String>,
    pub config_exists: bool,
    pub service_exists: bool,
    pub running: bool,
}

impl Installation {
    #[must_use]
    pub fn needs_binary(&self) -> bool {
        match self.version.as_deref() {
            Some(version) if version == CURRENT_VERSION => false,
            Some(version) if update::newer_version(CURRENT_VERSION, version) => false,
            None | Some(_) => true,
        }
    }

    #[must_use]
    pub fn summary(&self) -> String {
        match (self.version.as_deref(), self.service_exists, self.running) {
            (None, true, _) => format!("Incomplete installation · repair with v{CURRENT_VERSION}"),
            (None, false, _) => format!("Not installed · ready for v{CURRENT_VERSION}"),
            (Some(version), false, _) if version == CURRENT_VERSION => {
                format!("v{version} installed · service setup needed")
            }
            (Some(version), true, true) if version == CURRENT_VERSION => {
                format!("Already installed and running · v{version}")
            }
            (Some(version), true, false) if version == CURRENT_VERSION => {
                format!("Already installed · v{version} · service restart needed")
            }
            (Some(version), false, _) if update::newer_version(CURRENT_VERSION, version) => {
                format!("Newer v{version} installed · service setup needed")
            }
            (Some(version), true, true) if update::newer_version(CURRENT_VERSION, version) => {
                format!("Newer version already installed · v{version}")
            }
            (Some(version), true, false) if update::newer_version(CURRENT_VERSION, version) => {
                format!("Newer v{version} installed · service restart needed")
            }
            (Some(version), _, _) => format!("Upgrade ready · v{version} → v{CURRENT_VERSION}"),
        }
    }
}

pub fn validate_target(os: &str, arch: &str) -> Result<()> {
    if !matches!(os, "darwin" | "linux") || !matches!(arch, "arm64" | "amd64") {
        bail!("unsupported target {os}/{arch}");
    }
    Ok(())
}

pub async fn binary_for(os: &str, arch: &str) -> Result<PathBuf> {
    validate_target(os, arch)?;
    let current = std::env::current_exe()?;
    if current_target() == (os, arch) {
        return Ok(current);
    }
    let bundle_root = std::env::var_os("SSH_CLIPBOARD_BINARIES_DIR").map(PathBuf::from);
    let candidates = binary_candidates(&current, os, arch, bundle_root.as_deref());
    if let Some(binary) = candidates.into_iter().find(|candidate| candidate.is_file()) {
        return Ok(binary);
    }
    update::peer_binary(os, arch).await.with_context(|| {
        format!("obtain v{CURRENT_VERSION} binary for {os}/{arch}; check internet access and retry")
    })
}

fn binary_candidates(current: &Path, os: &str, arch: &str, bundle_root: Option<&Path>) -> Vec<PathBuf> {
    let filename = format!("ssh-clipboard-{os}-{arch}");
    let mut candidates = Vec::with_capacity(5);
    if let Some(root) = bundle_root {
        candidates.push(root.join(format!("{os}-{arch}")).join("ssh-clipboard"));
        candidates.push(root.join(&filename));
    }
    let executable_dir = current.parent().unwrap_or_else(|| Path::new("."));
    candidates.push(executable_dir.join(&filename));
    candidates.push(executable_dir.join("dist").join(&filename));
    candidates.push(PathBuf::from("dist").join(filename));
    candidates
}

pub async fn install_remote<F>(
    ssh_command: &str,
    probe: &ProbeResult,
    headless_x11: bool,
    mut progress: F,
) -> Result<service::InstallOutcome>
where
    F: FnMut(&str, &str),
{
    progress("inspect", "Inspecting the existing installation");
    let installation = inspect_remote(ssh_command, probe).await?;
    if installation.needs_binary() {
        progress(
            "resolve",
            "Locating peer binary (downloading and verifying if needed)",
        );
        let binary = binary_for(&probe.os, &probe.arch).await?;
        let detail = installation.version.as_ref().map_or_else(
            || format!("Installing v{CURRENT_VERSION}"),
            |version| format!("Upgrading v{version} → v{CURRENT_VERSION}"),
        );
        progress("upload", &detail);
        ssh::upload_binary(ssh_command, &binary).await?;
    } else {
        progress("binary", &installation.summary());
    }

    if installation.config_exists {
        progress("configure", "Keeping existing identity and peer configuration");
    } else {
        let mut remote = Config::default();
        if !probe.hostname.is_empty() {
            remote.node_name.clone_from(&probe.hostname);
        }
        let mut encoded = serde_json::to_vec_pretty(&remote)?;
        encoded.push(b'\n');
        progress("configure", "Creating a private node configuration");
        ssh::upload_config_if_missing(ssh_command, &encoded).await?;
    }

    progress("service", "Ensuring the per-user background service is available");
    let output = ssh::run(ssh_command, remote_service_install_command(headless_x11)).await?;
    let detail = String::from_utf8(output)?;
    service::InstallOutcome::from_detail(&detail)
        .context("remote service installer returned an unexpected result")
}

fn remote_service_install_command(headless_x11: bool) -> &'static str {
    if headless_x11 {
        r#"exec "$HOME/.local/bin/ssh-clipboard" service install --headless-x11 --binary "$HOME/.local/bin/ssh-clipboard""#
    } else {
        r#"exec "$HOME/.local/bin/ssh-clipboard" service install --binary "$HOME/.local/bin/ssh-clipboard""#
    }
}

pub async fn inspect_remote(ssh_command: &str, probe: &ProbeResult) -> Result<Installation> {
    let token = format!("SCB_INSTALL_{}", Uuid::new_v4().simple());
    let service = if probe.os == "darwin" {
        "$HOME/Library/LaunchAgents/dev.ssh-clipboard.plist"
    } else {
        "$HOME/.config/systemd/user/ssh-clipboard.service"
    };
    let remote = format!(
        r#"version=''; running=0; if [ -x "$HOME/.local/bin/ssh-clipboard" ]; then version=$("$HOME/.local/bin/ssh-clipboard" --version 2>/dev/null || true); version="${{version#ssh-clipboard }}"; "$HOME/.local/bin/ssh-clipboard" status --json >/dev/null 2>&1 && running=1; fi; config=0; [ -f "$HOME/.config/ssh-clipboard/config.json" ] && config=1; service=0; [ -f "{service}" ] && service=1; printf '{token}\t%s\t%s\t%s\t%s\n' "$version" "$config" "$service" "$running""#
    );
    let output = String::from_utf8(ssh::run(ssh_command, &remote).await?)?;
    parse_installation(&output, &token)
}

fn parse_installation(output: &str, token: &str) -> Result<Installation> {
    let line = output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{token}\t")))
        .context("installation inspection returned no result")?;
    let mut fields = line.split('\t');
    let version = fields
        .next()
        .filter(|version| !version.trim().is_empty())
        .map(str::to_owned);
    let config_exists = parse_flag(fields.next(), "config")?;
    let service_exists = parse_flag(fields.next(), "service")?;
    let running = parse_flag(fields.next(), "running")?;
    if fields.next().is_some() {
        bail!("installation inspection returned unexpected fields");
    }
    Ok(Installation {
        version,
        config_exists,
        service_exists,
        running,
    })
}

fn parse_flag(value: Option<&str>, name: &str) -> Result<bool> {
    match value {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(_) => bail!("installation inspection returned an invalid {name} flag"),
        None => bail!("installation inspection omitted the {name} flag"),
    }
}

pub async fn install_local_binary(source: Option<&Path>) -> Result<PathBuf> {
    let source = match source {
        Some(source) => source.to_path_buf(),
        None => std::env::current_exe()?,
    };
    let destination = paths()?.binary;
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = PathBuf::from(format!("{}.new", destination.display()));
    tokio::fs::copy(source, &temporary).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755)).await?;
    }
    if destination.is_file() {
        let previous = PathBuf::from(format!("{}.previous", destination.display()));
        tokio::fs::copy(&destination, &previous).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&previous, std::fs::Permissions::from_mode(0o700)).await?;
        }
    }
    tokio::fs::rename(&temporary, &destination).await?;
    Ok(destination)
}

pub async fn install_local_service() -> Result<service::InstallOutcome> {
    let destination = paths()?.binary;
    let installed = binary_version(&destination).await;
    let binary = if installed.as_deref() == Some(CURRENT_VERSION)
        || installed
            .as_deref()
            .is_some_and(|version| update::newer_version(CURRENT_VERSION, version))
    {
        destination
    } else {
        install_local_binary(None).await?
    };
    let headless_x11 = Config::load()?.headless_x11;
    service::install(
        &binary,
        service::InstallOptions {
            headless_x11,
            reset_display: false,
        },
    )
    .await
}

async fn binary_version(binary: &Path) -> Option<String> {
    let output = tokio::process::Command::new(binary)
        .arg("--version")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .strip_prefix("ssh-clipboard ")
        .map(str::to_owned)
}

pub async fn restore_previous_binary() -> Result<PathBuf> {
    let destination = paths()?.binary;
    let previous = PathBuf::from(format!("{}.previous", destination.display()));
    if !previous.is_file() {
        bail!("no previous ssh-clipboard binary is available");
    }
    let temporary = PathBuf::from(format!("{}.rollback", destination.display()));
    tokio::fs::copy(&previous, &temporary).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755)).await?;
    }
    tokio::fs::rename(temporary, &destination).await?;
    Ok(destination)
}

fn current_target() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    (os, arch)
}

pub fn current_target_name() -> Result<String> {
    let (os, arch) = current_target();
    if !matches!(os, "darwin" | "linux") || !matches!(arch, "arm64" | "amd64") {
        bail!("unsupported current target {os}/{arch}");
    }
    Ok(format!("{os}-{arch}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_binary_is_selected_for_the_current_target() {
        let (os, arch) = current_target();
        assert_eq!(
            binary_for(os, arch).await.unwrap(),
            std::env::current_exe().unwrap()
        );
    }

    #[tokio::test]
    async fn unsupported_targets_are_rejected() {
        assert!(binary_for("plan9", "mips").await.is_err());
    }

    #[test]
    fn npm_bundle_layout_is_searched_first() {
        let candidates = binary_candidates(
            Path::new("/app/vendor/darwin-arm64/ssh-clipboard"),
            "linux",
            "amd64",
            Some(Path::new("/app/vendor")),
        );
        assert_eq!(
            candidates[0],
            PathBuf::from("/app/vendor/linux-amd64/ssh-clipboard")
        );
        assert_eq!(
            candidates[1],
            PathBuf::from("/app/vendor/ssh-clipboard-linux-amd64")
        );
    }

    #[test]
    fn parses_existing_installations_without_losing_state() {
        let output = format!("banner\nTOKEN\t{CURRENT_VERSION}\t1\t1\t1\n");
        let installation = parse_installation(&output, "TOKEN").unwrap();
        assert_eq!(
            installation,
            Installation {
                version: Some(CURRENT_VERSION.into()),
                config_exists: true,
                service_exists: true,
                running: true,
            }
        );
        assert!(!installation.needs_binary());
        assert_eq!(
            installation.summary(),
            format!("Already installed and running · v{CURRENT_VERSION}")
        );
    }

    #[test]
    fn older_and_missing_installations_receive_the_current_binary() {
        assert!(
            Installation {
                version: Some("0.1.2".into()),
                config_exists: true,
                service_exists: true,
                running: true,
            }
            .needs_binary()
        );
        assert!(
            Installation {
                version: None,
                config_exists: false,
                service_exists: false,
                running: false,
            }
            .needs_binary()
        );
    }

    #[test]
    fn rejects_truncated_installation_inspection() {
        let error = parse_installation("TOKEN\t0.2.1\t1\n", "TOKEN").unwrap_err();
        assert!(error.to_string().contains("service flag"));
    }

    #[test]
    fn remote_headless_install_is_explicit() {
        assert!(!remote_service_install_command(false).contains("--headless-x11"));
        assert!(remote_service_install_command(true).contains("--headless-x11"));
    }
}
