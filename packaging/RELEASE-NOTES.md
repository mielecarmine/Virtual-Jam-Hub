Pre-release of the Virtual Jam client `1.0.5-beta`, prepared from `develop`.

## Download and Installation

| System | Installer | Requirements |
| --- | --- | --- |
| Windows | `Virtual-Jam-1.0.5-beta-windows-x64-setup.exe` | Windows 10/11 x64; per-user install, no administrator privileges required |
| macOS Apple Silicon | `Virtual-Jam-1.0.5-beta-macos-arm64.pkg` | macOS 11 or later |

macOS: open the package and launch **Virtual Jam** from Applications; grant microphone access.
Windows: run the installer and launch **Virtual Jam** from the Start menu.
`SHA256SUMS.txt` contains the installer hashes.

The installers in this beta do not carry commercial Windows/Apple code signing or Apple notarization. SmartScreen or Gatekeeper may require explicit approval in system security settings. The ad-hoc signature on the macOS bundle does not certify the author's identity.

## What's New Since 1.0.4-beta

- Improved audio continuity during UDP bursts and reduced mixer starvation (JAM-35).
- Reduced local receive buffers and stabilized Tempo Leader synchronization (JAM-36).
- Added hybrid P2P signaling and selective SFU server forwarding (JAM-37).
- Added direct UDP audio routes between peers, with automatic fallback to the SFU server and route indicators in the GUI (JAM-38).

Client and server must be updated together to `1.0.5-beta`: signaling introduces new binary messages and requires a compatible server.

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

A compatible Virtual Jam server reachable over UDP (default port 7777) is required: enter its address in the GUI. The server is not installed by the desktop packages; instructions to build and install it are in `deploy/README.md` in the attached sources.
Use headphones and devices that support 48 kHz. ASIO requires the manufacturer's driver.
The current protocol does not provide authentication or encryption: use sessions on trusted networks or VPNs.
Automated checks do not replace real audio testing: interface compatibility, latency, and cross-platform listening remain to be validated by beta testers.
