use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex, split};
use tokio::net::UnixStream;
use tokio::time::timeout;

use super::*;
use crate::clipboard::test_support::MockClipboard;
use crate::model::Representation;

#[tokio::test]
async fn retries_a_failed_file_capture_without_another_copy_notification() {
    use crate::clipboard::Snapshot;
    use std::sync::atomic::AtomicUsize;

    struct TransientClipboard {
        calls: AtomicUsize,
        receiver: std::sync::Mutex<Option<mpsc::UnboundedReceiver<()>>>,
    }
    #[async_trait::async_trait]
    impl ClipboardBackend for TransientClipboard {
        async fn capture(&self) -> Result<Option<Snapshot>> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(None),
                1 => bail!("temporary file-provider read failure"),
                _ => Ok(Some(Snapshot::new(vec![Representation {
                    item: 0,
                    format: "text/plain".into(),
                    data: b"retried".to_vec(),
                }]))),
            }
        }
        async fn apply(&self, _: &[Representation]) -> Result<Snapshot> {
            unreachable!()
        }
        fn name(&self) -> &'static str {
            "transient-test"
        }
        fn change_receiver(&self, _: Duration) -> Option<mpsc::UnboundedReceiver<()>> {
            self.receiver.lock().unwrap().take()
        }
    }
    let (change_tx, change_rx) = mpsc::unbounded_channel();
    let clipboard = Arc::new(TransientClipboard {
        calls: AtomicUsize::new(0),
        receiver: std::sync::Mutex::new(Some(change_rx)),
    });
    let daemon = Daemon::new(Config::default(), clipboard);
    let mut events = daemon.events.subscribe();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let watcher = tokio::spawn(daemon.watch_clipboard(shutdown_rx));
    change_tx.send(()).unwrap();
    let event = timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.direction, Direction::Local);
    assert_eq!(event.preview, "retried");
    shutdown_tx.send(true).unwrap();
    watcher.await.unwrap();
}

#[test]
fn status_from_an_older_daemon_defaults_version_fields() {
    let status: Status = serde_json::from_value(serde_json::json!({
        "running": true,
        "node_id": Uuid::new_v4(),
        "node_name": "older-mac",
        "clipboard_backend": "NSPasteboard",
        "connected_peers": []
    }))
    .unwrap();

    assert_eq!(status.version, "legacy");
    assert_eq!(status.desired_version, "legacy");
    assert!(status.machine_name.is_empty());
    assert!(status.configured_peers.is_empty());
    assert!(status.peers.is_empty());
}

