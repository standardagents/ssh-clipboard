mod capture_retry;
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
        let mut retry = capture_retry::CaptureRetry::default();
        let mut previous = match self.clipboard.capture().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(error = %format!("{error:#}"), "initial clipboard capture failed");
                retry.failed();
                None
            }
        };
        let poll = Duration::from_millis(self.config.poll_interval_ms);
        let mut changes = self.clipboard.change_receiver(poll);
        let mut interval = tokio::time::interval(Duration::from_millis(self.config.poll_interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = retry.next(&mut changes, &mut interval) => {
                    let _guard = self.apply_lock.lock().await;
                    let snapshot = match self.clipboard.capture().await {
                        Ok(Some(snapshot)) => { retry.reset(); snapshot },
                        Ok(None) => { retry.reset(); continue; },
                        Err(error) => {
                            warn!(error = %format!("{error:#}"), "clipboard capture failed");
                            retry.failed();
                            continue;
                        }
                    };
                    #[cfg(target_os = "macos")]
                    if let Some((kind, claimed)) = self.matching_file_claim(&snapshot.representations).await {
                        if kind == ClaimKind::Received {
                            match self.clipboard.apply(&claimed.representations).await {
                                Ok(restored) => previous = Some(restored),
                                Err(error) => warn!(%error, clip = %claimed.id, "failed to restore native file clipboard"),
                            }
                        }
                        continue;
                    }
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
                    self.file_claim.lock().await.replace(
                        FileClaim::new(Arc::clone(&clip), ClaimKind::Received).with_restore(claimed),
                    );
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
mod tests;
