mod installation;

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Gauge, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::runtime::Handle;

use crate::config::Config;
#[cfg(test)]
use crate::config::PeerConfig;
use crate::deploy;
use crate::ssh::{self, ProbeResult};
use crate::tailscale;

use super::{ACCENT, CYAN, GREEN, MUTED, PANEL, RED, SOFT, clean_truncate};
use installation::install_all;
#[cfg(test)]
use installation::merge_peer;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Welcome,
    Entry,
    Verifying,
    Confirmed,
    Installing,
    Ready,
    Failed,
}

#[derive(Clone, Debug)]
struct VerifiedPeer {
    command: String,
    probe: ProbeResult,
    installation: deploy::Installation,
    headless_x11: bool,
}

impl VerifiedPeer {
    fn new(command: String, probe: ProbeResult, installation: deploy::Installation) -> Self {
        Self {
            command,
            probe,
            installation,
            headless_x11: false,
        }
    }

    fn headless_capability(&self) -> Option<&crate::ssh::LinuxClipboard> {
        self.probe
            .linux_clipboard
            .as_ref()
            .filter(|clipboard| clipboard.needs_display())
    }
}

#[derive(Clone, Debug)]
struct TailscaleChoice {
    peer: tailscale::Peer,
    selected: bool,
}

enum UiMessage {
    Verified {
        command: String,
        result: Result<(ProbeResult, deploy::Installation), String>,
    },
    TailscaleDiscovered(Vec<tailscale::Peer>),
    TailscaleVerified(Result<Vec<VerifiedPeer>, String>),
    VerificationProgress(String),
    Progress {
        peer: String,
        detail: String,
        complete: bool,
    },
    Installed(Result<(), String>),
}

struct SetupApp {
    handle: Handle,
    config: Config,
    stage: Stage,
    input: String,
    peers: Vec<VerifiedPeer>,
    tailscale_choices: Vec<TailscaleChoice>,
    tailscale_cursor: usize,
    tailscale_detecting: bool,
    verifying: String,
    last_verified: usize,
    active_peer: String,
    detail: String,
    completed: Vec<(String, String)>,
    error: Option<String>,
    receiver: Receiver<UiMessage>,
    sender: Sender<UiMessage>,
    spinner: usize,
    last_tick: Instant,
    quit: bool,
}

