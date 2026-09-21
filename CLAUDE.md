# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`signal-call-tunnel` is a Rust binary that wraps RingRTC (Signal's WebRTC layer) to handle one voice call per process. It exposes a Unix control socket for signaling and uses virtual audio devices for audio I/O. The parent process (`signal-cli`, a separate repo) spawns one tunnel process per call.

## Repository Layout

- `build.sh` — Canonical build entry point; applies ringrtc patches, then runs cargo
- `signal-call-tunnel/` — Main Rust binary crate
  - `src/main.rs` — Entry point, RingRTC initialization, audio backend setup, event loop
  - `src/config.rs` — Startup config deserialization from stdin
  - `src/control.rs` — Unix socket control server, JSON-line protocol parsing
  - `src/platform.rs` — RingRTC trait implementations (`SignalingSender`, `CallStateHandler`)
  - `src/pipe_audio.rs` — Pipe audio backend (`PipeAudioDevice`), a `CustomAudioDevice` that exchanges PCM over a Unix socket
  - `build.rs` — Fallback that applies the ringrtc patches at compile time (idempotent)
  - `patches/ringrtc-disable-vpio.patch` — macOS audio fix (skips VoiceProcessingIO for virtual audio)
  - `patches/ringrtc-custom-audio-backend.patch` — adds the generic `CustomAudioDevice` extension point
- `third-party/ringrtc/` — Git submodule of Signal's RingRTC (https://github.com/signalapp/ringrtc)
- `docs/CALL_TUNNEL_IMPL.md` — Detailed implementation documentation

## Build & Test Commands

```bash
# Build (applies ringrtc patches, then builds; first build downloads ~100MB WebRTC lib)
./build.sh

# Debug build
./build.sh --debug

# Run tests
./build.sh test

# Build with logging
cd signal-call-tunnel && RUST_LOG=debug cargo run --release
```

Prefer `./build.sh` over a bare `cargo build`: it applies the required ringrtc
patches before cargo compiles the dependency, so a single invocation always
succeeds. A direct `cargo build` on a pristine checkout must be run twice (see
Build-time patching below).

## Key Architecture Details

**Control protocol**: JSON-line protocol over a Unix socket. Messages include `auth`, `createOutgoingCall`, `proceed`, `receivedOffer`, `receivedAnswer`, `receivedIce`, `accept`, `hangup`. Responses include `stateChange`, `sendOffer`, `sendAnswer`, `sendIce`, `sendHangup`, `sendBusy`.

**Virtual audio**: Uses `VirtualAudioDevicePair` from RingRTC's `virtual_audio` module. On Linux, PulseAudio null sinks are created dynamically per call. On macOS, pre-installed BlackHole drivers are required (fixed names `signal_input`/`signal_output`).

**Encryption**: RingRTC's custom key derivation (x25519 DH → HKDF-SHA256 → SRTP keys), not DTLS-SRTP. Identity keys are passed via the control protocol, not embedded in offer/answer blobs.

**Build-time patching**: The ringrtc submodule is kept pristine in git. Two patches under `signal-call-tunnel/patches/` are applied to its working tree at build time: `ringrtc-disable-vpio.patch` (adds a `RINGRTC_NO_VOICE_PROCESSING` env var check to skip macOS VPIO) and `ringrtc-custom-audio-backend.patch` (adds the generic `CustomAudioDevice` extension point used by the pipe backend). Order matters — the custom-audio-backend patch is generated against a VPIO-patched tree. Both `build.sh` and `build.rs` apply them idempotently (checking for a marker string before applying). Because cargo compiles the ringrtc dependency *before* `build.rs` runs and the patches change ringrtc's public API, a direct `cargo build` on a pristine checkout must run **twice** (the first run applies the patches and stops with a message); `build.sh` applies them up front to avoid this. To regenerate a patch, diff against a tree that already has the *earlier* patches applied (e.g. the custom-audio-backend patch is diffed against pristine+VPIO), otherwise hunks overlap.

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
