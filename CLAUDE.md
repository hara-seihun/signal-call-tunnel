# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`signal-call-tunnel` is a Rust binary that wraps RingRTC (Signal's WebRTC layer) to handle one voice call per process. It exposes a Unix control socket for signaling and uses virtual audio devices for audio I/O. The parent process (`signal-cli`, a separate repo) spawns one tunnel process per call.

## Repository Layout

- `signal-call-tunnel/` — Main Rust binary crate
  - `src/main.rs` — Entry point, RingRTC initialization, virtual audio setup, event loop
  - `src/config.rs` — Startup config deserialization from stdin
  - `src/control.rs` — Unix socket control server, JSON-line protocol parsing
  - `src/platform.rs` — RingRTC trait implementations (`SignalingSender`, `CallStateHandler`)
  - `build.rs` — Applies VPIO-disable patch to RingRTC at compile time
  - `patches/ringrtc-disable-vpio.patch` — macOS audio fix (skips VoiceProcessingIO for virtual audio)
- `third-party/ringrtc/` — Git submodule of Signal's RingRTC (https://github.com/signalapp/ringrtc)
- `docs/CALL_TUNNEL_IMPL.md` — Detailed implementation documentation

## Build & Test Commands

```bash
# Build (first build downloads ~100MB prebuilt WebRTC library)
cd signal-call-tunnel && cargo build --release

# Run tests
cd signal-call-tunnel && cargo test

# Run a single test
cd signal-call-tunnel && cargo test <test_name>

# Build with logging
cd signal-call-tunnel && RUST_LOG=debug cargo run --release
```

## Key Architecture Details

**Control protocol**: JSON-line protocol over a Unix socket. Messages include `auth`, `createOutgoingCall`, `proceed`, `receivedOffer`, `receivedAnswer`, `receivedIce`, `accept`, `hangup`. Responses include `stateChange`, `sendOffer`, `sendAnswer`, `sendIce`, `sendHangup`, `sendBusy`.

**Virtual audio**: Uses `VirtualAudioDevicePair` from RingRTC's `virtual_audio` module. On Linux, PulseAudio null sinks are created dynamically per call. On macOS, pre-installed BlackHole drivers are required (fixed names `signal_input`/`signal_output`).

**Encryption**: RingRTC's custom key derivation (x25519 DH → HKDF-SHA256 → SRTP keys), not DTLS-SRTP. Identity keys are passed via the control protocol, not embedded in offer/answer blobs.

**Build-time patching**: `build.rs` applies a patch to RingRTC's `audio_device_module.rs` that adds a `RINGRTC_NO_VOICE_PROCESSING` env var check. The patch is idempotent (checks for marker before applying). The tunnel sets this env var at startup.

**Startup sequence**: stdin config → create virtual audio pair → init `CallManager` → bind control socket → wait for cubeb device enumeration → select virtual devices by name → enter event loop.

## Platform Setup

**macOS** (one-time, requires root):
```bash
cd third-party/ringrtc
sudo bin/virtual_audio.sh --setup --input-source signal_input --output-sink signal_output
```

**Linux**: PulseAudio must be running. No setup needed — virtual audio modules are created/destroyed per call automatically.

## Dependencies

RingRTC is a local path dependency from the git submodule with features `prebuilt_webrtc` and `virtual_audio`. The crate patches `curve25519-dalek` to use Signal's fork for zkgroup compatibility.

Rust edition: 2024. License: AGPL-3.0-only.
