//! Same-binary supervision for containers without a user service manager.
//! A persisted enabled flag lets incoming SSH revive an installed service after
//! container restart, without undoing an explicit `service stop`.
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;

use super::{Action, InstallOptions};
use crate::config::{ensure_private_dir, paths};

const ENVIRONMENT: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

#[derive(Debug, Serialize, Deserialize)]
struct Settings {
    binary: PathBuf,
    environment: BTreeMap<String, String>,
    xvfb: Option<PathBuf>,
    enabled: bool,
}

fn settings_path() -> Result<PathBuf> {
    Ok(paths()?.state_dir.join("container-service.json"))
}

fn socket_path() -> Result<PathBuf> {
    Ok(paths()?.state_dir.join("supervisor.sock"))
}

pub fn installed() -> Result<bool> {
    Ok(settings_path()?.is_file())
}

pub async fn required() -> bool {
    !super::command_succeeds("systemctl", &["--user", "show-environment"])
        .await
        .unwrap_or(false)
        && !Path::new("/run/systemd/system").exists()
}

fn read_settings() -> Result<Settings> {
    Ok(serde_json::from_slice(&std::fs::read(settings_path()?)?)?)
}

fn save_settings(settings: &Settings) -> Result<()> {
    let path = settings_path()?;
    let temporary = path.with_extension(format!("{}.new", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(settings)?)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

pub async fn install(binary: &Path, options: InstallOptions) -> Result<()> {
    let previous = if installed()? {
        Some(read_settings()?)
    } else {
        None
    };
    let environment = select_environment(
        previous.as_ref().map(|s| &s.environment),
        std::env::vars()
            .filter(|(key, _)| ENVIRONMENT.contains(&key.as_str()))
            .collect(),
        options,
    )?;
    let xvfb = if options.headless_x11 {
        if previous.as_ref().is_none_or(|s| s.xvfb.is_none()) || request(b'P').await.is_err() {
            super::protect_existing_xvfb(&paths()?.headless_service).await?;
        }
        Some(super::xvfb_binary().await?)
    } else {
        None
    };
    save_settings(&Settings {
        binary: binary.to_owned(),
        environment,
        xvfb,
        enabled: true,
    })?;
    control(Action::Restart).await
}

fn select_environment(
    previous: Option<&BTreeMap<String, String>>,
    current: BTreeMap<String, String>,
    options: InstallOptions,
) -> Result<BTreeMap<String, String>> {
    let explicit_display = current.get("DISPLAY").is_some_and(|s| !s.is_empty())
        || current.get("WAYLAND_DISPLAY").is_some_and(|s| !s.is_empty());
    let mut environment = if explicit_display || options.reset_display {
        current
    } else {
        previous.cloned().unwrap_or(current)
    };
    if options.headless_x11 {
        environment.remove("WAYLAND_DISPLAY");
        environment.insert("DISPLAY".into(), ":99".into());
    } else if environment.get("DISPLAY").is_none_or(String::is_empty)
        && environment.get("WAYLAND_DISPLAY").is_none_or(String::is_empty)
    {
        bail!(
            "No desktop display selected. Run service install --native-display from the Linux desktop terminal, or supply DISPLAY and XAUTHORITY for that desktop. Use --headless-x11 only for a private virtual clipboard."
        );
    }
    Ok(environment)
}

async fn request(command: u8) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut stream = UnixStream::connect(socket_path()?).await?;
        stream.write_all(&[command]).await?;
        let response = stream.read_u8().await?;
        anyhow::ensure!(response == b'K', "supervisor rejected command");
        Ok(())
    })
    .await
    .context("supervisor control timed out")?
}

