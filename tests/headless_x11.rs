#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ssh_clipboard::clipboard::{ClipboardBackend, NativeClipboard};
use ssh_clipboard::model::Representation;

const DISPLAY: &str = ":97";
const CHILD_MARKER: &str = "SSH_CLIPBOARD_XVFB_TEST_CHILD";

struct Xvfb(Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn headless_x11_round_trip() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let clipboard = NativeClipboard::new(1024 * 1024).unwrap();
            assert_eq!(clipboard.name(), "X11");
            clipboard
                .apply(&[
                    Representation {
                        item: 0,
                        format: "text/plain".into(),
                        data: b"headless clipboard smoke test".to_vec(),
                    },
                    Representation {
                        item: 0,
                        format: "text/x-ssh-clipboard-probe".into(),
                        data: b"headless clipboard smoke test".to_vec(),
                    },
                ])
                .await
                .unwrap();
            let snapshot = clipboard.capture().await.unwrap().unwrap();
            assert!(
                snapshot
                    .representations
                    .iter()
                    .any(|representation| representation.format == "text/plain"
                        && representation.data == b"headless clipboard smoke test")
            );
            assert!(
                snapshot
                    .representations
                    .iter()
                    .any(|representation| representation.format == "text/x-ssh-clipboard-probe"),
                "arbitrary MIME names must pass through the X11 backend unchanged"
            );
        });
        return;
    }

    let mut xvfb = Xvfb(
        Command::new("Xvfb")
            .args([
                DISPLAY,
                "-screen",
                "0",
                "1280x720x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("CI installs Xvfb before running this test"),
    );
    for _ in 0..50 {
        if Path::new("/tmp/.X11-unix/X97").exists() {
            break;
        }
        if let Some(status) = xvfb.0.try_wait().unwrap() {
            panic!("Xvfb exited before the test with {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        Path::new("/tmp/.X11-unix/X97").exists(),
        "Xvfb did not become ready"
    );

    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "headless_x11_round_trip", "--nocapture"])
        .env(CHILD_MARKER, "1")
        .env("DISPLAY", DISPLAY)
        .env_remove("WAYLAND_DISPLAY")
        .status()
        .unwrap();
    assert!(status.success());
}
