use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

use crate::config::{ensure_private_dir, paths};

pub mod container;

const LABEL: &str = "dev.ssh-clipboard";
const MANAGED_MARKER: &str = "# Managed by ssh-clipboard; use the CLI instead of editing this file.";

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Start,
    Stop,
    Restart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Running,
    PendingLogin,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstallOptions {
    pub headless_x11: bool,
    pub reset_display: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReconcileAction {
    None,
    Start,
    Restart,
}

impl InstallOutcome {
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::Running => "Installed and running",
            Self::PendingLogin => "Installed; starts at next login",
        }
    }

    #[must_use]
    pub fn from_detail(value: &str) -> Option<Self> {
        match value.trim() {
            "Installed and running" => Some(Self::Running),
            "Installed; starts at next login" => Some(Self::PendingLogin),
            _ => None,
        }
    }
}

pub async fn install(binary: &Path, options: InstallOptions) -> Result<InstallOutcome> {
    if !binary.is_absolute() {
        bail!("service binary path must be absolute");
    }
    let expected_version = binary_version(binary).await?;
    let paths = paths()?;
    if let Some(parent) = paths.service.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    ensure_private_dir(&paths.state_dir)?;
    if cfg!(target_os = "linux") && (container::installed()? || container::required().await) {
        container::install(binary, options).await?;
        wait_until_healthy(&paths.socket, &expected_version).await?;
        return Ok(InstallOutcome::Running);
    }
    if options.headless_x11 && !cfg!(target_os = "linux") {
        bail!("managed Xvfb is supported only on Linux");
    }
    let xvfb = if options.headless_x11 {
        Some(xvfb_binary().await?)
    } else {
        None
    };
    let preserved_display = if cfg!(target_os = "linux") && !options.headless_x11 && !options.reset_display {
        existing_display_directive(&paths.service).await?
    } else {
        None
    };
    let contents = if cfg!(target_os = "macos") {
        launch_agent(binary, &paths.log)
    } else if cfg!(target_os = "linux") {
        systemd_unit(binary, options.headless_x11, preserved_display.as_deref())
    } else {
        bail!("background services are supported on macOS and Linux");
    };
    let already_healthy = check_health(&paths.socket, &expected_version).await.is_ok();
    if xvfb.is_some() {
        protect_existing_xvfb(&paths.headless_service).await?;
    }
    write_atomic(&paths.service, contents.as_bytes()).await?;
    if let Some(xvfb) = xvfb {
        write_atomic(&paths.headless_service, xvfb_unit(&xvfb).as_bytes()).await?;
    }
    let outcome = if cfg!(target_os = "macos") {
        start_macos(&paths.service, already_healthy).await
    } else {
        start_linux(
            already_healthy,
            options.headless_x11,
            options.reset_display,
            &paths.headless_service,
        )
        .await
    }?;
    if outcome == InstallOutcome::Running {
        wait_until_healthy(&paths.socket, &expected_version).await?;
    }
    Ok(outcome)
}

pub async fn control(action: Action) -> Result<()> {
    if cfg!(target_os = "macos") {
        let domain = format!("gui/{}", uid().await?);
        match action {
            Action::Start => run("launchctl", &["kickstart", &format!("{domain}/{LABEL}")]).await,
            Action::Stop => run("launchctl", &["bootout", &format!("{domain}/{LABEL}")]).await,
            Action::Restart => run("launchctl", &["kickstart", "-k", &format!("{domain}/{LABEL}")]).await,
        }
    } else if cfg!(target_os = "linux") {
        if container::installed()? {
            return container::control(action).await;
        }
        let action = match action {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
        };
        run("systemctl", &["--user", action, "ssh-clipboard.service"]).await
    } else {
        bail!("background services are supported on macOS and Linux")
    }
}

fn launch_agent(binary: &Path, log: &Path) -> String {
    let binary = xml_escape(&binary.display().to_string());
    let log = xml_escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{binary}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#
    )
}

fn systemd_unit(binary: &Path, headless_x11: bool, preserved_display: Option<&str>) -> String {
    let binary = systemd_quote(&binary.display().to_string());
    let companion = if headless_x11 {
        "Wants=ssh-clipboard-xvfb.service\nAfter=ssh-clipboard-xvfb.service"
    } else {
        "After=graphical-session.target"
    };
    let display = if headless_x11 {
        "Environment=DISPLAY=:99\nPassEnvironment=XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS".to_owned()
    } else if let Some(directive) = preserved_display {
        format!("{directive}\nPassEnvironment=WAYLAND_DISPLAY XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS")
    } else {
        "PassEnvironment=DISPLAY WAYLAND_DISPLAY XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS".to_owned()
    };
    format!(
        r"[Unit]
Description=Native encrypted clipboard sync over SSH
{companion}

[Service]
Type=simple
ExecStart={binary} daemon
Restart=always
RestartSec=1
{display}

[Install]
WantedBy=default.target
"
    )
}

