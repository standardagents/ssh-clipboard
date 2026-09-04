mod client;
#[cfg(target_os = "macos")]
mod file_conflict;
mod runtime;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, watch};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::clipboard::ClipboardBackend;
use crate::config::{Config, detected_machine_name};
use crate::filebundle;
use crate::model::{Clip, Direction, MonitorEvent};
use crate::protocol::{Message, read_message, write_clip, write_message};
use crate::update::{self, CURRENT_VERSION};

#[cfg(target_os = "macos")]
use file_conflict::{ClaimKind, FileClaim};

pub use client::{bridge, connect_monitor, notify_updates, query_status};
pub use runtime::run;
#[cfg(test)]
use runtime::{handle_local, remove_stale_socket};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerStatus {
    pub node_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub machine_name: Option<String>,
    pub version: Option<String>,
    pub desired_version: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub running: bool,
    pub node_id: Uuid,
    pub node_name: String,
    #[serde(default)]
    pub machine_name: String,
    pub clipboard_backend: String,
    #[serde(default = "legacy_version")]
    pub version: String,
    #[serde(default = "legacy_version")]
    pub desired_version: String,
    #[serde(default)]
    pub configured_peers: Vec<String>,
    pub connected_peers: Vec<String>,
    #[serde(default)]
    pub peers: Vec<PeerStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateNotification {
    pub version: String,
    pub notified_peers: usize,
    pub version_unknown_peers: usize,
}

fn legacy_version() -> String {
    "legacy".to_owned()
}

struct PeerLink {
    node_id: Uuid,
    name: String,
    machine_name: Option<String>,
    version: Option<String>,
    desired_version: Option<String>,
    send: watch::Sender<Option<Arc<Clip>>>,
}

struct Daemon {
    config: Config,
    machine_name: String,
    clipboard: Arc<dyn ClipboardBackend>,
    peers: RwLock<HashMap<Uuid, PeerLink>>,
    seen: Mutex<HashMap<Uuid, Instant>>,
    suppressed: Mutex<HashMap<[u8; 32], usize>>,
    #[cfg(target_os = "macos")]
    file_claim: Mutex<Option<FileClaim>>,
    apply_lock: Mutex<()>,
    events: broadcast::Sender<MonitorEvent>,
    desired_version: watch::Sender<String>,
    update_hints: mpsc::UnboundedSender<String>,
}

impl Daemon {
    #[cfg(test)]
    fn new(config: Config, clipboard: Arc<dyn ClipboardBackend>) -> Arc<Self> {
        let (desired_version, _) = watch::channel(CURRENT_VERSION.to_owned());
        let (update_hints, _) = mpsc::unbounded_channel();
        Self::with_updates(config, clipboard, desired_version, update_hints)
    }

    fn with_updates(
        config: Config,
        clipboard: Arc<dyn ClipboardBackend>,
        desired_version: watch::Sender<String>,
        update_hints: mpsc::UnboundedSender<String>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            machine_name: detected_machine_name(),
            config,
            clipboard,
            peers: RwLock::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
            suppressed: Mutex::new(HashMap::new()),
            #[cfg(target_os = "macos")]
            file_claim: Mutex::new(None),
            apply_lock: Mutex::new(()),
            events,
            desired_version,
            update_hints,
        })
    }

    async fn watch_clipboard(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut previous = self.clipboard.capture().await.ok().flatten();
        let poll = Duration::from_millis(self.config.poll_interval_ms);
        let mut changes = self.clipboard.change_receiver(poll);
        let mut interval = tokio::time::interval(Duration::from_millis(self.config.poll_interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = next_clipboard_change(&mut changes, &mut interval) => {
                    let _guard = self.apply_lock.lock().await;
                    let snapshot = match self.clipboard.capture().await {
                        Ok(Some(snapshot)) => snapshot,
                        Ok(None) => continue,
                        Err(error) => {
                            debug!(%error, "clipboard capture skipped");
                            continue;
                        }
                    };
                    if previous.as_ref().is_some_and(|last| last.fingerprint == snapshot.fingerprint) {
                        continue;
                    }
                    previous = Some(snapshot.clone());
                    let mut suppressed = self.suppressed.lock().await;
                    if let Some(count) = suppressed.get_mut(&snapshot.fingerprint) {
                        *count -= 1;
                        if *count == 0 {
                            suppressed.remove(&snapshot.fingerprint);
                        }
                        continue;
                    }
                    drop(suppressed);
                    #[cfg(target_os = "macos")]
                    if let Some((kind, claimed)) = self.matching_file_claim(&snapshot.representations).await {
                        if kind == ClaimKind::Received {
                            match self.clipboard.apply(&claimed.representations).await {
                                Ok(restored) => self.suppress_fingerprint(restored.fingerprint).await,
                                Err(error) => warn!(%error, clip = %claimed.id, "failed to restore native file clipboard"),
                            }
                        }
                        continue;
                    }
                    let clip = Arc::new(Clip::new(self.config.node_id, snapshot.representations));
                    #[cfg(target_os = "macos")]
                    if file_conflict::has_file_bundle(&clip.representations) {
                        self.remember_file_clip(Arc::clone(&clip), ClaimKind::Local).await;
                    } else {
                        self.file_claim.lock().await.take();
                    }
                    self.mark_seen(clip.id).await;
                    self.emit(Direction::Local, None, &clip);
                    self.broadcast_clip(clip, None).await;
                }
                result = shutdown.changed() => {
                    if result.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
            }
        }
    }

    async fn serve_peer<R, W>(
        self: Arc<Self>,
        reader: &mut R,
        writer: &mut W,
        label: &str,
        shutdown: watch::Receiver<bool>,
        established: Option<&AtomicBool>,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let desired_version = self.desired_version.borrow().clone();
        write_message(
            writer,
            &Message::Hello {
                node_id: self.config.node_id,
                node_name: self.config.node_name.clone(),
                machine_name: Some(self.machine_name.clone()),
                app_version: Some(CURRENT_VERSION.to_owned()),
                desired_version: Some(desired_version),
            },
            self.config.max_bytes,
        )
        .await?;
        let Message::Hello {
            node_id,
            node_name,
            machine_name,
            app_version,
            desired_version,
        } = read_message(reader, self.config.max_bytes).await?
        else {
            bail!("peer did not begin with a hello message");
        };
        if let Some(version) = desired_version.as_deref()
            && update::newer_version(CURRENT_VERSION, version)
        {
            let _ = self.update_hints.send(version.to_owned());
        }
        let connection_id = Uuid::new_v4();
        let (sender, mut receiver) = watch::channel(None);
        let mut desired_updates = self.desired_version.subscribe();
        self.peers.write().await.insert(
            connection_id,
            PeerLink {
                node_id,
                name: node_name.clone(),
                machine_name: machine_name.clone(),
                version: app_version.clone(),
                desired_version: desired_version.clone(),
                send: sender,
            },
        );
        info!(peer = %node_name, %node_id, version = ?app_version, %label, "peer connected");
        if let Some(established) = established {
            established.store(true, Ordering::Release);
        }

        // Keep the read half active while a large clipboard payload is being
        // written. Clipboard bridges can make both peers publish at the same
        // instant; serializing reads and writes would then fill both SSH pipe
        // buffers and deadlock the connection.
        let mut read_shutdown = shutdown.clone();
        let mut write_shutdown = shutdown;
        let read_loop = async {
            loop {
                tokio::select! {
                    incoming = read_message(reader, self.config.max_bytes) => {
                        match incoming {
                            Ok(Message::Clip(clip)) => self.receive_clip(Arc::new(clip), &node_name, connection_id).await,
                            Ok(Message::Hello { .. }) => {
                                break Err(anyhow::anyhow!("peer sent a second hello"));
                            }
                            Ok(Message::UpdateAvailable { version, .. }) => {
                                if let Some(peer) = self.peers.write().await.get_mut(&connection_id) {
                                    peer.desired_version = Some(version.clone());
                                }
                                if update::newer_version(CURRENT_VERSION, &version) {
                                    let _ = self.update_hints.send(version);
                                }
                            }
                            Err(error) => break Err(error.into()),
                        }
                    }
                    changed = read_shutdown.changed() => {
                        if changed.is_err() || *read_shutdown.borrow() {
                            break Ok(());
                        }
                    }
                }
            }
        };
        let write_loop = async {
            loop {
                tokio::select! {
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            break Ok(());
                        }
                        let clip = receiver.borrow_and_update().clone();
                        if let Some(clip) = clip {
                            if let Err(error) = write_clip(writer, &clip, self.config.max_bytes).await {
                                break Err(error.into());
                            }
                            self.emit(Direction::Send, Some(node_name.clone()), &clip);
                        }
                    }
                    changed = desired_updates.changed(), if app_version.is_some() => {
                        if changed.is_err() {
                            break Ok(());
                        }
                        let version = desired_updates.borrow_and_update().clone();
                        if let Err(error) = write_message(
                            writer,
                            &Message::UpdateAvailable {
                                update_id: Uuid::new_v4(),
                                version,
                            },
                            self.config.max_bytes,
                        ).await {
                            break Err(error.into());
                        }
                    }
                    changed = write_shutdown.changed() => {
                        if changed.is_err() || *write_shutdown.borrow() {
                            break Ok(());
                        }
                    }
                }
            }
        };
        let result = tokio::select! {
            result = read_loop => result,
            result = write_loop => result,
        };
        self.peers.write().await.remove(&connection_id);
        info!(peer = %node_name, "peer disconnected");
        result
    }

    async fn receive_clip(&self, clip: Arc<Clip>, peer_name: &str, source: Uuid) {
        if !self.mark_seen(clip.id).await {
            return;
        }
        self.emit(Direction::Receive, Some(peer_name.to_owned()), &clip);
        #[cfg(target_os = "macos")]
        if self.matching_file_claim(&clip.representations).await.is_some() {
            debug!(peer = %peer_name, clip = %clip.id, "ignored lossy file clipboard echo");
            return;
        }
        self.broadcast_clip(Arc::clone(&clip), Some(source)).await;
        let _guard = self.apply_lock.lock().await;
        #[cfg(target_os = "macos")]
        let received_file = file_conflict::has_file_bundle(&clip.representations);
        let representations = match filebundle::materialize(clip.id, &clip.representations) {
            Ok(representations) => representations,
            Err(error) => {
                warn!(%error, peer = %peer_name, clip = %clip.id, "failed to materialize copied files");
                return;
            }
        };
        match self.clipboard.apply(&representations).await {
            Ok(snapshot) => {
                let fingerprint = snapshot.fingerprint;
                #[cfg(target_os = "macos")]
                if received_file {
                    let claimed = Arc::new(Clip {
                        id: clip.id,
                        origin: clip.origin,
                        created_millis: clip.created_millis,
                        representations: snapshot.representations,
                    });
                    self.remember_file_clip(claimed, ClaimKind::Received).await;
                } else {
                    self.file_claim.lock().await.take();
                }
                self.suppress_fingerprint(fingerprint).await;
            }
            Err(error) => warn!(%error, peer = %peer_name, clip = %clip.id, "failed to apply clipboard"),
        }
    }

    async fn suppress_fingerprint(&self, fingerprint: [u8; 32]) {
        *self.suppressed.lock().await.entry(fingerprint).or_default() += 1;
    }

    #[cfg(target_os = "macos")]
    async fn remember_file_clip(&self, clip: Arc<Clip>, kind: ClaimKind) {
        self.file_claim.lock().await.replace(FileClaim::new(clip, kind));
    }

    #[cfg(target_os = "macos")]
    async fn matching_file_claim(
        &self,
        representations: &[crate::model::Representation],
    ) -> Option<(ClaimKind, Arc<Clip>)> {
        let mut claim = self.file_claim.lock().await;
        let current = claim.as_ref()?;
        if current.expired() {
            claim.take();
            return None;
        }
        current
            .matches_lossy_echo(representations)
            .then(|| (current.kind(), current.clip()))
    }

    async fn broadcast_clip(&self, clip: Arc<Clip>, except: Option<Uuid>) {
        let peers = self.peers.read().await;
        for (id, peer) in peers.iter() {
            if Some(*id) != except {
                peer.send.send_replace(Some(Arc::clone(&clip)));
            }
        }
    }

    async fn mark_seen(&self, id: Uuid) -> bool {
        let mut seen = self.seen.lock().await;
        if seen.contains_key(&id) {
            return false;
        }
        seen.insert(id, Instant::now());
        if seen.len() > 4096 {
            let cutoff = Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or_else(Instant::now);
            seen.retain(|_, instant| *instant >= cutoff);
        }
        true
    }

    fn emit(&self, direction: Direction, peer: Option<String>, clip: &Clip) {
        let _ = self.events.send(MonitorEvent::from_clip(direction, peer, clip));
    }

    async fn status(&self) -> Status {
        let peers = self.peers.read().await;
        let mut configured_peers = self
            .config
            .peers
            .iter()
            .map(|peer| peer.name.clone())
            .collect::<Vec<_>>();
        configured_peers.sort();
        configured_peers.dedup();
        let mut connected_peers = peers.values().map(|peer| peer.name.clone()).collect::<Vec<_>>();
        connected_peers.sort();
        connected_peers.dedup();
        let mut peer_statuses = peers
            .values()
            .map(|peer| PeerStatus {
                node_id: peer.node_id,
                name: peer.name.clone(),
                machine_name: peer.machine_name.clone(),
                version: peer.version.clone(),
                desired_version: peer.desired_version.clone(),
            })
            .collect::<Vec<_>>();
        peer_statuses.sort_by(|left, right| left.name.cmp(&right.name));
        peer_statuses.dedup_by(|left, right| left.node_id == right.node_id);
        Status {
            running: true,
            node_id: self.config.node_id,
            node_name: self.config.node_name.clone(),
            machine_name: self.machine_name.clone(),
            clipboard_backend: self.clipboard.name().to_owned(),
            version: CURRENT_VERSION.to_owned(),
            desired_version: self.desired_version.borrow().clone(),
            configured_peers,
            connected_peers,
            peers: peer_statuses,
        }
    }

    async fn notify_updates(&self) -> UpdateNotification {
        let peers = self.peers.read().await;
        let notified_peers = peers.values().filter(|peer| peer.version.is_some()).count();
        let version_unknown_peers = peers.len().saturating_sub(notified_peers);
        drop(peers);

        let version = self.desired_version.borrow().clone();
        self.desired_version.send_replace(version.clone());
        let _ = self.update_hints.send(version.clone());
        UpdateNotification {
            version,
            notified_peers,
            version_unknown_peers,
        }
    }
}

async fn next_clipboard_change(
    changes: &mut Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    interval: &mut tokio::time::Interval,
) {
    if let Some(receiver) = changes {
        if receiver.recv().await.is_some() {
            return;
        }
        *changes = None;
    }
    interval.tick().await;
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex, split};
    use tokio::net::UnixStream;
    use tokio::time::timeout;

    use super::*;
    use crate::clipboard::test_support::MockClipboard;
    use crate::model::Representation;

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
}
