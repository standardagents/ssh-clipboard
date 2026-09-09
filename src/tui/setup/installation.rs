use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{Config, PeerConfig};
use crate::deploy;
use crate::{daemon, service};

use super::{UiMessage, VerifiedPeer};

pub(super) async fn install_all(
    config: Config,
    peers: Vec<VerifiedPeer>,
    sender: Sender<UiMessage>,
) -> Result<()> {
    // A healthy daemon may still be running the configuration from before setup.
    // Remember it before saving so adding/replacing peers takes effect immediately.
    let was_running = matches!(
        tokio::time::timeout(Duration::from_secs(2), daemon::query_status()).await,
        Ok(Ok(_))
    );
    let mut local = config;
    for peer in &peers {
        merge_peer(
            &mut local.peers,
            PeerConfig {
                name: peer.probe.hostname.clone(),
                ssh_command: peer.command.clone(),
            },
        );
    }
    local.save()?;

    for peer in &peers {
        let name = peer.probe.hostname.clone();
        let progress_sender = sender.clone();
        let outcome = deploy::install_remote(&peer.command, &peer.probe, peer.headless_x11, |_, detail| {
            let _ = progress_sender.send(UiMessage::Progress {
                peer: name.clone(),
                detail: detail.to_owned(),
                complete: false,
            });
        })
        .await
        .with_context(|| format!("install {name}"))?;
        let _ = sender.send(UiMessage::Progress {
            peer: name,
            detail: outcome.detail().into(),
            complete: true,
        });
    }

    let _ = sender.send(UiMessage::Progress {
        peer: local.node_name.clone(),
        detail: "Installing this machine’s service".into(),
        complete: false,
    });
    let outcome = deploy::install_local_service().await?;
    if was_running && outcome == service::InstallOutcome::Running {
        service::control(service::Action::Restart).await?;
        wait_for_configured_peers(&local).await?;
    }
    let _ = sender.send(UiMessage::Progress {
        peer: local.node_name,
        detail: outcome.detail().into(),
        complete: true,
    });
    Ok(())
}

async fn wait_for_configured_peers(config: &Config) -> Result<()> {
    let expected = configured_peer_names(config);
    for _ in 0..100 {
        if let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(1), daemon::query_status()).await
            && status.configured_peers == expected
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("service did not load the saved peers after restart; inspect the daemon log")
}

fn configured_peer_names(config: &Config) -> Vec<String> {
    // Status returns sorted, unique names, not configuration insertion order.
    let mut names: Vec<_> = config.peers.iter().map(|peer| peer.name.clone()).collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_readiness_uses_status_order_for_multiple_peers() {
        let config = Config {
            peers: ["z-mac", "a-linux", "z-mac"]
                .into_iter()
                .map(|name| PeerConfig {
                    name: name.into(),
                    ssh_command: format!("ssh {name}"),
                })
                .collect(),
            ..Config::default()
        };
        assert_eq!(configured_peer_names(&config), ["a-linux", "z-mac"]);
    }
}

pub(super) fn merge_peer(peers: &mut Vec<PeerConfig>, configured: PeerConfig) {
    if let Some(existing) = peers
        .iter_mut()
        .find(|existing| existing.ssh_command == configured.ssh_command || existing.name == configured.name)
    {
        *existing = configured;
    } else {
        peers.push(configured);
    }
}
