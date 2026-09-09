use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ssh_clipboard::config::{Config, paths};
use ssh_clipboard::daemon;
use ssh_clipboard::model::{Direction, MonitorEvent, human_bytes};
use ssh_clipboard::{deploy, service, tui, update};
use tokio::io::AsyncBufReadExt;

#[derive(Parser)]
#[command(
    name = "ssh-clipboard",
    version,
    about = "Native clipboard sync over encrypted SSH",
    long_about = "Copy normally on one macOS or Linux machine and paste on another. Original clipboard formats travel over persistent passwordless SSH connections."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Configure, verify, and install peers
    Setup,
    /// Watch clipboard values and peer health
    Monitor {
        /// Stream readable lines instead of the TUI
        #[arg(long, conflicts_with = "json")]
        plain: bool,
        /// Stream newline-delimited JSON instead of the TUI
        #[arg(long)]
        json: bool,
    },
    /// Show daemon and connection status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Check for and install the latest stable release
    Update {
        /// Report versions without installing
        #[arg(long)]
        check: bool,
    },
    /// Manage the per-user background service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    #[command(hide = true)]
    Daemon,
    #[command(hide = true)]
    ServiceSupervisor,
    #[command(hide = true)]
    Bridge,
    #[command(hide = true)]
    UpdateWatchdog { version: String },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install {
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Manage a private Xvfb display for a headless Linux machine
        #[arg(long, conflicts_with = "native_display")]
        headless_x11: bool,
        /// Stop using the managed Xvfb display
        #[arg(long, conflicts_with = "headless_x11")]
        native_display: bool,
    },
    Start,
    Stop,
    Restart,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("ssh-clipboard: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            let config_path = paths()?.config_file;
            if config_path.exists() {
                tui::run_monitor(Config::load()?).await
            } else {
                tui::run_setup(Config::default()).await
            }
        }
        Some(Command::Setup) => {
            let config = if paths()?.config_file.exists() {
                Config::load()?
            } else {
                Config::default()
            };
            tui::run_setup(config).await
        }
        Some(Command::Monitor { plain, json }) => {
            if plain || json {
                stream_monitor(json).await
            } else {
                tui::run_monitor(Config::load()?).await
            }
        }
        Some(Command::Status { json }) => {
            let status = daemon::query_status().await.context("daemon is not running")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "running as {} ({}, {}, version {})",
                    if status.machine_name.is_empty() {
                        &status.node_name
                    } else {
                        &status.machine_name
                    },
                    status.node_id,
                    status.clipboard_backend,
                    status.version
                );
                if status.desired_version != status.version {
                    println!("updating to: {}", status.desired_version);
                }
                if status.connected_peers.is_empty() {
                    println!("no peers connected");
                } else {
                    for peer in status.connected_peers {
                        println!("connected: {peer}");
                    }
                }
                for peer in status.peers {
                    let version = peer
                        .version
                        .as_deref()
                        .filter(|version| *version != "legacy" && !version.is_empty())
                        .unwrap_or("unknown");
                    println!("peer version: {} {}", peer.name, version);
                }
            }
            Ok(())
        }
        Some(Command::Update { check }) => {
            if check {
                let latest = update::latest_version().await?;
                println!("current: {}", update::CURRENT_VERSION);
                println!("latest:  {latest}");
                return Ok(());
            }
            if let Some(version) = update::update_now().await? {
                println!("installed {version}; reconciling service");
            } else {
                println!("no newer stable release than {}", update::CURRENT_VERSION);
            }
            let outcome = deploy::install_local_service().await?;
            println!("{}", outcome.detail());
            Ok(())
        }
        Some(Command::Service { action }) => match action {
            ServiceAction::Install {
                binary,
                headless_x11,
                native_display,
            } => {
                let binary = match binary {
                    Some(binary) => binary,
                    None => std::env::current_exe()?,
                };
                let binary = binary.canonicalize().context("resolve service binary")?;
                let mut config = if paths()?.config_file.exists() {
                    Config::load()?
                } else {
                    Config::default()
                };
                if headless_x11 || native_display {
                    config.headless_x11 = headless_x11;
                    config.save()?;
                }
                let outcome = service::install(
                    &binary,
                    service::InstallOptions {
                        headless_x11: config.headless_x11,
                        reset_display: native_display,
                    },
                )
                .await?;
                println!("{}", outcome.detail());
                Ok(())
            }
            ServiceAction::Start => service::control(service::Action::Start).await,
            ServiceAction::Stop => service::control(service::Action::Stop).await,
            ServiceAction::Restart => service::control(service::Action::Restart).await,
        },
        Some(Command::Daemon) => {
            service::container::prepare_daemon()?;
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "ssh_clipboard=info".into()),
                )
                .with_writer(std::io::stderr)
                .init();
            daemon::run(Config::load()?).await
        }
        Some(Command::Bridge) => {
            // Tokio stdin uses an uncancellable blocking read. Returning through
            // #[tokio::main] can keep an idle SSH bridge alive after its daemon
            // disconnects. This dedicated pipe process must close SSH promptly.
            let code = match daemon::bridge().await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("ssh-clipboard: {error:#}");
                    1
                }
            };
            std::process::exit(code);
        }
        Some(Command::ServiceSupervisor) => service::container::supervise().await,
        Some(Command::UpdateWatchdog { version }) => update::watchdog(&version).await,
    }
}

async fn stream_monitor(json: bool) -> Result<()> {
    let mut reader = daemon::connect_monitor().await.context("daemon is not running")?;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            bail!("daemon monitor stream closed");
        }
        let event: MonitorEvent = serde_json::from_str(&line)?;
        if json {
            print!("{line}");
            continue;
        }
        let flow = match event.direction {
            Direction::Local => "local".to_owned(),
            Direction::Send => format!("send → {}", event.peer.as_deref().unwrap_or("peer")),
            Direction::Receive => format!("recv ← {}", event.peer.as_deref().unwrap_or("peer")),
        };
        println!(
            "{}  {:<24} {:<10} {:>2} formats  {}",
            format_time(event.timestamp_millis),
            flow,
            human_bytes(event.total_bytes()),
            event.representations.len(),
            event.preview
        );
    }
}

fn format_time(timestamp_millis: u64) -> String {
    let day = timestamp_millis % 86_400_000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        day / 3_600_000,
        (day / 60_000) % 60,
        (day / 1_000) % 60,
        day % 1_000
    )
}
