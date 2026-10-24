<div align="center">

<img src="viewer/src-tauri/icons/128x128@2x.png" width="96" alt="OpenDesk icon">

# OpenDesk

**Open-source, self-hosted remote desktop with end-to-end encryption.**<br>
An alternative to TeamViewer, AnyDesk and Parsec for Windows, macOS, Linux and the browser.

[![CI](https://github.com/treeot/opendesk/actions/workflows/ci.yml/badge.svg)](https://github.com/treeot/opendesk/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/treeot/opendesk?include_prereleases&label=release)](https://github.com/treeot/opendesk/releases)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Written in Rust](https://img.shields.io/badge/written%20in-Rust-orange)](https://www.rust-lang.org)

[Website](https://opendesk.treeot.dev) ·
[Docs](https://opendesk.treeot.dev/docs/) ·
[Download](https://github.com/treeot/opendesk/releases) ·
[Browser viewer](https://opendesk.treeot.dev/connect/) ·
[Self-hosting](docs/DEPLOYMENT.md)

</div>

---

OpenDesk lets you see and control another computer through a relay you run
yourself. Screen, input and sound are encrypted with a key only the two
computers hold, so the relay passes along bytes it can't read. Both sides
connect out over `wss://` on port 443: no port forwarding, and a direct UDP
path is used whenever the network allows it.

> [!CAUTION]
> OpenDesk is for computers you own or have permission to control. Using it
> to access someone else's computer without their consent is not supported
> and may be illegal.

## Features

| Feature | What you get |
|---|---|
| 🔒 **End-to-end encrypted** | SPAKE2 password key exchange, ChaCha20-Poly1305 for every frame and keystroke |
| 🛰️ **Self-hosted relay** | One small Rust binary or Docker image; it never sees your screen or keys |
| ⚡ **Direct when possible** | Peer-to-peer UDP with automatic fallback to the relay; latency badge in the toolbar |
| 🖥️ **Hardware H.264** | VideoToolbox on macOS, Media Foundation on Windows, openh264 elsewhere |
| 🔊 **System sound** | macOS 14.6+, Windows, Linux (PulseAudio / PipeWire) |
| 📁 **Files and clipboard** | Encrypted file transfer with consent and hash check; text clipboard sync |
| 🎬 **Session recording** | Record to a local WebM file, browse past recordings, and cap the disk space they use |
| 🪟 **Multi-monitor** | Pick a monitor, or add a virtual display on macOS and Windows |
| 🌙 **Unattended access** | Stored password, optional 2FA code, start at sign-in with a tray icon |
| 🔗 **Quick connect** | Save a device's password for one-click reconnect, or share an invite link |
| ⏾ **Wake-on-LAN** | Save a device's MAC address to wake it over the local network before connecting |
| 🌐 **Browser viewer** | Connect from any modern browser at [/connect](https://opendesk.treeot.dev/connect/) |
| 🔄 **Auto-update** | Signed updates checked from GitHub Releases |

## Quickstart

1. **Run a relay** on any server with Docker and a DNS name:
   ```bash
   DOMAIN=relay.example.com docker compose up -d
   ```
   HTTPS is automatic. See [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md).
2. **Install the app** on both computers from
   [Releases](https://github.com/treeot/opendesk/releases) and set the relay to
   `wss://relay.example.com/ws`.
3. **Host:** click **Start hosting** and note the nine-digit ID, or use **Copy
   invite** to share a link that fills it in.
4. **Viewer:** enter that ID (or paste the invite link) and the session
   password, then accept on the host. Tick **Remember password for this
   device** to reconnect in one click, or set up unattended access to skip the
   prompt entirely.

The [getting started guide](https://opendesk.treeot.dev/docs/getting-started/)
covers each step in more detail.

## Platform support

| | Host | Viewer | Notes |
|---|:---:|:---:|---|
| **macOS** 12.3+ | ✅ | ✅ | Most tested. Needs Screen Recording and Accessibility |
| **Windows** 10 1803+ | ✅ | ✅ | Less runtime-tested than macOS |
| **Linux** | ✅ | ✅ | PipeWire + xdg-desktop-portal; Wayland input is limited |
| **Browser** | — | ✅ | Relay only (no direct connection); can't host |

Release builds are not code-signed. The
[install guide](https://opendesk.treeot.dev/docs/getting-started/) shows how to
open them.

## How it works

```
 HOST                         RELAY                         VIEWER
 capture → H.264 → seal  ──►  forwards sealed bytes  ──►  open → decode
 inject input ◄── open   ◄──  (no keys, no plaintext) ◄──  seal ← input
          └───────────── direct UDP when reachable ─────────────┘
```

| Crate | What it does |
|---|---|
| [`crates/protocol`](crates/protocol) | Wire types, SPAKE2 handshake, sealing and key derivation |
| [`crates/relay`](crates/relay) | axum WebSocket server that pairs peers by ID and forwards sealed bytes |
| [`crates/host-core`](crates/host-core) | Transport (ICE, relay fallback), capture, encoding, input injection |
| [`viewer/`](viewer) | Tauri + React desktop app that both hosts and views |
| [`crates/web-core`](crates/web-core) · [`web/`](web) | The browser viewer, compiled to WebAssembly |
| [`site/`](site) | The website and docs (Astro + Starlight, Markdown pages) |

More detail: [architecture](docs/ARCHITECTURE.md) ·
[security model](docs/SECURITY.md) ·
[browser viewer trade-offs](https://opendesk.treeot.dev/docs/browser-viewer/).

## Build from source

Needs a recent Rust toolchain and Node.js.

```bash
cargo run -p relay                              # relay on :8080 (ws://127.0.0.1:8080/ws)
cd viewer && npm install && npm run tauri dev   # desktop app
```

<details>
<summary>Browser viewer and website</summary>

```bash
cd web && npm ci && npm run wasm && npm run build   # needs wasm32-unknown-unknown + wasm-bindgen-cli 0.2.129
cd site && npm ci && npm run build                  # run `npm run wasm` in web/ first
```

Set `OPENDESK_DEFAULT_RELAY` at build time to change the browser viewer's
default relay. Docs pages are Markdown files in `site/src/content/docs/docs/`.

</details>

On Linux, hardware H.264 through VAAPI is an experimental, opt-in build. It
needs the ffmpeg development libraries (`libavcodec`, `libavutil`) and a working
VAAPI driver, and is enabled by building host-core with `--features vaapi`. If
the encoder cannot start, hosting falls back to the openh264 software encoder.

To run two instances on one machine, see [`docs/TESTING.md`](docs/TESTING.md).

## Roadmap

- [x] Direct UDP with relay fallback, end-to-end encryption
- [x] Hardware H.264 on macOS and Windows
- [x] Files, clipboard, audio, recording, multi-monitor
- [x] Unattended access with 2FA, start at sign-in
- [x] Saved device passwords and shareable invite links
- [x] Browser viewer
- [x] Virtual displays on macOS and Windows
- [x] In-session text chat and view-only sessions
- [x] Privacy mode and TCP tunneling (port forwarding over the data channel)
- [x] Wake-on-LAN for saved devices on the local network
- [ ] Windows sign-in screen, UAC prompts, lock screen and Ctrl+Alt+Del (shipped experimental; needs hardware/VM validation)
- [ ] Hardware H.264 on Linux (VAAPI) (shipped experimental behind `--features vaapi`; needs validation on Linux GPU hardware)

### Longer term

Bigger pieces that would bring OpenDesk closer to a full TeamViewer or AnyDesk
replacement, in rough priority order:

- **Accounts and a web console**: sign-in, synced address book, and device
  and user management for teams.
- **Native mobile apps**: Android and iOS, beyond today's browser viewer.
- **More codecs**: H.265 and VP8/VP9, and hardware encoding on Linux.
- **Session extras**: remote restart with auto-reconnect, swapping which side
  controls, and remote printing.

## Contributing

Issues and pull requests are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

[AGPL-3.0-only](LICENSE). You can use, change and self-host OpenDesk freely.
If you distribute a modified version, or run one others use over a network
(a modified relay, for example), you must publish its source under the same
licence.

<details>
<summary>Third-party software: Amyuni virtual display driver (Windows)</summary>

On Windows, OpenDesk can download and install Amyuni Technologies'
`usbmmidd_v2` virtual display driver for you (see
[docs/SECURITY.md](docs/SECURITY.md#virtual-displays-on-windows)). It is
not part of OpenDesk and comes under its own licence:

```text
Copyright 2014-2021 Amyuni Technologies Inc.
https://www.amyuni.com

This software is provided 'as-is', without any express or implied warranty. In no event will the authors be held liable for any damages arising from the use of this software.
By using this software, you accept to be prompted with an advertisement page which is not always under the control of the author. A fully commercial version is available for users who do not wisth to see those advertisements.

Permission is granted to anyone to use this software for any purpose subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not claim that you wrote the original software.
   If you use this software in a product, an acknowledgment in the product documentation would be required.
2. Altered versions must be plainly marked as such, and must not be misrepresented as being the original software.
3. This notice may not be removed or altered from any distribution.
```

</details>
