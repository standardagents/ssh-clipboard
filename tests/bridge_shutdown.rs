use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn idle_bridge_exits_when_daemon_disconnects_with_stdin_still_open() {
    let directory = tempfile::Builder::new().prefix("sc-").tempdir_in("/tmp").unwrap();
    let listener = UnixListener::bind(directory.path().join("daemon.sock")).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(stream);
        let mut command = String::new();
        reader.read_line(&mut command).unwrap();
        assert_eq!(command, "BRIDGE\n");
        // Let Tokio's blocking stdin reader start before the daemon disconnects.
        std::thread::sleep(Duration::from_millis(200));
    });
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_ssh-clipboard"))
        .arg("bridge")
        .env("SSH_CLIPBOARD_STATE_DIR", directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    // Keep this pipe open and empty, just like an idle persistent SSH connection.
    let _stdin = bridge.stdin.take().unwrap();
    server.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = bridge.try_wait().unwrap() {
            assert!(status.success());
            return;
        }
        if Instant::now() >= deadline {
            bridge.kill().unwrap();
            bridge.wait().unwrap();
            panic!("bridge hung after daemon disconnected while SSH stdin remained open");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
