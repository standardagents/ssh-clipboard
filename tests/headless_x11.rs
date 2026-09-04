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
                .apply(&[Representation {
                    item: 0,
                    format: "text/plain".into(),
                    data: b"headless clipboard smoke test".to_vec(),
                }])
                .await
                .unwrap();
            let snapshot = clipboard.capture().await.unwrap().unwrap();
            assert!(
                snapshot
                    .representations
                    .iter()
                    .any(|representation| { representation.data == b"headless clipboard smoke test" })
            );
            // Exercise the real X11 owner, not just a MIME serialization unit
            // test. Finder bundles must become local, encoded file references.
            let source = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
            let paths = ["Manual #1.pdf", "Disk.dmg", "Installer.pkg"].map(|name| source.path().join(name));
            for path in &paths {
                std::fs::write(path, [0, 1, 2, 255]).unwrap();
            }
            let mut files = vec![Representation {
                item: 0,
                format: "text/uri-list".into(),
                data: paths
                    .iter()
                    .map(|path| ssh_clipboard::filebundle::path_to_uri(path))
                    .collect::<Vec<_>>()
                    .join("\r\n")
                    .into_bytes(),
            }];
            ssh_clipboard::filebundle::attach_bundle(&mut files, 1_000_000).unwrap();
            // Materialization uses a process-local temporary state directory.
            // The child environment below isolates this from real user data.
            let local_files = ssh_clipboard::filebundle::materialize(uuid::Uuid::new_v4(), &files).unwrap();
            clipboard.apply(&local_files).await.unwrap();
            let captured = clipboard.capture().await.unwrap().unwrap();
            let uri_list = captured
                .representations
                .iter()
                .find(|r| r.format == "text/uri-list")
                .unwrap();
            let received = ssh_clipboard::filebundle::parse_uri_list(&uri_list.data);
            assert_eq!(received.len(), paths.len());
            for (received, original) in received.iter().zip(&paths) {
                assert_ne!(received, original);
                // Simulate the file manager's copy operation from its local URL.
                let pasted = target.path().join(received.file_name().unwrap());
                std::fs::copy(received, &pasted).unwrap();
                assert_eq!(std::fs::read(pasted).unwrap(), [0, 1, 2, 255]);
            }
            let gnome = captured
                .representations
                .iter()
                .find(|r| r.format == "x-special/gnome-copied-files")
                .unwrap();
            assert!(gnome.data.starts_with(b"copy\nfile://"));
            assert!(String::from_utf8_lossy(&uri_list.data).contains("Manual%20%231.pdf"));
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

    let state = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "headless_x11_round_trip", "--nocapture"])
        .env(CHILD_MARKER, "1")
        .env("DISPLAY", DISPLAY)
        .env("XDG_STATE_HOME", state.path())
        .env_remove("WAYLAND_DISPLAY")
        .status()
        .unwrap();
    assert!(status.success());
}