#[tokio::test]
async fn bridge_command_preserves_a_coalesced_peer_hello() {
    let config = Config::default();
    let max_bytes = config.max_bytes;
    let clipboard = Arc::new(MockClipboard::default());
    let daemon = Daemon::new(config, clipboard);
    let (mut client, server) = UnixStream::pair().unwrap();
    client.write_all(b"BRIDGE\n").await.unwrap();
    write_message(
        &mut client,
        &Message::Hello {
            node_id: Uuid::new_v4(),
            node_name: "remote-mac".into(),
            machine_name: Some("remote-mac.local".into()),
            app_version: Some(CURRENT_VERSION.into()),
            desired_version: Some(CURRENT_VERSION.into()),
        },
        max_bytes,
    )
    .await
    .unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(handle_local(Arc::clone(&daemon), server, shutdown_rx));
    assert!(matches!(
        read_message(&mut client, max_bytes).await.unwrap(),
        Message::Hello { .. }
    ));
    timeout(Duration::from_secs(1), async {
        loop {
            if daemon.status().await.connected_peers == ["remote-mac"] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn local_update_command_queues_a_check_and_reports_peer_counts() {
    let config = Config::default();
    let clipboard = Arc::new(MockClipboard::default());
    let (desired_tx, _) = watch::channel(CURRENT_VERSION.to_owned());
    let (hint_tx, mut hint_rx) = mpsc::unbounded_channel();
    let daemon = Daemon::with_updates(config, clipboard, desired_tx, hint_tx);
    let (mut client, server) = UnixStream::pair().unwrap();
    client.write_all(b"NOTIFY_UPDATE\n").await.unwrap();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    handle_local(daemon, server, shutdown_rx).await.unwrap();

    let mut line = String::new();
    BufReader::new(client).read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<UpdateNotification>(&line).unwrap(),
        UpdateNotification {
            version: CURRENT_VERSION.into(),
            notified_peers: 0,
            version_unknown_peers: 0,
        }
    );
    assert_eq!(hint_rx.recv().await, Some(CURRENT_VERSION.into()));
}

#[tokio::test]
async fn update_versions_are_announced_and_peer_hints_are_forwarded() {
    let config = Config::default();
    let max_bytes = config.max_bytes;
    let clipboard = Arc::new(MockClipboard::default());
    let (desired_tx, _) = watch::channel(CURRENT_VERSION.to_owned());
    let (hint_tx, mut hint_rx) = mpsc::unbounded_channel();
    let daemon = Daemon::with_updates(config, clipboard, desired_tx.clone(), hint_tx);
    let (mut peer, server) = duplex(4096);
    let (mut reader, mut writer) = split(server);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let serving_daemon = Arc::clone(&daemon);
    let task = tokio::spawn(async move {
        serving_daemon
            .serve_peer(&mut reader, &mut writer, "test", shutdown_rx, None)
            .await
    });
    assert!(matches!(
        read_message(&mut peer, max_bytes).await.unwrap(),
        Message::Hello { .. }
    ));
    write_message(
        &mut peer,
        &Message::Hello {
            node_id: Uuid::new_v4(),
            node_name: "peer".into(),
            machine_name: Some("peer.local".into()),
            app_version: Some(CURRENT_VERSION.into()),
            desired_version: Some(CURRENT_VERSION.into()),
        },
        max_bytes,
    )
    .await
    .unwrap();

    timeout(Duration::from_secs(1), async {
        while desired_tx.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let notification = daemon.notify_updates().await;
    assert_eq!(
        notification,
        UpdateNotification {
            version: CURRENT_VERSION.into(),
            notified_peers: 1,
            version_unknown_peers: 0,
        }
    );
    assert!(matches!(
        read_message(&mut peer, max_bytes).await.unwrap(),
        Message::UpdateAvailable { version, .. } if version == CURRENT_VERSION
    ));
    assert_eq!(
        timeout(Duration::from_secs(1), hint_rx.recv()).await.unwrap(),
        Some(CURRENT_VERSION.into())
    );

    desired_tx.send("9.0.0".into()).unwrap();
    assert!(matches!(
        read_message(&mut peer, max_bytes).await.unwrap(),
        Message::UpdateAvailable { version, .. } if version == "9.0.0"
    ));
    write_message(
        &mut peer,
        &Message::UpdateAvailable {
            update_id: Uuid::new_v4(),
            version: "9.1.0".into(),
        },
        max_bytes,
    )
    .await
    .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), hint_rx.recv()).await.unwrap(),
        Some("9.1.0".into())
    );

    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn relays_a_clip_between_peers_and_applies_it_locally() {
    let config = Config::default();
    let clipboard = Arc::new(MockClipboard::default());
    let daemon = Daemon::new(config.clone(), clipboard.clone());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let (peer_a, server_a) = duplex(16 * 1024);
    let (peer_b, server_b) = duplex(16 * 1024);
    let (mut server_a_read, mut server_a_write) = split(server_a);
    let (mut server_b_read, mut server_b_write) = split(server_b);
    let daemon_a = Arc::clone(&daemon);
    let shutdown_a = shutdown_rx.clone();
    tokio::spawn(async move {
        daemon_a
            .serve_peer(&mut server_a_read, &mut server_a_write, "a", shutdown_a, None)
            .await
    });
    let daemon_b = Arc::clone(&daemon);
    tokio::spawn(async move {
        daemon_b
            .serve_peer(&mut server_b_read, &mut server_b_write, "b", shutdown_rx, None)
            .await
    });
    let (mut a_read, mut a_write) = split(peer_a);
    let (mut b_read, mut b_write) = split(peer_b);
    for (reader, writer, name) in [
        (&mut a_read, &mut a_write, "peer-a"),
        (&mut b_read, &mut b_write, "peer-b"),
    ] {
        assert!(matches!(
            read_message(reader, config.max_bytes).await.unwrap(),
            Message::Hello { .. }
        ));
        write_message(
            writer,
            &Message::Hello {
                node_id: Uuid::new_v4(),
                node_name: name.into(),
                machine_name: Some(format!("{name}.local")),
                app_version: Some(CURRENT_VERSION.into()),
                desired_version: Some(CURRENT_VERSION.into()),
            },
            config.max_bytes,
        )
        .await
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    let clip = Clip::new(
        Uuid::new_v4(),
        vec![Representation {
            item: 0,
            format: "image/heic".into(),
            data: vec![1, 2, 3, 4],
        }],
    );
    write_clip(&mut a_write, &clip, config.max_bytes).await.unwrap();
    let relayed = timeout(
        Duration::from_secs(1),
        read_message(&mut b_read, config.max_bytes),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(relayed, Message::Clip(clip.clone()));
    let applied = timeout(Duration::from_secs(1), async {
        loop {
            if let Some(snapshot) = clipboard.capture().await.unwrap()
                && snapshot.representations == clip.representations
            {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(applied.representations, clip.representations);
}

#[tokio::test]
async fn simultaneous_large_clip_writes_do_not_deadlock() {
    let config_a = Config {
        node_name: "peer-a".into(),
        ..Config::default()
    };
    let config_b = Config {
        node_name: "peer-b".into(),
        ..Config::default()
    };
    let clipboard_a = Arc::new(MockClipboard::default());
    let clipboard_b = Arc::new(MockClipboard::default());
    let daemon_a = Daemon::new(config_a.clone(), clipboard_a.clone());
    let daemon_b = Daemon::new(config_b.clone(), clipboard_b.clone());
    let (stream_a, stream_b) = duplex(1024);
    let (mut read_a, mut write_a) = split(stream_a);
    let (mut read_b, mut write_b) = split(stream_b);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let serving_a = Arc::clone(&daemon_a);
    let shutdown_a = shutdown_rx.clone();
    let task_a = tokio::spawn(async move {
        serving_a
            .serve_peer(&mut read_a, &mut write_a, "a", shutdown_a, None)
            .await
    });
    let serving_b = Arc::clone(&daemon_b);
    let task_b = tokio::spawn(async move {
        serving_b
            .serve_peer(&mut read_b, &mut write_b, "b", shutdown_rx, None)
            .await
    });

    timeout(Duration::from_secs(1), async {
        loop {
            if daemon_a.status().await.connected_peers == ["peer-b"]
                && daemon_b.status().await.connected_peers == ["peer-a"]
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let clip_a = Arc::new(Clip::new(
        config_a.node_id,
        vec![Representation {
            item: 0,
            format: "application/octet-stream".into(),
            data: vec![0xa5; 128 * 1024],
        }],
    ));
    let clip_b = Arc::new(Clip::new(
        config_b.node_id,
        vec![Representation {
            item: 0,
            format: "application/octet-stream".into(),
            data: vec![0x5a; 128 * 1024],
        }],
    ));
    let expected_a = clip_a.representations.clone();
    let expected_b = clip_b.representations.clone();

    tokio::join!(
        daemon_a.broadcast_clip(clip_a, None),
        daemon_b.broadcast_clip(clip_b, None),
    );

    timeout(Duration::from_secs(2), async {
        loop {
            let received_by_a = clipboard_a
                .capture()
                .await
                .unwrap()
                .is_some_and(|snapshot| snapshot.representations == expected_b);
            let received_by_b = clipboard_b
                .capture()
                .await
                .unwrap()
                .is_some_and(|snapshot| snapshot.representations == expected_a);
            if received_by_a && received_by_b {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    shutdown_tx.send(true).unwrap();
    let _ = task_a.await.unwrap();
    let _ = task_b.await.unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn ignores_a_lossy_peer_echo_of_a_recent_local_file_copy() {
    let config = Config::default();
    let clipboard = Arc::new(MockClipboard::default());
    let daemon = Daemon::new(config.clone(), clipboard.clone());
    let local_file = Arc::new(Clip::new(
        config.node_id,
        vec![
            Representation {
                item: 0,
                format: filebundle::BUNDLE_FORMAT.into(),
                data: b"bundle fixture".to_vec(),
            },
            Representation {
                item: 0,
                format: "public.utf8-plain-text".into(),
                data: b"report.pdf".to_vec(),
            },
        ],
    ));
    daemon.remember_file_clip(local_file, ClaimKind::Local).await;
    let lossy_echo = Arc::new(Clip::new(
        Uuid::new_v4(),
        vec![Representation {
            item: 0,
            format: "NSStringPboardType".into(),
            data: b"report.pdf".to_vec(),
        }],
    ));

    daemon
        .receive_clip(lossy_echo, "screen-sharing-peer", Uuid::new_v4())
        .await;

    assert!(clipboard.capture().await.unwrap().is_none());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn restores_a_received_file_after_a_lossy_local_echo() {
    let config = Config {
        poll_interval_ms: 20,
        ..Config::default()
    };
    let clipboard = Arc::new(MockClipboard::default());
    let native_file = vec![
        Representation {
            item: 0,
            format: "public.file-url".into(),
            data: b"file:///tmp/report.pdf".to_vec(),
        },
        Representation {
            item: 0,
            format: "public.utf8-plain-text".into(),
            data: b"report.pdf".to_vec(),
        },
    ];
    clipboard.replace(native_file.clone()).await;
    let daemon = Daemon::new(config, clipboard.clone());
    daemon
        .remember_file_clip(
            Arc::new(Clip::new(Uuid::new_v4(), native_file.clone())),
            ClaimKind::Received,
        )
        .await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let watcher = tokio::spawn(Arc::clone(&daemon).watch_clipboard(shutdown_rx));
    tokio::time::sleep(Duration::from_millis(10)).await;
    clipboard
        .replace(vec![Representation {
            item: 0,
            format: "NSStringPboardType".into(),
            data: b"report.pdf".to_vec(),
        }])
        .await;

    timeout(Duration::from_secs(1), async {
        loop {
            if clipboard
                .capture()
                .await
                .unwrap()
                .is_some_and(|snapshot| snapshot.representations == native_file)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    shutdown_tx.send(true).unwrap();
    watcher.await.unwrap();
}

#[test]
fn refuses_to_delete_a_non_socket_path() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("daemon.sock");
    std::fs::write(&path, b"important").unwrap();
    assert!(remove_stale_socket(&path).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"important");
}
