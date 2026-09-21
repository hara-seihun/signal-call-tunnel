# signal-call-tunnel

A Rust binary that wraps [RingRTC](https://github.com/signalapp/ringrtc) (Signal's WebRTC layer) to handle one voice call per process. It exposes a Unix control socket for signaling and uses virtual audio devices for audio I/O.

Designed to be spawned by [signal-cli](https://github.com/AsamK/signal-cli) — one tunnel process per call.

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

## Prerequisites

- Rust toolchain (edition 2024)
- Git (for submodule and build-time patching)

**macOS** — install BlackHole virtual audio drivers (one-time, requires root):

```bash
cd third-party/ringrtc
sudo bin/virtual_audio.sh --setup --input-source signal_input --output-sink signal_output
```

Optionally install `sox` for manual audio testing: `brew install sox`

**Linux** — PulseAudio must be running. No other setup needed; virtual audio modules are created and destroyed automatically per call.

## Installation

```bash
git clone --recurse-submodules https://github.com/visigoth/signal-call-tunnel.git
cargo install --path signal-call-tunnel/signal-call-tunnel
```

## Building from source

```bash
git submodule update --init
./build.sh            # release build (recommended)
```

`build.sh` applies the required RingRTC patches (see below) and then builds, so a
single invocation always succeeds. It also accepts cargo subcommands:

```bash
./build.sh --debug    # debug build
./build.sh test       # cargo test
```

The first build downloads a prebuilt WebRTC library (~100 MB) from Signal's artifact server. Subsequent builds use the cached copy.

### RingRTC patches

The RingRTC submodule is kept pristine in git. Two patches under
`signal-call-tunnel/patches/` are applied to its working tree at build time:

1. `ringrtc-disable-vpio.patch` — disables macOS VoiceProcessingIO (VPIO), which
   hangs with virtual audio devices.
2. `ringrtc-custom-audio-backend.patch` — adds a generic `CustomAudioDevice`
   extension point that the pipe audio backend (`src/pipe_audio.rs`) plugs into.

Order matters: the custom-audio-backend patch is generated against a tree that
already has the VPIO patch applied. Both patch applications are idempotent.

`build.rs` can also apply these patches as a fallback if you run `cargo build`
directly. However, cargo compiles the RingRTC dependency *before* running
`build.rs`, and the patches change RingRTC's public API — so a direct
`cargo build` on a pristine checkout must be run **twice** (the first run applies
the patches and stops with an explanatory message). Use `./build.sh` to avoid
this.


## Running

The tunnel reads a JSON config from stdin, then listens on a Unix control socket:

```bash
echo '{
  "call_id": 12345678,
  "is_outgoing": true,
  "control_socket_path": "/tmp/sc-abc/ctrl.sock",
  "control_token": "dG9rZW4=",
  "local_device_id": 1
}' | cargo run --release
```

Config fields:

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | `u64` | Unique call identifier |
| `is_outgoing` | `bool` | Whether this side initiated the call |
| `control_socket_path` | `string` | Path for the Unix control socket |
| `control_token` | `string` | Authentication token for control socket clients |
| `local_device_id` | `u32` | Signal device ID |
| `input_device_name` | `string?` | Virtual input device name (auto-generated on Linux, defaults to `signal_input` on macOS) |
| `output_device_name` | `string?` | Virtual output device name (auto-generated on Linux, defaults to `signal_output` on macOS) |

Set `RUST_LOG` for logging: `RUST_LOG=debug cargo run --release`

## Control Protocol

JSON-line protocol over the Unix control socket (one JSON object per line).

**Inbound messages** (from signal-cli to tunnel):
`auth`, `createOutgoingCall`, `proceed`, `receivedOffer`, `receivedAnswer`, `receivedIce`, `accept`, `hangup`

**Outbound messages** (from tunnel to signal-cli):
`ready`, `stateChange`, `sendOffer`, `sendAnswer`, `sendIce`, `sendHangup`, `sendBusy`

For the full protocol spec, call flows, and state machine, see `CALL_TUNNEL.md` in the signal-cli repository.

## Virtual Audio Devices

### PCM Parameters

| Parameter | Value |
|-----------|-------|
| Sample rate | 48,000 Hz |
| Channels | 1 (mono) |
| Sample format | 16-bit signed integer, little-endian |

### Linux (PulseAudio)

```bash
# Send audio to WebRTC
paplay --device=sink_for_signal_input_12345 tone.wav

# Record from WebRTC
parecord --device=signal_output_12345.monitor --rate=48000 --channels=1 --format=s16le captured.wav
```

### macOS (CoreAudio + BlackHole)

```bash
# Send audio to WebRTC
sox tone.wav -t coreaudio signal_input repeat -

# Record from WebRTC
sox -t coreaudio signal_output captured.wav trim 0 5
```

Connect to the virtual audio devices after the call reaches `CONNECTED` state.

## Testing

```bash
cd signal-call-tunnel && cargo test
```

## Documentation

- [docs/CALL_TUNNEL_IMPL.md](docs/CALL_TUNNEL_IMPL.md) — implementation details, encryption, audio device lifecycle

## License

AGPL-3.0-only — Copyright Signal Messenger, LLC
