# Call Tunnel Implementation

Implementation details for `signal-call-tunnel` as specified by `signal-cli`'s
documentation. For the control socket protocol, config format, authentication,
call flows, and state machine, see `CALL_TUNNEL.md` in the `signal-cli`
repository.

## Overview

`signal-call-tunnel` is a Rust binary that wraps RingRTC (Signal's WebRTC
layer) to handle one voice call per process. It exposes a control socket for
signaling and uses virtual audio devices for audio I/O.

```
                       signal-call-tunnel
                      /                    \
              ctrl.sock          VirtualAudioDevicePair
          (JSON control)        /                     \
              |          [virtual input]         [virtual output]
     signal-cli connects  (signal_input_XXX)     (signal_output_XXX)
     (signaling relay)        |                        |
                        client writes audio      client reads audio
                        (PulseAudio/CoreAudio)   (PulseAudio/CoreAudio)
```

---

## Source Layout

| File | Role |
|------|------|
| `main.rs` | Entry point, RingRTC initialization, virtual audio setup, event loop |
| `config.rs` | Deserializes startup config from stdin |
| `control.rs` | Control socket server, JSON message parsing/serialization |
| `platform.rs` | RingRTC trait impls (`SignalingSender`, `CallStateHandler`) |

---

## Startup

On launch, the tunnel:

1. Parses config JSON from stdin
2. Creates a `VirtualAudioDevicePair` from ringrtc's `virtual_audio` module
3. Initializes a RingRTC `CallManager`
4. Binds the control socket and queues a `ready` message (with audio device names)
5. Initializes a cubeb `AudioDeviceModule`
6. Waits for cubeb to enumerate devices
7. Selects the virtual devices by name

Audio flows through cubeb with virtual audio devices selected by name.

---

## Encryption and Key Derivation

The tunnel handles all encryption. Neither the parent process nor the audio client
ever sees SRTP keys or encrypted media.

RingRTC uses a custom key derivation scheme (not DTLS-SRTP):

1. Each side generates an ephemeral x25519 keypair
2. Public keys are embedded in the opaque offer/answer blobs
3. x25519 DH produces a shared secret
4. HKDF-SHA256 derives SRTP keys with info string:
   `Signal_Calling_20200807_SignallingDH_SRTPKey_KDF || caller_identity || callee_identity`
5. Keys are injected into WebRTC with DTLS disabled

The identity keys are **not** inside the opaque blobs. They are passed
separately via the control protocol (`senderIdentityKey`, `receiverIdentityKey`)
and come from the Signal protocol message envelope. See the `signal-cli`
documentation for the required encoding.

---

## Virtual Audio Devices

The tunnel uses `VirtualAudioDevicePair` from ringrtc's `virtual_audio` module
to create platform-specific virtual audio devices.

### PCM Parameters

| Parameter | Value |
|-----------|-------|
| Sample rate | 48,000 Hz |
| Channels | 1 (mono) |
| Sample format | 16-bit signed integer, little-endian |

### Linux (PulseAudio)

Virtual devices are PulseAudio null sinks/sources created automatically per
call and torn down on drop. No setup required.

- **Input device** (client -> WebRTC): write to PulseAudio sink `sink_for_<inputDeviceName>`
- **Output device** (WebRTC -> client): read from PulseAudio monitor `<outputDeviceName>.monitor`

Example:

```bash
# Send audio to WebRTC
paplay --device=sink_for_signal_input_12345 tone.wav

# Record from WebRTC
parecord --device=signal_output_12345.monitor --rate=48000 --channels=1 --format=s16le captured.wav
```

### macOS (BlackHole)

Requires one-time root setup to install BlackHole audio drivers:

```bash
cd third-party/ringrtc
sudo bin/virtual_audio.sh --setup --input-source signal_input --output-sink signal_output
```

This installs drivers in `/Library/Audio/Plug-Ins/HAL/` that persist across
reboots. No root is needed after setup. Unlike Linux, BlackHole drivers
cannot be created dynamically per call — they must be pre-installed with
exact matching names. When `input_device_name`/`output_device_name` are
omitted from the config, the tunnel defaults to the fixed names
`signal_input` and `signal_output` on macOS (vs. per-call unique names on
Linux).

Example:

```bash
# Send audio to WebRTC
sox tone.wav -t coreaudio signal_input repeat -

# Record from WebRTC
sox -t coreaudio signal_output captured.wav trim 0 5
```

### VirtualAudioDevicePair Lifecycle

The `VirtualAudioDevicePair` is kept alive for the duration of the tunnel
process. On drop:
- **Linux**: calls `pactl unload-module` to clean up PulseAudio modules
- **macOS**: logs a warning (can't teardown without root) but BlackHole drivers
  persist for the next call, which is the intended behavior

---

## Audio Client Integration

Any process that sends/receives audio via the virtual audio devices using
platform audio APIs.

### When to connect

Connect to the virtual audio devices **after** the call reaches `CONNECTED`
state. Device names are returned in the `startCall`/`acceptCall` JSON-RPC
response and in `callEvent` notifications.

You can connect earlier (the devices exist from tunnel startup), but no
meaningful audio will flow until ICE completes and the call is connected.

### Sending audio (recording path)

Write audio to the virtual input device using platform APIs. The tunnel's
cubeb recording captures from this device and feeds it to WebRTC.

- **Linux**: `paplay --device=sink_for_<inputDeviceName> audio.wav`
- **macOS**: `sox audio.wav -t coreaudio <inputDeviceName>`

If you have nothing to send (muted), simply don't write. WebRTC's Opus DTX
will detect silence and send minimal comfort noise packets.

### Receiving audio (playout path)

Read audio from the virtual output device using platform APIs. WebRTC receives
and Opus-decodes remote audio, cubeb plays it to the virtual output device,
and you capture it from the monitor/device.

- **Linux**: `parecord --device=<outputDeviceName>.monitor output.wav`
- **macOS**: `sox -t coreaudio <outputDeviceName> output.wav`

---

## Building

```bash
cd signal-call-tunnel && cargo build --release
```

The first build downloads a prebuilt WebRTC library (~100 MB) from Signal's
artifact server. Subsequent builds use the cached copy.

### RingRTC Patch

The build script (`build.rs`) automatically applies
`patches/ringrtc-disable-vpio.patch` to the local RingRTC source. This patch
adds a `RINGRTC_NO_VOICE_PROCESSING` environment variable check to cubeb's
`AudioDeviceModule`: when set, audio streams use `StreamPrefs::NONE` instead
of `StreamPrefs::VOICE`, skipping macOS VoiceProcessingIO (VPIO).

VPIO creates an aggregate audio device internally, which hangs when the
underlying device is a BlackHole virtual driver. Voice processing (AEC, AGC,
noise suppression) is also unnecessary for virtual audio. The tunnel sets
`RINGRTC_NO_VOICE_PROCESSING=1` at startup before any audio threads are
created.

The patch is idempotent — `build.rs` checks for the marker function before
applying and skips if already present.

### Prerequisites

- **macOS**: Install BlackHole virtual audio drivers (one-time, requires root):
  ```bash
  cd third-party/ringrtc
  sudo bin/virtual_audio.sh --setup --input-source signal_input --output-sink signal_output
  ```
  Install `sox` for audio playback/recording in tests: `brew install sox`

- **Linux**: PulseAudio must be running. Virtual audio modules are created
  automatically per call.
