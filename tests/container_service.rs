//! Exercise the real detached supervisor with an isolated, synthetic daemon.
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

fn cli(state: &Path, args: &[&str]) -> std::process::Output {
    Command::new(binary())
        .env("SSH_CLIPBOARD_STATE_DIR", state)
        .env("SSH_CLIPBOARD_CONFIG_DIR", state)
        .env_remove("DISPLAY")
        .args(args)
        .output()
        .unwrap()
}

fn binary() -> std::path::PathBuf {
    std::env::var_os("SSH_CLIPBOARD_TEST_BINARY").map_or_else(
        || env!("CARGO_BIN_EXE_ssh-clipboard").into(),
        std::path::PathBuf::from,
    )
}

fn wait_for(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while !check() {
        assert!(Instant::now() < deadline, "service transition timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn supervisor_restarts_owned_child_and_preserves_display() {
    let directory = tempfile::Builder::new().prefix("sc-").tempdir_in("/tmp").unwrap();
    let state = directory.path();
    let fixture = state.join("daemon");
    std::fs::write(&fixture, include_bytes!("fixtures/supervised-daemon.sh")).unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let settings = json!({
        "binary": fixture, "environment": {"DISPLAY": ":5"},
        "xvfb": null, "enabled": true
    });
    std::fs::write(
        state.join("container-service.json"),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let mut supervisor = Command::new(binary())
        .arg("service-supervisor")
        .env("SSH_CLIPBOARD_STATE_DIR", state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let marker = state.join("test-child");
    wait_for(|| marker.exists());
    let first = std::fs::read_to_string(&marker).unwrap();
    assert!(first.trim_end().ends_with(" :5"));
    let duplicate = cli(state, &["service-supervisor"]);
    assert!(duplicate.status.success());
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), first);
    // The daemon exits after an update; the supervisor must start a fresh child.
    let pid: i32 = first.split_whitespace().next().unwrap().parse().unwrap();
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM).unwrap();
    wait_for(|| std::fs::read_to_string(&marker).is_ok_and(|s| s != first));
    let second = std::fs::read_to_string(&marker).unwrap();
    assert!(second.trim_end().ends_with(" :5"));
    // Service control is Linux-only; exercise the same private protocol on macOS CI.
    let mut socket = std::os::unix::net::UnixStream::connect(state.join("supervisor.sock")).unwrap();
    socket.write_all(b"R").unwrap();
    let mut ack = [0];
    socket.read_exact(&mut ack).unwrap();
    assert_eq!(ack, *b"K");
    wait_for(|| std::fs::read_to_string(&marker).is_ok_and(|s| s != second));
    if cfg!(target_os = "linux") {
        assert!(cli(state, &["service", "stop"]).status.success());
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state.join("container-service.json")).unwrap()).unwrap();
        assert_eq!(settings["enabled"], false);
    } else {
        let mut socket = std::os::unix::net::UnixStream::connect(state.join("supervisor.sock")).unwrap();
        socket.write_all(b"S").unwrap();
        socket.read_exact(&mut ack).unwrap();
    }
    wait_for(|| supervisor.try_wait().unwrap().is_some());
    assert!(!state.join("supervisor.sock").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn real_container_service_start_stop_restart_and_ssh_revival() {
    struct Cleanup(std::process::Child, std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = cli(&self.1, &["service", "stop"]);
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let directory = tempfile::Builder::new().prefix("sc-").tempdir_in("/tmp").unwrap();
    let state = directory.path();
    let _cleanup = Cleanup(
        Command::new("Xvfb")
            .args([
                ":96",
                "-screen",
                "0",
                "640x480x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        state.to_owned(),
    );
    wait_for(|| Path::new("/tmp/.X11-unix/X96").exists());
    ssh_clipboard::config::Config::default()
        .save_at(&state.join("config.json"))
        .unwrap();
    // Select the fallback even on a CI host with systemd as PID 1.
    std::fs::write(
        state.join("container-service.json"),
        serde_json::to_vec(&json!({
            "binary": binary(), "environment": {"DISPLAY": ":96"}, "xvfb": null, "enabled": false
        }))
        .unwrap(),
    )
    .unwrap();
    let installed = Command::new(binary())
        .env("SSH_CLIPBOARD_STATE_DIR", state)
        .env("SSH_CLIPBOARD_CONFIG_DIR", state)
        .env("DISPLAY", ":96")
        .env_remove("WAYLAND_DISPLAY")
        .args(["service", "install", "--native-display"])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(cli(state, &["status"]).status.success());
    assert!(cli(state, &["service", "restart"]).status.success());
    wait_for(|| cli(state, &["status"]).status.success());
    assert!(cli(state, &["service", "stop"]).status.success());
    assert!(!cli(state, &["status"]).status.success());
    // Stopped services are not revived by a stray incoming bridge.
    assert!(!cli(state, &["bridge"]).status.success());
    assert!(cli(state, &["service", "start"]).status.success());
    wait_for(|| cli(state, &["status"]).status.success());
    // Simulate a container lifetime boundary without changing the enabled flag.
    let mut socket = std::os::unix::net::UnixStream::connect(state.join("supervisor.sock")).unwrap();
    socket.write_all(b"S").unwrap();
    socket.read_exact(&mut [0]).unwrap();
    wait_for(|| !state.join("supervisor.sock").exists());
    assert!(cli(state, &["bridge"]).status.success());
    assert!(cli(state, &["status"]).status.success());
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("container-service.json")).unwrap()).unwrap();
    assert_eq!(saved["environment"]["DISPLAY"], ":96");
}