pub async fn control(action: Action) -> Result<()> {
    let mut settings = read_settings()?;
    settings.enabled = !matches!(action, Action::Stop);
    save_settings(&settings)?;
    match action {
        Action::Stop => {
            if request(b'P').await.is_ok() {
                request(b'S').await?;
                for _ in 0..50 {
                    if request(b'P').await.is_err() {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                bail!("container supervisor did not stop");
            }
            Ok(())
        }
        Action::Start => start(&settings).await,
        Action::Restart => {
            if request(b'P').await.is_ok() {
                request(b'R').await
            } else {
                start(&settings).await
            }
        }
    }
}

async fn start(settings: &Settings) -> Result<()> {
    if request(b'P').await.is_ok() {
        return Ok(());
    }
    let paths = paths()?;
    ensure_private_dir(&paths.state_dir)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&paths.log)?;
    // The child calls setsid before supervising; no external daemon package or shell.
    Command::new(&settings.binary)
        .arg("service-supervisor")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    for _ in 0..50 {
        if request(b'P').await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "container supervisor failed to start; inspect {}",
        paths.log.display()
    )
}

pub async fn ensure_started() -> Result<()> {
    if !installed()? {
        return Ok(());
    }
    let settings = read_settings()?;
    if !settings.enabled {
        return Ok(());
    }
    start(&settings).await?;
    super::wait_until_healthy(&paths()?.socket, &super::binary_version(&settings.binary).await?).await
}

pub async fn supervise() -> Result<()> {
    nix::unistd::setsid().context("detach container supervisor")?;
    let paths = paths()?;
    ensure_private_dir(&paths.state_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(paths.state_dir.join("supervisor.lock"))?;
    // Hold this lock until every owned child has exited. Never signal a saved PID.
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let socket = socket_path()?;
    if socket.exists() {
        anyhow::ensure!(
            std::fs::symlink_metadata(&socket)?.file_type().is_socket(),
            "refusing to remove a non-socket supervisor path"
        );
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let result = run_supervisor(listener).await;
    let _ = std::fs::remove_file(socket);
    result
}

/// Avoid leaving an orphan clipboard owner if the container supervisor is killed.
pub fn prepare_daemon() -> Result<()> {
    #[cfg(target_os = "linux")]
    if let Ok(parent) = std::env::var("SSH_CLIPBOARD_SUPERVISED_PARENT") {
        nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM)?;
        anyhow::ensure!(
            nix::unistd::getppid().as_raw().to_string() == parent,
            "supervisor exited before daemon startup"
        );
    }
    Ok(())
}

async fn run_supervisor(listener: UnixListener) -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let settings = read_settings()?;
        if !settings.enabled {
            return Ok(());
        }
        let mut display = if let Some(binary) = &settings.xvfb {
            anyhow::ensure!(
                !Path::new("/tmp/.X11-unix/X99").exists(),
                "display :99 already in use"
            );
            let child = Command::new(binary)
                .args([
                    ":99",
                    "-screen",
                    "0",
                    "1280x720x24",
                    "-nolisten",
                    "tcp",
                    "-noreset",
                ])
                .kill_on_drop(true)
                .spawn()?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            Some(child)
        } else {
            None
        };
        let mut command = Command::new(&settings.binary);
        command
            .arg("daemon")
            .env("SSH_CLIPBOARD_SUPERVISED_PARENT", std::process::id().to_string())
            .envs(&settings.environment)
            .kill_on_drop(true);
        for key in ENVIRONMENT {
            if !settings.environment.contains_key(*key) {
                command.env_remove(key);
            }
        }
        let mut daemon = command.spawn().context("start supervised daemon")?;
        let restart = loop {
            tokio::select! {
                _ = daemon.wait() => break true,
                _ = terminate.recv() => break false,
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted?;
                    let Ok(Ok(command)) = tokio::time::timeout(Duration::from_secs(1), stream.read_u8()).await else { continue; };
                    if !matches!(command, b'P' | b'R' | b'S') { continue; }
                    // Ack before stopping the daemon: it may be the updater calling us.
                    let _ = stream.write_all(b"K").await;
                    if command != b'P' { break command == b'R'; }
                }
            }
        };
        let _ = daemon.kill().await;
        let _ = daemon.wait().await;
        if let Some(child) = display.as_mut() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if !restart {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_update_preserves_desktop_environment() {
        let saved = BTreeMap::from([
            ("DISPLAY".into(), ":5".into()),
            ("XAUTHORITY".into(), "/tmp/auth".into()),
        ]);
        assert_eq!(
            select_environment(Some(&saved), BTreeMap::new(), InstallOptions::default()).unwrap(),
            saved
        );
    }

    #[test]
    fn explicit_native_display_replaces_private_display() {
        let saved = BTreeMap::from([("DISPLAY".into(), ":99".into())]);
        let current = BTreeMap::from([("DISPLAY".into(), ":5".into())]);
        assert_eq!(
            select_environment(
                Some(&saved),
                current.clone(),
                InstallOptions {
                    reset_display: true,
                    headless_x11: false
                }
            )
            .unwrap(),
            current
        );
    }

    #[test]
    fn missing_desktop_requires_explicit_choice() {
        assert!(select_environment(None, BTreeMap::new(), InstallOptions::default()).is_err());
        let environment = select_environment(
            None,
            BTreeMap::new(),
            InstallOptions {
                headless_x11: true,
                reset_display: false,
            },
        )
        .unwrap();
        assert_eq!(environment["DISPLAY"], ":99");
    }
}
