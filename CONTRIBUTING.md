# Contributing to Virtual Jam

Thank you for your interest in contributing to **Virtual Jam**! This document outlines
the technical guidelines, non-negotiable real-time constraints, and workflow for
submitting changes via Pull Requests.

---

## 1. Monorepo Structure (Cargo Workspace)

The project is a Rust workspace (`edition = "2024"`, MSRV `1.85`) composed of four crates:

- `crates/jam-protocol`: shared library containing constants, `#[repr(C)]` binary headers/messages, and zero-copy serialization. **No OS, audio, or networking dependencies.**
- `crates/jam-server`: headless UDP SFU daemon (`1 -> N-1` packet reflector, `SessionTable` with 5 s watchdog, P2P signaling, and room clock/metronome synchronization).
- `crates/jam-client`: desktop client (`cpal` + `rtrb` audio engine, per-peer adaptive jitter buffer, `opus` codec, DSP mixer, hybrid P2P engine, and `eframe`/`egui` GUI).
- `crates/jam-collector`: optional HTTP service for centralized collection of operational logs and anonymous beta-testing reports.

---

## 2. Architectural & Real-Time Constraints (Non-Negotiable)

Every contribution must strictly respect the following rules to keep end-to-end latency below 20–25 ms and prevent audio glitches:

1. **Real-Time Audio Thread (`cpal` callbacks):**
   - **FORBIDDEN** to allocate heap memory (`Box`, `Vec`, `String`, `format!`, `.to_owned()`).
   - **FORBIDDEN** to acquire `Mutex`, `RwLock`, standard MPSC channels, or perform blocking syscalls / I/O (network, disk, logging, sleep).
   - Communication between the Audio Thread, Network Thread, and UI Thread must occur **exclusively** through lock-free SPSC ring buffers (`rtrb`) and atomics (`AtomicU32` with `f32::to_bits()`, `AtomicBool`, `AtomicU64`).
2. **Networking & Protocol:**
   - **UDP only** for real-time audio streaming and control (never TCP, to avoid *Head-of-Line Blocking*).
   - No text-based formats (JSON, MessagePack, etc.) on the audio/control wire: use fixed binary layouts with explicit endianness (`to_le_bytes` / `from_le_bytes`) and datagrams always `< 1400` bytes (below MTU).
   - Every change to `jam-protocol` must include **round-trip serialization unit tests** and boundary/malformed input validation.
3. **Per-Peer Isolation:**
   - Jitter buffers (`PeerJitterBuffer`) and Opus decoders must be instantiated **independently for each `peer_id`**, never shared across different peers.
4. **Rust Quality & Safety:**
   - Avoid `unsafe`; if strictly necessary, document every block with a thorough `// SAFETY:` comment.
   - Avoid `unwrap()` / `expect()` in library code and non-test runtime paths; propagate errors explicitly (`Result` / `?`).
   - Comply with workspace lints (`clippy::all` and `clippy::pedantic` enforced as errors in CI).

---

## 3. Build Prerequisites

- **Rust Toolchain:** `rustc >= 1.85` (with `rustfmt` and `clippy`).
- **Opus (`opusic-sys`):** CMake and a C/C++ compiler.
- **Linux:** `pkg-config`, `libasound2-dev`, `libxkbcommon-dev`, `libwayland-dev`, `libx11-dev`, `libxcursor-dev`, `libxi-dev`, `libxrandr-dev`, `libgl1-mesa-dev`.
- **Windows:** MSVC C++ build tools, LLVM/`libclang`, and Steinberg ASIO SDK (`CPAL_ASIO_DIR`, `LIBCLANG_PATH`) for builds with the ASIO backend.
- **macOS:** Xcode Command Line Tools (CoreAudio).

---

## 4. Workflow and Pull Requests

1. **Open or check an Issue:** before architectural changes or new features, open an issue to discuss the approach.
2. **Create a dedicated branch** from `develop` (or the active pre-release branch):
   ```bash
   git checkout develop
   git pull origin develop
   git checkout -b feat/short-description
   ```
3. **Implement and add tests:**
   - Add deterministic unit tests (and zero-allocation tests if touching audio paths).
   - Update `README.md` if you change the wire protocol or user-facing behavior.
4. **Run all local verification gates before opening a PR:**
   ```bash
   cargo fmt --check
   cargo clippy --workspace -- -D warnings
   cargo test --workspace
   cargo clippy --workspace --features jam-client/telemetry-log -- -D warnings
   cargo test --workspace --features jam-client/telemetry-log
   ```
   If you have `cargo-deny` installed, also verify third-party license compliance:
   ```bash
   cargo deny check licenses
   ```
5. **Open the Pull Request:** clearly describe context, changes, tests executed, and any required manual hardware verification (e.g., ASIO/CoreAudio/ALSA).

---

## 5. Contribution License

Virtual Jam is distributed under the **GNU General Public License v3.0 or later (`GPL-3.0-or-later`)**, with an additional permission under GPLv3 Section 7 for linking with the Steinberg ASIO SDK on Windows builds (see the **License** section in [README.md](README.md)).

By submitting a Pull Request, you agree that your contribution will be licensed under the same terms as the project.
