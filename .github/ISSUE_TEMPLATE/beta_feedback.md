---
name: Beta feedback
about: Share a beta test result, audio issue, connection problem, or suggestion.
title: "[Beta feedback] "
labels: []
assignees: []
---

<!-- Thank you for testing Virtual Jam! Successful sessions and suggestions are welcome.
Use wired headphones at 48 kHz. Fill in what you know; "unknown" is fine.
This issue is public. Logs and screenshots are optional: review them before posting,
and remove private IPs, nicknames, paths, tokens, or other personal information.
Do not attach client.toml: it may contain a telemetry token. -->

### What happened?

<!-- Describe the problem, successful test, or suggestion in your own words. -->

### Versions and audio hardware

- Client version / release tag (or commit for a source build):
- Server version (if known):
- OS version and architecture (Windows x64 / macOS ARM64 / Linux x64):
- Audio backend (ASIO / WASAPI / CoreAudio / ALSA, including PipeWire via ALSA):
- Audio interface model and driver version:
- Input device / instrument channel / microphone channel (or disabled):
- Output device / wired headphones:
- Sample rate (expected: 48 kHz):
- Buffer size (64 / 128 / 256 frames):
- Hardware direct monitoring on or off:

### Session and connection

- Test type (local loopback / LAN / remote shared server / self-hosted server):
- Number of musicians (including yourself; maximum 8):
- Network (Ethernet / Wi-Fi / mobile hotspot / VPN):
- Approximate distance or broad region of peers/server (optional; no exact address):
- Observed routes (P2P / SFU / mixed / no peers; read `Peer N: P2P/SFU`):
- Session duration and time until the issue appeared:
- Displayed E2E Latency estimate in ms and status (or unavailable):
- Ping RTT / jitter / packet loss (if visible):
- External latency measurement, if any (value and method; separate from the GUI estimate):
- Room Metronome and Tempo Leader settings (if relevant):

### How to reproduce

1.
2.
3.

- Expected behavior:
- Actual behavior (clicks, dropouts, delayed monitoring, disconnects, etc.):
- Frequency (always / intermittent / once) and any workaround:

### Optional telemetry or screenshots

<!-- After disconnecting, find telemetry-*.txt next to client.toml (the GUI displays
the Configuration path). Attach the reviewed file or paste relevant summary lines below.
Pre-release installers enable local logging; source builds require telemetry-log.
The remote "Share anonymous end-of-session technical report (beta)" checkbox is optional,
off by default, and not required to submit feedback. No audio recording is needed. -->

```text
Paste relevant telemetry-*.txt summary lines here, or leave blank.
```

### Anything else?

<!-- For a successful test, tell us what worked. For a suggestion, describe your use case. -->
