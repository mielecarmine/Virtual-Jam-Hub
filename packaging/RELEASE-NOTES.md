Pre-release of Virtual Jam `1.0.6-beta`.

## Download and Installation

| System | Installer | Requirements |
| --- | --- | --- |
| Windows x64 | `Virtual-Jam-1.0.6-beta-windows-x64-setup.exe` | Windows 10/11 x64; per-user install, no administrator privileges required |
| macOS Apple Silicon | `Virtual-Jam-1.0.6-beta-macos-arm64.pkg` | macOS 11 or later on ARM64 |
| Linux x64 client | `Virtual-Jam-1.0.6-beta-linux-amd64.deb` | Ubuntu 22.04+ / compatible Debian-based x86_64 system with glibc 2.35+ |
| Linux x64 standalone server | `Virtual-Jam-Server-1.0.6-beta-linux-amd64.tar.gz` | Linux x86_64 with glibc 2.35+; no audio hardware or Rust toolchain required |

macOS: open the package and launch **Virtual Jam** from Applications; grant microphone access.
Windows: run the installer and launch **Virtual Jam** from the Start menu.
Linux client: run `sudo apt install ./Virtual-Jam-1.0.6-beta-linux-amd64.deb`, then launch **Virtual Jam** or `virtual-jam`.
Linux server: extract the archive and run `./jam-server`; see the included `deploy/README.md` for systemd installation.
`SHA256SUMS.txt` contains hashes for all four downloadable assets.

The installers in this beta do not carry commercial Windows/Apple code signing or Apple notarization. SmartScreen or Gatekeeper may require explicit approval in system security settings. The ad-hoc signature on the macOS bundle does not certify the author's identity.

## What's New Since 1.0.5-beta

- Independent instrument and optional microphone channel selection on multi-channel interfaces, with separate local headphone-monitoring strips and a **Mute Mic (Net)** toggle (JAM-42).
- Desktop interface, tooltips, and user-facing validation and diagnostic messages in English (JAM-45).
- Linux x64 `.deb` client and precompiled standalone `jam-server` archive, built and published automatically alongside Windows and macOS installers (JAM-46).
- Optional sharing of an anonymous end-of-session technical report, disabled by default, plus an optional collector for client reports and server logs. Reports contain no audio or nicknames; the receiving service observes the source IP. See the README privacy notice (JAM-39).
- GPL-3.0-or-later licensing with an additional permission for linking the Steinberg ASIO SDK, third-party license checks, repository hardening, and English documentation (JAM-40, JAM-41).
- Quick Start for beta testers, interface preview, and a structured GitHub feedback template (JAM-47).

Update the whole room to `1.0.6-beta`, including the server. See the README's **Quick Start for Beta Testers** for hardware setup, connection instructions, and optional feedback.

## What's New in 1.0.5-beta

- Improved audio continuity during UDP bursts and reduced mixer starvation (JAM-35).
- Reduced local receive buffers and stabilized Tempo Leader synchronization (JAM-36).
- Added hybrid P2P signaling and selective SFU server forwarding (JAM-37).
- Added direct UDP audio routes between peers, with automatic fallback to the SFU server and route indicators in the GUI (JAM-38).

The `1.0.5-beta` signaling changes introduce new binary messages and require a compatible server.

## What's New in 1.0.4-beta

- Improved playback continuity while draining backlogs and recovering from micro-dropouts (JAM-28, JAM-30).
- Isolated telemetry I/O and improved audio controller thread priority (JAM-29).
- Added an end-to-end latency budget estimate (JAM-31).
- Added room clock and shared metronome, with local synthesis and GUI controls (JAM-32, JAM-33, JAM-34).

## What's New in 1.0.3-beta

- Preserved incoming audio during client scheduling pauses (JAM-24).
- Improved recovery of empty jitter buffers after latency spikes and fixed peer activity timeout (JAM-25).
- Extended audio and worker diagnostics to analyze problematic sessions (JAM-26).
- Preferred stereo output configurations during audio device selection (JAM-27).
- Installers available for Windows x64 and macOS Apple Silicon ARM64.

## Features Introduced in Earlier Betas

- Reduced periodic receive mixer underflows: peer queues are prefilled and maintain a playback reserve (JAM-21).
- Preserved Opus decoder continuity during jitter buffer adaptation, avoiding jumps in codec history (JAM-22).
- Fixed jitter buffer latency growth caused by client scheduling delays, with burst packet handling and playback clock jumps (JAM-23).
- Bounded receive latency and playback clock recovery to prevent delay accumulation across sessions (JAM-17).
- Direct local input headphone monitoring, with dedicated gain and mute without altering the signal sent to peers (JAM-18).
- GUI session telemetry logs, enabled in pre-release installers: `telemetry-*.txt` files next to `client.toml`, useful for reporting audio and network issues (JAM-20).
- Multi-peer headless network benchmark available in source as the `net_bench` example to verify behavior under load (JAM-19).

## Features

- Desktop client with audio backend/device selection, buffer size selection, and persistent preferences.
- UDP audio sessions via SFU server, Opus codec, and independent per-peer jitter buffers.
- Per-participant mixer with gain, mute, and solo; VU meters and network telemetry.
- ALSA backend on Linux, WASAPI/ASIO on Windows, and CoreAudio on macOS.

## Beta Limitations

A compatible Virtual Jam server reachable over UDP (default port 7777) is required: enter its address in the GUI. The server is distributed separately from the desktop packages in the Linux standalone archive, which includes `deploy/README.md` installation instructions.
Use headphones and devices that support 48 kHz. ASIO requires the manufacturer's driver.
The current protocol does not provide authentication or encryption: use sessions on trusted networks or VPNs.
Automated checks do not replace real audio testing: interface compatibility, latency, and cross-platform listening remain to be validated by beta testers.
