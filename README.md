<div align="center">

# ssh-clipboard

### Your clipboard. Every machine. No cloud.

[![CI](https://github.com/standardagents/ssh-clipboard/actions/workflows/ci.yml/badge.svg)](https://github.com/standardagents/ssh-clipboard/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/ssh-clipboard?color=cb3837&logo=npm)](https://www.npmjs.com/package/ssh-clipboard)
[![MIT](https://img.shields.io/badge/license-MIT-7c3aed)](LICENSE)

```text
┌──────────────┐        encrypted SSH        ┌──────────────┐
│   your Mac   │  ◀══════════════════════▶  │  Mac / Linux │
└──────────────┘                             └──────────────┘
```

Copy here. Paste there. Text, images, files, rich content—native formats intact.

</div>

```sh
npm i -g ssh-clipboard
ssh-clipboard
```

The first-run TUI offers compatible online machines from Tailscale when it is installed, or accepts any passwordless SSH connection. It verifies each connection, inspects any existing installation, installs or upgrades only when needed, and starts a per-user background service without replacing that machine’s identity or peer configuration. After that, it just feels like one clipboard.

- **Native:** macOS pasteboard plus Linux Wayland/X11—not terminal escape tricks.
- **Private:** persistent peer-to-peer SSH; no relay, account, port, or new encryption key.
- **Faithful:** preserves every available representation, with native Finder file paste on macOS.
- **Invisible:** Raycast and other clipboard managers see ordinary system clipboard writes.
- **Fast:** raw bytes, persistent connections, deduplication, and newest-value queues.

```sh
ssh-clipboard monitor          # delightful live dashboard
ssh-clipboard status --json    # automation-friendly health
ssh-clipboard setup            # add or repair peers
ssh-clipboard update --check   # compare this node with npm @latest
```

The manual update command also reconciles the per-user service, so it can recover an installed binary whose launchd or systemd job is missing.

### Copying files

Copy files or folders in Finder, wait for the transfer to finish in `ssh-clipboard monitor`,
then paste into a Finder folder on the other Mac. Transfers are extension-independent:
PDFs, DMGs, PKGs, images, documents, and unknown file types all use the same byte-transfer
path. Folders, empty directories, executable permissions, and symbolic links are preserved.
Copying does not install or execute anything, and does not delete the source.

On Linux, a running graphical clipboard (X11 or supported Wayland session) is required.
The receiver publishes local file URLs with GNOME/Nautilus and KDE copy formats;
macOS applications and installers remain files, not Linux-compatible applications.
macOS-only metadata such as resource forks and extended attributes is not currently
preserved. Both peers must run the current version for symbolic-link transfers.

The configured `max_bytes` limit applies to the **whole selection**, including transfer
metadata (256 MiB by default). Larger selections require raising it on both peers and
restarting their services. Transfers are currently buffered in memory, so choose a limit
that both machines can accommodate; large files are not instantaneous.

When using Apple's Screen Sharing, turn off **Edit → Use Shared Clipboard** to avoid
its separate filename-only clipboard updates competing with ssh-clipboard.

### Headless Linux

Linux servers without Wayland or X11 need a virtual display before they have a clipboard.
`ssh-clipboard setup` detects this condition and offers an opt-in managed Xvfb service. If Xvfb is
not installed, setup shows the appropriate `apt`, `dnf`, or `pacman` command; it never runs `sudo`
without you. Once selected, the per-user service runs against private display `:99` and upgrades
preserve that choice. See the [headless Linux guide](docs/headless-linux.md) for guided and manual
setup, lingering, and troubleshooting.

The monitor shows each machine on its own row with installed and target versions. Press `u` to queue an immediate npm check and notify every connected client that supports update events.

Every installed daemon independently checks the stable npm release at startup and every 15 minutes, and gossips its verified desired version to connected peers. Any online machine can therefore trigger convergence; there is no permanent update coordinator. Packages are accepted only after npm SHA-512 integrity, the bundled SHA-256 manifest, executable target, and reported binary version all agree. Updates retain the previous executable, replace the live binary atomically, and explicitly ask launchd/systemd to restart the daemon.

macOS and Linux · arm64 and x64 · Rust + [Ratatui](https://ratatui.rs)

<sub>Deep cuts: [architecture](docs/architecture.md) · [headless Linux](docs/headless-linux.md) · [TUI design](docs/tui-design.md) · [npm distribution](docs/distribution.md)</sub>