fn xvfb_unit(binary: &Path) -> String {
    let binary = systemd_quote(&binary.display().to_string());
    format!(
        r"{MANAGED_MARKER}
[Unit]
Description=Private virtual X11 display for ssh-clipboard

[Service]
Type=simple
ExecStart={binary} :99 -screen 0 1280x720x24 -nolisten tcp -noreset
Restart=on-failure
RestartSec=1

[Install]
WantedBy=default.target
"
    )
}

async fn start_macos(service_path: &Path, already_healthy: bool) -> Result<InstallOutcome> {
    let domain = format!("gui/{}", uid().await?);
    if !command_succeeds("launchctl", &["print", &domain]).await? {
        return Ok(InstallOutcome::PendingLogin);
    }

    let service = format!("{domain}/{LABEL}");
    let loaded = command_succeeds("launchctl", &["print", &service]).await?;
    match reconcile_action(loaded, already_healthy) {
        ReconcileAction::None => {}
        ReconcileAction::Start => {
            let bootstrap = run(
                "launchctl",
                &["bootstrap", &domain, &service_path.display().to_string()],
            )
            .await;
            if let Err(error) = bootstrap
                && !command_succeeds("launchctl", &["print", &service]).await?
            {
                return Err(error).context("install LaunchAgent");
            }
            // RunAtLoad starts a newly bootstrapped job. Killing it here would
            // trigger launchd's minimum-runtime throttle and create avoidable
            // clipboard downtime.
        }
        ReconcileAction::Restart => {
            run("launchctl", &["kickstart", "-k", &service])
                .await
                .context("restart LaunchAgent")?;
        }
    }
    Ok(InstallOutcome::Running)
}

async fn start_linux(
    already_healthy: bool,
    headless_x11: bool,
    reset_display: bool,
    headless_service: &Path,
) -> Result<InstallOutcome> {
    if !command_succeeds("systemctl", &["--user", "show-environment"]).await? {
        return Ok(InstallOutcome::PendingLogin);
    }
    let _ = run(
        "systemctl",
        &[
            "--user",
            "import-environment",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
        ],
    )
    .await;
    run("systemctl", &["--user", "daemon-reload"]).await?;
    if headless_x11 {
        run("systemctl", &["--user", "enable", "ssh-clipboard-xvfb.service"]).await?;
        run("systemctl", &["--user", "restart", "ssh-clipboard-xvfb.service"]).await?;
    } else if reset_display && is_managed_xvfb(headless_service).await? {
        let _ = run(
            "systemctl",
            &["--user", "disable", "--now", "ssh-clipboard-xvfb.service"],
        )
        .await;
        tokio::fs::remove_file(headless_service).await?;
        run("systemctl", &["--user", "daemon-reload"]).await?;
    }
    run("systemctl", &["--user", "enable", "ssh-clipboard.service"]).await?;
    let active = command_succeeds(
        "systemctl",
        &["--user", "is-active", "--quiet", "ssh-clipboard.service"],
    )
    .await?;
    match reconcile_action(active, already_healthy) {
        ReconcileAction::None => {}
        ReconcileAction::Start => {
            run("systemctl", &["--user", "start", "ssh-clipboard.service"]).await?;
        }
        ReconcileAction::Restart => {
            run("systemctl", &["--user", "restart", "ssh-clipboard.service"]).await?;
        }
    }
    Ok(InstallOutcome::Running)
}

async fn existing_display_directive(service: &Path) -> Result<Option<String>> {
    let Ok(contents) = tokio::fs::read_to_string(service).await else {
        return Ok(None);
    };
    Ok(contents.lines().find_map(|line| {
        let trimmed = line.trim();
        (trimmed.starts_with("Environment=DISPLAY=") || trimmed.starts_with("Environment=\"DISPLAY="))
            .then(|| trimmed.to_owned())
    }))
}

async fn protect_existing_xvfb(service: &Path) -> Result<()> {
    if service.is_file() && !is_managed_xvfb(service).await? {
        bail!(
            "{} already exists and is not managed by ssh-clipboard; it was left unchanged",
            service.display()
        );
    }
    if Path::new("/tmp/.X11-unix/X99").exists() && !service.is_file() {
        bail!("X display :99 is already in use; the existing display was left unchanged");
    }
    Ok(())
}