impl SetupApp {
    fn new(handle: Handle, config: Config) -> Self {
        let (sender, receiver) = mpsc::channel();
        let discovery_sender = sender.clone();
        handle.spawn(async move {
            let peers = tailscale::discover().await.unwrap_or_default();
            let _ = discovery_sender.send(UiMessage::TailscaleDiscovered(peers));
        });
        Self {
            handle,
            config,
            stage: Stage::Welcome,
            input: String::new(),
            peers: Vec::new(),
            tailscale_choices: Vec::new(),
            tailscale_cursor: 0,
            tailscale_detecting: true,
            verifying: String::new(),
            last_verified: 0,
            active_peer: String::new(),
            detail: String::new(),
            completed: Vec::new(),
            error: None,
            receiver,
            sender,
            spinner: 0,
            last_tick: Instant::now(),
            quit: false,
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            while let Ok(message) = self.receiver.try_recv() {
                self.on_message(message);
            }
            if self.last_tick.elapsed() >= Duration::from_millis(80) {
                self.spinner = (self.spinner + 1) % SPINNER.len();
                self.last_tick = Instant::now();
            }
            terminal.draw(|frame| self.render(frame))?;
            if event::poll(Duration::from_millis(40))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                    Event::Paste(value) if self.stage == Stage::Entry => self.input.push_str(&value),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match self.stage {
            Stage::Welcome => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                    self.stage = Stage::Entry;
                }
            }
            Stage::Entry => self.on_entry_key(key),
            Stage::Confirmed => match key.code {
                KeyCode::Char('a') => {
                    self.input.clear();
                    self.stage = Stage::Entry;
                }
                KeyCode::Enter | KeyCode::Char('i') => self.begin_install(),
                KeyCode::Char('x') => {
                    let enable = self.peers.iter().any(|peer| {
                        peer.headless_capability()
                            .is_some_and(|clipboard| clipboard.xvfb_available)
                            && !peer.headless_x11
                    });
                    for peer in &mut self.peers {
                        if peer
                            .headless_capability()
                            .is_some_and(|clipboard| clipboard.xvfb_available)
                        {
                            peer.headless_x11 = enable;
                        }
                    }
                    self.error = None;
                }
                _ => {}
            },
            Stage::Ready => match key.code {
                KeyCode::Char('a') => {
                    self.input.clear();
                    self.error = None;
                    self.stage = Stage::Entry;
                }
                KeyCode::Enter | KeyCode::Char('q') => self.quit = true,
                _ => {}
            },
            Stage::Failed if key.code == KeyCode::Char('r') => self.begin_install(),
            Stage::Verifying | Stage::Installing | Stage::Failed => {}
        }
    }

    fn on_entry_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if !self.input.trim().is_empty() {
                    let command = ssh::normalize_command(&self.input);
                    self.error = None;
                    self.verifying.clone_from(&command);
                    self.stage = Stage::Verifying;
                    let sender = self.sender.clone();
                    self.handle.spawn(async move {
                        let result = async {
                            let probe = ssh::probe(&command).await?;
                            deploy::validate_target(&probe.os, &probe.arch)?;
                            let installation = deploy::inspect_remote(&command, &probe).await?;
                            Ok((probe, installation))
                        }
                        .await
                        .map_err(|error: anyhow::Error| format!("{error:#}"));
                        let _ = sender.send(UiMessage::Verified { command, result });
                    });
                } else if self.tailscale_choices.iter().any(|choice| choice.selected) {
                    self.begin_tailscale_verification();
                } else if !self.peers.is_empty() {
                    self.begin_install();
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Up if !self.tailscale_choices.is_empty() => {
                self.tailscale_cursor = self.tailscale_cursor.saturating_sub(1);
            }
            KeyCode::Down if !self.tailscale_choices.is_empty() => {
                self.tailscale_cursor = (self.tailscale_cursor + 1).min(self.tailscale_choices.len() - 1);
            }
            KeyCode::Char(' ') if self.input.is_empty() && !self.tailscale_choices.is_empty() => {
                let choice = &mut self.tailscale_choices[self.tailscale_cursor];
                choice.selected = !choice.selected;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.push(character);
            }
            _ => {}
        }
    }

    fn begin_tailscale_verification(&mut self) {
        let selected = self
            .tailscale_choices
            .iter()
            .filter(|choice| choice.selected)
            .map(|choice| choice.peer.clone())
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return;
        }
        self.error = None;
        self.verifying = format!("{} selected Tailscale machine(s)", selected.len());
        self.stage = Stage::Verifying;
        let sender = self.sender.clone();
        self.handle.spawn(async move {
            let result = async {
                let mut verified = Vec::with_capacity(selected.len());
                for peer in selected {
                    let _ = sender.send(UiMessage::VerificationProgress(peer.hostname.clone()));
                    let command = peer.ssh_command();
                    let probe = ssh::probe(&command)
                        .await
                        .with_context(|| format!("verify {}", peer.hostname))?;
                    deploy::validate_target(&probe.os, &probe.arch)
                        .with_context(|| format!("check compatibility for {}", peer.hostname))?;
                    let installation = deploy::inspect_remote(&command, &probe)
                        .await
                        .with_context(|| format!("inspect {}", peer.hostname))?;
                    verified.push(VerifiedPeer::new(command, probe, installation));
                }
                Ok(verified)
            }
            .await
            .map_err(|error: anyhow::Error| format!("{error:#}"));
            let _ = sender.send(UiMessage::TailscaleVerified(result));
        });
    }

    fn begin_install(&mut self) {
        if self.peers.is_empty() {
            return;
        }
        if let Some(peer) = self.peers.iter().find(|peer| {
            peer.headless_capability()
                .is_some_and(|clipboard| !clipboard.xvfb_available)
        }) {
            let clipboard = peer.headless_capability().expect("headless peer");
            self.error = Some(format!(
                "{} has no graphical clipboard. Run `{}` there, then verify it again.",
                peer.probe.hostname,
                clipboard.install_hint()
            ));
            return;
        }
        if let Some(peer) = self
            .peers
            .iter()
            .find(|peer| peer.headless_capability().is_some() && !peer.headless_x11)
        {
            self.error = Some(format!(
                "{} has no graphical clipboard. Press x to enable managed Xvfb.",
                peer.probe.hostname
            ));
            return;
        }
        self.stage = Stage::Installing;
        self.error = None;
        self.completed.clear();
        self.active_peer.clear();
        self.detail = "Preparing private peer configuration".into();
        let sender = self.sender.clone();
        let config = self.config.clone();
        let peers = self.peers.clone();
        self.handle.spawn(async move {
            let result = install_all(config, peers, sender.clone())
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(UiMessage::Installed(result));
        });
    }

    fn on_message(&mut self, message: UiMessage) {
        match message {
            UiMessage::Verified { command, result } => match result {
                Ok((probe, installation)) => {
                    if !self.peers.iter().any(|peer| peer.command == command) {
                        self.peers.push(VerifiedPeer::new(command, probe, installation));
                    }
                    self.last_verified = 1;
                    self.stage = Stage::Confirmed;
                    self.error = None;
                }
                Err(error) => {
                    self.error = Some(error);
                    self.stage = Stage::Entry;
                }
            },
            UiMessage::TailscaleDiscovered(peers) => {
                self.tailscale_detecting = false;
                self.tailscale_choices = peers
                    .into_iter()
                    .map(|peer| TailscaleChoice {
                        peer,
                        selected: false,
                    })
                    .collect();
                self.tailscale_cursor = 0;
            }
            UiMessage::TailscaleVerified(result) => match result {
                Ok(verified) => {
                    self.last_verified = verified.len();
                    for peer in verified {
                        if !self.peers.iter().any(|existing| existing.command == peer.command) {
                            self.peers.push(peer);
                        }
                    }
                    for choice in &mut self.tailscale_choices {
                        choice.selected = false;
                    }
                    self.stage = Stage::Confirmed;
                    self.error = None;
                }
                Err(error) => {
                    self.error = Some(error);
                    self.stage = Stage::Entry;
                }
            },
            UiMessage::VerificationProgress(peer) => {
                self.verifying = format!("{peer} via Tailscale");
            }
            UiMessage::Progress {
                peer,
                detail,
                complete,
            } => {
                if complete && !self.completed.iter().any(|(name, _)| name == &peer) {
                    self.completed.push((peer.clone(), detail.clone()));
                }
                self.active_peer = peer;
                self.detail = detail;
            }
            UiMessage::Installed(result) => match result {
                Ok(()) => self.stage = Stage::Ready,
                Err(error) => {
                    self.error = Some(error);
                    self.stage = Stage::Failed;
                }
            },
        }
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let [header, steps, body] =
            Layout::vertical([Constraint::Length(3), Constraint::Length(2), Constraint::Min(1)]).areas(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("ssh", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(" ◇ ", Style::new().fg(SOFT)),
                Span::styled("clipboard", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            ]))
            .alignment(Alignment::Center),
            header,
        );
        frame.render_widget(self.steps(), steps);
        let panel_height = self.preferred_panel_height().min(body.height.saturating_sub(2));
        let [panel_row, help_row, _] = Layout::vertical([
            Constraint::Length(panel_height),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(body);
        let body_width = area.width.saturating_sub(6).min(92);
        let [body_area] = Layout::horizontal([Constraint::Length(body_width)])
            .flex(Flex::Center)
            .areas(panel_row);
        self.render_body(frame, body_area);
        frame.render_widget(Paragraph::new(self.help()).alignment(Alignment::Center), help_row);
    }

    fn preferred_panel_height(&self) -> u16 {
        match self.stage {
            Stage::Entry if self.tailscale_choices.is_empty() => {
                11 + u16::from(self.error.is_some()) * 2 + u16::from(!self.peers.is_empty()) * 2
            }
            Stage::Entry => {
                13 + u16::try_from(self.tailscale_choices.len().min(4)).expect("choice count is at most four")
                    + u16::from(self.error.is_some()) * 2
                    + u16::from(!self.peers.is_empty()) * 2
            }
            Stage::Confirmed => 12_u16
                .saturating_add(u16::try_from(self.confirmed_headless_lines().len()).unwrap_or(u16::MAX)),
            Stage::Welcome | Stage::Verifying | Stage::Failed => 12,
            Stage::Installing | Stage::Ready => 14,
        }
    }

    fn steps(&self) -> Paragraph<'static> {
        let current = match self.stage {
            Stage::Welcome => 0,
            Stage::Entry | Stage::Verifying | Stage::Confirmed => 1,
            Stage::Installing | Stage::Failed => 2,
            Stage::Ready => 3,
        };
        let labels = ["WELCOME", "PEERS", "INSTALL", "READY"];
        let mut spans = Vec::new();
        for (index, label) in labels.into_iter().enumerate() {
            let (mark, color) = match index.cmp(&current) {
                std::cmp::Ordering::Less => ("✓", GREEN),
                std::cmp::Ordering::Equal => ("●", CYAN),
                std::cmp::Ordering::Greater => ("○", MUTED),
            };
            spans.push(Span::styled(
                format!("{mark} {label}"),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ));
            if index < 3 {
                spans.push(Span::styled(" ──── ", Style::new().fg(MUTED)));
            }
        }
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center)
    }

    fn render_body(&self, frame: &mut Frame, area: Rect) {
        let title = match self.stage {
            Stage::Welcome => "  Your clipboard, without borders  ",
            Stage::Entry | Stage::Verifying => "  Add a passwordless SSH peer  ",
            Stage::Confirmed => "  Peer verified  ",
            Stage::Installing => "  Installing  ",
            Stage::Ready => "  Connected  ",
            Stage::Failed => "  Installation paused  ",
        };
        let block = Block::new()
            .title(Line::styled(title, Style::new().fg(ACCENT).bold()).centered())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(PANEL))
            .padding(ratatui::widgets::Padding::new(2, 2, 1, 1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        match self.stage {
            Stage::Installing => self.render_installing(frame, inner),
            _ => frame.render_widget(Paragraph::new(self.body_text()).wrap(Wrap { trim: false }), inner),
        }
    }

    fn body_text(&self) -> Text<'static> {
        match self.stage {
            Stage::Welcome => Text::from(vec![
                Line::styled(
                    "Copy on one machine. Paste on another.",
                    Style::new().fg(SOFT).bold(),
                ),
                Line::raw(""),
                Line::styled(
                    "Text, images, files, rich text, and native clipboard formats move as original bytes over persistent encrypted SSH.",
                    Style::new().fg(SOFT),
                ),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("● No cloud account", Style::new().fg(GREEN)),
                    Span::raw("     "),
                    Span::styled("● No conversion", Style::new().fg(GREEN)),
                    Span::raw("     "),
                    Span::styled("● Starts at login", Style::new().fg(GREEN)),
                ]),
            ]),
            Stage::Entry if self.tailscale_choices.is_empty() => {
                let visible = if self.input.is_empty() {
                    Span::styled("ssh macbookserver   or   ssh user@host", Style::new().fg(MUTED))
                } else {
                    Span::styled(clean_truncate(&self.input, 74), Style::new().fg(SOFT))
                };
                let mut lines = vec![
                    Line::styled(
                        if self.tailscale_detecting {
                            "Looking for Tailscale machines… or paste your working SSH command."
                        } else {
                            "Paste your working SSH command, or just enter its host."
                        },
                        Style::new().fg(MUTED),
                    ),
                    Line::raw(""),
                    Line::from(vec![
                        Span::styled(" command  ", Style::new().fg(CYAN).bold()),
                        visible,
                        Span::styled("▏", Style::new().fg(CYAN)),
                    ]),
                ];
                if let Some(error) = &self.error {
                    lines.push(Line::raw(""));
                    lines.push(Line::from(vec![
                        Span::styled("Couldn’t verify  ", Style::new().fg(RED).bold()),
                        Span::styled(clean_truncate(error, 72), Style::new().fg(SOFT)),
                    ]));
                }
                if !self.peers.is_empty() {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(
                        format!(
                            "✓ {} peer(s) ready — leave blank and press enter to install",
                            self.peers.len()
                        ),
                        Style::new().fg(GREEN),
                    ));
                }
                Text::from(lines)
            }
            Stage::Entry => self.tailscale_entry_text(),
            Stage::Verifying => Text::from(vec![
                Line::from(vec![
                    Span::styled(SPINNER[self.spinner], Style::new().fg(CYAN)),
                    Span::styled("  Opening an encrypted connection…", Style::new().fg(SOFT).bold()),
                ]),
                Line::raw(""),
                Line::styled(clean_truncate(&self.verifying, 76), Style::new().fg(MUTED)),
                Line::raw(""),
                Line::styled(
                    "Password and keyboard-interactive prompts are disabled; verification fails safely.",
                    Style::new().fg(MUTED),
                ),
            ]),
            Stage::Confirmed => {
                let verified_count = self.last_verified.max(1).min(self.peers.len());
                let mut lines = if verified_count == 1 {
                    let peer = self.peers.last().expect("confirmed stage has a peer");
                    vec![
                        Line::styled("✓  Connection verified", Style::new().fg(GREEN).bold()),
                        Line::raw(""),
                        Line::from(vec![
                            Span::styled(peer.probe.hostname.clone(), Style::new().fg(SOFT).bold()),
                            Span::styled(
                                format!("   {}/{}", peer.probe.os, peer.probe.arch),
                                Style::new().fg(MUTED),
                            ),
                        ]),
                        Line::styled(peer.command.clone(), Style::new().fg(MUTED)),
                        Line::styled(peer.installation.summary(), Style::new().fg(CYAN)),
                        Line::raw(""),
                        Line::styled(
                            "The host is reachable without a password and ready.",
                            Style::new().fg(SOFT),
                        ),
                    ]
                } else {
                    let names = self.peers[self.peers.len() - verified_count..]
                        .iter()
                        .map(|peer| format!("{} ({})", peer.probe.hostname, peer.installation.summary()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    vec![
                        Line::styled(
                            format!("✓  {verified_count} connections verified"),
                            Style::new().fg(GREEN).bold(),
                        ),
                        Line::raw(""),
                        Line::styled(clean_truncate(&names, 76), Style::new().fg(SOFT).bold()),
                        Line::raw(""),
                        Line::styled(
                            "The selected hosts are reachable without a password and ready for installation.",
                            Style::new().fg(SOFT),
                        ),
                    ]
                };
                lines.extend(self.confirmed_headless_lines());
                Text::from(lines)
            }
            Stage::Ready => {
                let pending = self
                    .completed
                    .iter()
                    .filter(|(_, detail)| detail.contains("next login"))
                    .count();
                Text::from(vec![
                    Line::styled(
                        if pending == 0 {
                            "✓  Your clipboards are connected".to_owned()
                        } else {
                            format!("✓  Setup complete · {pending} machine(s) start at next login")
                        },
                        Style::new().fg(GREEN).bold(),
                    ),
                    Line::raw(""),
                    Line::styled(
                        format!("{} peer(s) configured", self.peers.len()),
                        Style::new().fg(SOFT).bold(),
                    ),
                    Line::styled(
                        "Copy normally on either machine. The destination’s native clipboard changes, so Raycast and other clipboard managers see it naturally.",
                        Style::new().fg(SOFT),
                    ),
                    Line::raw(""),
                    Line::styled(
                        "Run ssh-clipboard monitor any time to watch activity and connection health.",
                        Style::new().fg(MUTED),
                    ),
                ])
            }
            Stage::Failed => Text::from(vec![
                Line::styled("Installation paused", Style::new().fg(RED).bold()),
                Line::raw(""),
                Line::styled(
                    clean_truncate(self.error.as_deref().unwrap_or("Unknown error"), 140),
                    Style::new().fg(SOFT),
                ),
                Line::raw(""),
                Line::styled(
                    "Completed peers are safe to reinstall; setup is idempotent.",
                    Style::new().fg(MUTED),
                ),
            ]),
            Stage::Installing => Text::default(),
        }
    }

    fn tailscale_entry_text(&self) -> Text<'static> {
        const PAGE_SIZE: usize = 4;
        let page_start = self.tailscale_cursor / PAGE_SIZE * PAGE_SIZE;
        let page_end = (page_start + PAGE_SIZE).min(self.tailscale_choices.len());
        let mut lines = vec![
            Line::styled(
                "Select online machines from your Tailscale network.",
                Style::new().fg(MUTED),
            ),
            Line::raw(""),
        ];
        for (index, choice) in self.tailscale_choices[page_start..page_end].iter().enumerate() {
            let absolute_index = page_start + index;
            let cursor = if absolute_index == self.tailscale_cursor {
                "›"
            } else {
                " "
            };
            let check = if choice.selected { "[✓]" } else { "[ ]" };
            let marker_color = if choice.selected { GREEN } else { CYAN };
            lines.push(Line::from(vec![
                Span::styled(format!("{cursor} {check} "), Style::new().fg(marker_color).bold()),
                Span::styled(
                    clean_truncate(&choice.peer.hostname, 24),
                    Style::new().fg(SOFT).bold(),
                ),
                Span::styled(
                    format!("   {}   {}", choice.peer.os, choice.peer.dns_name),
                    Style::new().fg(MUTED),
                ),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Or paste a passwordless SSH command:",
            Style::new().fg(MUTED),
        ));
        let visible = if self.input.is_empty() {
            Span::styled("ssh user@host", Style::new().fg(MUTED))
        } else {
            Span::styled(clean_truncate(&self.input, 68), Style::new().fg(SOFT))
        };
        lines.push(Line::from(vec![
            Span::styled(" command  ", Style::new().fg(CYAN).bold()),
            visible,
            Span::styled("▏", Style::new().fg(CYAN)),
        ]));
        if let Some(error) = &self.error {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("Couldn’t verify  ", Style::new().fg(RED).bold()),
                Span::styled(clean_truncate(error, 72), Style::new().fg(SOFT)),
            ]));
        }
        if !self.peers.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(
                    "✓ {} peer(s) ready — leave all options blank and press enter to install",
                    self.peers.len()
                ),
                Style::new().fg(GREEN),
            ));
        }
        Text::from(lines)
    }

    fn confirmed_headless_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for peer in &self.peers {
            let Some(clipboard) = peer.headless_capability() else {
                continue;
            };
            lines.push(Line::raw(""));
            if clipboard.xvfb_available {
                let mark = if peer.headless_x11 { "[✓]" } else { "[ ]" };
                lines.push(Line::from(vec![
                    Span::styled(format!("{mark} Managed Xvfb  "), Style::new().fg(CYAN).bold()),
                    Span::styled(
                        format!("{} · private display :99", peer.probe.hostname),
                        Style::new().fg(MUTED),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("Xvfb required  ", Style::new().fg(RED).bold()),
                    Span::styled(clipboard.install_hint(), Style::new().fg(SOFT)),
                ]));
            }
        }
        if let Some(error) = &self.error {
            lines.push(Line::styled(clean_truncate(error, 88), Style::new().fg(RED)));
        }
        lines
    }

    fn render_installing(&self, frame: &mut Frame, area: Rect) {
        let [heading, list, gauge, note] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(2),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(SPINNER[self.spinner], Style::new().fg(CYAN)),
                Span::styled(
                    "  Building your private clipboard mesh",
                    Style::new().fg(SOFT).bold(),
                ),
            ])),
            heading,
        );
        let mut lines = self
            .completed
            .iter()
            .map(|(peer, detail)| Line::styled(format!("✓  {peer}   {detail}"), Style::new().fg(GREEN)))
            .collect::<Vec<_>>();
        if !self.active_peer.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("●  ", Style::new().fg(CYAN)),
                Span::styled(self.active_peer.clone(), Style::new().fg(SOFT).bold()),
                Span::styled(format!("   {}", self.detail), Style::new().fg(MUTED)),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), list);
        let total = self.peers.len() + 1;
        let ratio = (self.completed.len() as f64 / total as f64).clamp(0.02, 1.0);
        frame.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(Style::new().fg(ACCENT).bg(PANEL))
                .label(format!("{} / {total}", self.completed.len())),
            gauge.inner(Margin::new(0, 0)),
        );
        frame.render_widget(
            Paragraph::new("Persistent channels keep updates instant—even for large images.")
                .style(Style::new().fg(MUTED)),
            note,
        );
    }

    fn help(&self) -> Line<'static> {
        let items: &[(&str, &str)] = match self.stage {
            Stage::Welcome => &[("enter", "begin"), ("ctrl+c", "quit")],
            Stage::Entry if self.tailscale_choices.is_empty() => {
                &[("enter", "verify / install"), ("ctrl+c", "quit")]
            }
            Stage::Entry => &[
                ("↑↓", "move"),
                ("space", "select"),
                ("enter", "verify / install"),
                ("ctrl+c", "quit"),
            ],
            Stage::Confirmed if self.peers.iter().any(|peer| peer.headless_capability().is_some()) => &[
                ("x", "toggle Xvfb"),
                ("enter", "install"),
                ("a", "add another"),
                ("ctrl+c", "quit"),
            ],
            Stage::Confirmed => &[("enter", "install"), ("a", "add another"), ("ctrl+c", "quit")],
            Stage::Installing | Stage::Verifying => &[("ctrl+c", "cancel")],
            Stage::Ready => &[
                ("a", "add another"),
                ("enter", "close"),
                ("ssh-clipboard monitor", "watch activity"),
            ],
            Stage::Failed => &[("r", "retry"), ("ctrl+c", "quit")],
        };
        let mut spans = Vec::new();
        for (index, (key, description)) in items.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("   •   ", Style::new().fg(MUTED)));
            }
            spans.push(Span::styled((*key).to_owned(), Style::new().fg(CYAN).bold()));
            spans.push(Span::styled(format!(" {description}"), Style::new().fg(MUTED)));
        }
        Line::from(spans)
    }
}

pub async fn run_setup(config: Config) -> Result<()> {
    let handle = Handle::current();
    tokio::task::spawn_blocking(move || ratatui::run(|terminal| SetupApp::new(handle, config).run(terminal)))
        .await
        .context("setup TUI task failed")??;
    Ok(())
}

#[cfg(test)]
mod tests;