async fn is_managed_xvfb(service: &Path) -> Result<bool> {
    match tokio::fs::read_to_string(service).await {
        Ok(contents) => Ok(contents.lines().next() == Some(MANAGED_MARKER)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn xvfb_binary() -> Result<PathBuf> {
    let output = Command::new("sh")
        .args(["-c", "command -v Xvfb"])
        .stdin(Stdio::null())
        .output()
        .await
        .context("find Xvfb")?;
    if !output.status.success() {
        bail!("Xvfb is not installed; install it with your system package manager, then retry");
    }
    let path = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    if !path.is_absolute() {
        bail!("Xvfb resolved to a non-absolute path");
    }
    Ok(path)
}

const fn reconcile_action(loaded: bool, healthy: bool) -> ReconcileAction {
    match (loaded, healthy) {
        (true, true) => ReconcileAction::None,
        (true, false) => ReconcileAction::Restart,
        (false, _) => ReconcileAction::Start,
    }
}

async fn uid() -> Result<String> {
    let output = Command::new("id").arg("-u").output().await?;
    if !output.status.success() {
        bail!("could not determine user id");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn run(program: &str, arguments: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("run {program}"))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(())
}

async fn command_succeeds(program: &str, arguments: &[&str]) -> Result<bool> {
    Ok(Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .with_context(|| format!("run {program}"))?
        .success())
}

async fn binary_version(binary: &Path) -> Result<String> {
    let output = Command::new(binary).arg("--version").output().await?;
    if !output.status.success() {
        bail!("service binary failed its version check");
    }
    let output = String::from_utf8(output.stdout)?;
    output
        .trim()
        .strip_prefix("ssh-clipboard ")
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
        .context("service binary reported an invalid version")
}

async fn wait_until_healthy(socket: &Path, expected_version: &str) -> Result<()> {
    let mut last_error = None;
    for _ in 0..40 {
        match check_health(socket, expected_version).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("daemon did not create its status socket")))
        .context("background service did not become healthy")
}

async fn check_health(socket: &Path, expected_version: &str) -> Result<()> {
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(b"STATUS\n").await?;
    let mut response = String::new();
    let read = tokio::time::timeout(
        Duration::from_millis(500),
        BufReader::new(stream).read_line(&mut response),
    )
    .await??;
    if read == 0 {
        bail!("daemon returned an empty status");
    }
    let status: serde_json::Value = serde_json::from_str(&response)?;
    if status.get("running").and_then(serde_json::Value::as_bool) != Some(true) {
        bail!("daemon reported that it is not running");
    }
    if status.get("version").and_then(serde_json::Value::as_str) != Some(expected_version) {
        bail!("daemon is not running the expected version {expected_version}");
    }
    Ok(())
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = PathBuf::from(format!("{}.new", path.display()));
    tokio::fs::write(&temporary, bytes).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote(value: &str) -> String {
    format!(r#""{}""#, value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_documents_escape_paths() {
        let plist = launch_agent(Path::new("/Users/a&b/tool"), Path::new("/tmp/a&b.log"));
        assert!(plist.contains("/Users/a&amp;b/tool"));
        let unit = systemd_unit(Path::new("/home/me/My Tools/tool"), false, None);
        assert!(unit.contains("ExecStart=\"/home/me/My Tools/tool\" daemon"));
    }

    #[test]
    fn headless_service_is_local_only_and_orders_the_clipboard_after_it() {
        let unit = systemd_unit(Path::new("/home/me/tool"), true, None);
        assert!(unit.contains("Wants=ssh-clipboard-xvfb.service"));
        assert!(unit.contains("Environment=DISPLAY=:99"));
        let xvfb = xvfb_unit(Path::new("/usr/bin/Xvfb"));
        assert!(xvfb.contains("ExecStart=\"/usr/bin/Xvfb\" :99"));
        assert!(xvfb.contains("-nolisten tcp"));
        assert!(!xvfb.contains(" -ac"));
    }

    #[test]
    fn existing_display_directives_survive_service_reconciliation() {
        let unit = systemd_unit(Path::new("/home/me/tool"), false, Some("Environment=DISPLAY=:42"));
        assert!(unit.contains("Environment=DISPLAY=:42"));
        assert!(!unit.contains("Environment=DISPLAY=:99"));
    }

    #[tokio::test]
    async fn foreign_xvfb_units_are_never_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let service = directory.path().join("ssh-clipboard-xvfb.service");
        tokio::fs::write(&service, "[Service]\nExecStart=/custom/Xvfb :99\n")
            .await
            .unwrap();
        let error = protect_existing_xvfb(&service).await.unwrap_err();
        assert!(error.to_string().contains("left unchanged"));
        assert_eq!(
            tokio::fs::read_to_string(service).await.unwrap(),
            "[Service]\nExecStart=/custom/Xvfb :99\n"
        );
    }

    #[test]
    fn install_outcomes_round_trip_through_remote_output() {
        for outcome in [InstallOutcome::Running, InstallOutcome::PendingLogin] {
            assert_eq!(InstallOutcome::from_detail(outcome.detail()), Some(outcome));
        }
    }

    #[test]
    fn service_reconciliation_avoids_unnecessary_or_double_restarts() {
        assert_eq!(reconcile_action(true, true), ReconcileAction::None);
        assert_eq!(reconcile_action(true, false), ReconcileAction::Restart);
        assert_eq!(reconcile_action(false, false), ReconcileAction::Start);
        assert_eq!(reconcile_action(false, true), ReconcileAction::Start);
    }

    #[tokio::test]
    async fn health_check_requires_a_live_status_response() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"{\"running\":true,\"version\":\"test\"}\n")
                .await
                .unwrap();
        });

        wait_until_healthy(&socket, "test").await.unwrap();
    }
}
