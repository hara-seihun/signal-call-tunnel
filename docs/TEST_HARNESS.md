# E2E Voice Call Test Harness

Design document for the automated end-to-end voice call test infrastructure.

## Architecture Overview

The test harness has two layers: a shell orchestrator (`run_e2e.sh`) that manages
builds, processes, and cleanup, and a Python test runner (`e2e_test.py`) that
executes the five test scenarios.

```
run_e2e.sh (orchestrator)
  +-- pre-flight checks (builds, adb, Signal installed)
  +-- start signal-cli daemon on test socket
  +-- start logcat collection
  +-- launch e2e_test.py (test runner)
        +-- SignalRPC        -- JSON-RPC 2.0 client for signal-cli daemon
        +-- EmulatorControl  -- ADB-based Signal UI automation
        +-- AudioProcessing  -- tone generation + Goertzel detection
        +-- VirtualAudioHelpers -- platform-aware play/record via sox/paplay
        +-- GrpcAudio        -- emulator gRPC audio HAL client
```

### File layout

```
test/
  run_e2e.sh              # Shell orchestrator
  config.sh               # Shared environment config (paths, ports, accounts, frequencies)
  requirements.txt        # Python deps (grpcio, grpcio-tools)
  generate_proto.sh       # Compile emulator_controller.proto -> Python stubs
  e2e_test.py             # Test runner (scenarios A-E)
  lib/
    signal_rpc.py         # JSON-RPC 2.0 client for signal-cli
    audio.py              # Tone generation + Goertzel frequency detection + WAV I/O
    emulator.py           # ADB-based Signal UI automation
    grpc_audio.py         # Emulator gRPC audio injection/capture
    proto/                # Generated protobuf stubs (created by generate_proto.sh)
```

---

## Component Descriptions

### `run_e2e.sh` -- Shell Orchestrator

Handles everything outside the test scenarios themselves:

- **Pre-flight checks**: builds exist, adb reachable, Signal installed on emulator
- **Binary builds**: `gradlew installDist` (in `third-party/signal-cli/`) + `cargo build --release`
- **Python setup**: virtualenv activation, `pip install` from `requirements.txt`,
  proto stub generation
- **Daemon lifecycle**: starts signal-cli on a test socket, captures stdout to log file
- **Logcat collection**: background `adb logcat` filtered to Signal-related tags
- **Cleanup traps**: `trap cleanup EXIT INT TERM` ensures daemon, logcat, and orphaned
  tunnel processes are killed on any exit path
- **Stale process detection**: pre-run `lsof`/`pkill` to kill leftover processes from
  previous runs that might hold the test socket or tunnel ports

### `config.sh` -- Shared Configuration

Sourced by `run_e2e.sh`. Centralizes:

- Account phone numbers (`SIGNAL_CLI_ACCOUNT`, `EMULATOR_ACCOUNT`)
- Android SDK paths (adb, emulator binary)
- Emulator AVD name, gRPC port
- Test socket path (`/tmp/signal-cli-test.sock`)
- Audio test frequencies (440 Hz outgoing, 1000 Hz incoming)
- Log directory paths

### `e2e_test.py` -- Test Runner

Runs scenarios A-E with per-scenario logging, screen recording, and result reporting.

Key components:

- **`LogCollector`**: tracks log file positions per scenario; on failure, extracts only
  the relevant segment from each of the three log sources (signal-cli, daemon console,
  logcat) with categorized line filtering (tunnel messages, call-related, errors)
- **`ScreenRecorder`**: optional `--record` flag captures emulator screen via
  `adb screenrecord` for post-mortem debugging
- **Virtual audio helpers**: `play_to_device()` and `record_from_device()` provide
  platform-aware audio I/O using `sox` (macOS) or `paplay`/`parecord` (Linux)
- **Fail-fast mode**: default behavior stops after the first failure; `--no-fail-fast`
  runs the full suite
- **Clean state**: `wait_for_clean_state()` kills and relaunches Signal between
  scenarios, then waits for the WebSocket to reconnect (~20 s)

### `lib/signal_rpc.py` -- JSON-RPC Client

JSON-RPC 2.0 client over Unix socket for the signal-cli daemon.

- `start_call()`, `accept_call()`, `reject_call()`, `hangup_call()` -- call control
- `wait_for_state()` -- blocks until a `callEvent` notification with the target state
- `_read_lines()` -- generator yielding newline-delimited JSON; buffers partial reads
- `_wait_response()` -- waits for matching response ID, queues non-matching notifications
  in `_pending_events` for later consumption by `read_event()`

### `lib/audio.py` -- Audio Processing

Pure-Python audio utilities (no numpy dependency):

- `generate_tone(freq, duration)` -- sine wave PCM (48 kHz, 16-bit signed LE, mono)
- `goertzel_magnitude(samples, freq)` -- O(n) single-frequency energy detector
- `detect_tone(pcm, expected_freq)` -- scans a +/-100 Hz window around the target
  frequency, compares peak to noise floor (SNR threshold = 3.0)
- `pcm_to_wav()` -- write raw PCM bytes to a WAV file
- `wav_to_pcm()` -- read a WAV file back as raw PCM bytes
- `rms_level()` -- RMS amplitude normalized to [0.0, 1.0]

The +/-100 Hz scan window compensates for Opus codec frequency shifts (~30-50 Hz).
Noise is measured from bins 400-600 Hz away from the signal to avoid adjacent-band
leakage.

### `lib/emulator.py` -- Emulator UI Automation

ADB-based Signal UI automation using dynamic element lookup and logcat polling.

- `_dump_ui()` -- runs `adb shell uiautomator dump /sdcard/window_dump.xml` then reads
  the file back (piping to `/dev/stdout` is unreliable on many emulators). Requires
  `adb root` on API 34+. Retries once on failure with a 5-second timeout.
- `_find_element()` / `_tap_element()` -- search the XML hierarchy for elements by
  text, content-desc, resource-id, or class name, and tap their center coordinates
- `_dismiss_permission_dialogs()` -- dismiss camera/mic permission prompts on the
  pre-join call screen by tapping "Not now" / "Deny"
- `launch_signal()` -- uses `monkey` launcher intent (not unexported `MainActivity`)
- `open_conversation()` -- kill, relaunch, find first conversation row via uiautomator
- `tap_call_button()` -- find call icon by content-desc, dismiss permission dialogs,
  then tap "Start Call" on the pre-join screen
- `answer_incoming_call()` -- expand notification shade, find "Answer"/"Accept" button
  via uiautomator (heads-up notification is invisible to uiautomator, but shade actions
  are visible). Falls back to `HEADSETHOOK` keyevent and `cmd telecom` commands.
- `reject_incoming_call()` -- uses `ENDCALL` keyevent with `cmd telecom end-call` fallback
- `_wait_for_incoming_call()` -- poll logcat for `handleReceivedOffer` -> `LOCAL_RINGING`
- Foreground verification via `dumpsys window | grep mCurrentFocus`

UI element coordinates are discovered dynamically via `uiautomator dump`, making the
harness portable across emulator display resolutions.

### `lib/grpc_audio.py` -- gRPC Audio HAL Client

Client for the Android emulator's gRPC audio streaming API.

- `inject_audio(pcm)` -- client-streaming RPC to inject PCM into the virtual mic,
  paced at ~8 ms per frame (slightly under 10 ms to avoid underruns)
- `capture_audio(seconds)` -- server-streaming RPC to capture speaker output
- **Auth discovery**: tries unauthenticated first (`-grpc` flag); on
  `UNAUTHENTICATED` error, searches `~/.android/avd/running/` and `$TMPDIR/avd/running/`
  for `grpc.token` or `.jwk` files
- 4-second settling delay after gRPC connection for HAL initialization

---

## Test Scenarios

| ID | Name | Flow |
|----|------|------|
| A | Outgoing call lifecycle | signal-cli places call -> emulator answers -> signal-cli hangs up |
| B | Incoming call lifecycle | Emulator places call -> signal-cli accepts -> signal-cli hangs up |
| C | Incoming call rejection | Emulator places call -> signal-cli rejects (verify hangup reason) |
| D | Ring timeout | signal-cli places call -> nobody answers -> verify timeout after ~60 s |
| E | Bidirectional audio | Connected call with tone generation via virtual audio devices + Goertzel detection via gRPC |

Scenario E verifies both directions of the audio pipeline:

1. **signal-cli -> emulator**: play 440 Hz tone WAV into the virtual input device
   (via `sox`/`paplay`), capture from the emulator's speaker via gRPC, verify with
   Goertzel
2. **emulator -> signal-cli**: record from the virtual output device (via
   `sox`/`parecord`), verify playout data is non-empty and at the expected rate

---

## Key Design Decisions & Lessons Learned

### Emulator UI Automation

- **Use `monkey` launcher intent, not unexported `MainActivity`**: the
  `am start` command with `MainActivity` throws `SecurityException` because it is not
  exported. `monkey -p org.thoughtcrime.securesms 1` uses the default launcher intent.
- **Dynamic element lookup via `uiautomator dump`**: dumps to
  `/sdcard/window_dump.xml` then reads back with `cat` (piping to `/dev/stdout` is
  unreliable). Requires `adb root` on API 34+ (run once in `run_e2e.sh`). Searches by
  text, content-desc, or resource-id. Portable across display resolutions. The dump has
  a 5-second timeout and retries once on failure.
- **Wait for `handleLocalRinging` logcat event, not `handleReceivedOffer`**: the offer
  event fires before the UI is ready to accept taps. Waiting for `LOCAL_RINGING` ensures
  the incoming call notification is visible.
- **Notification shade for call answer**: the heads-up notification is invisible to
  `uiautomator dump` (it's rendered by SystemUI, not the app). Expanding the notification
  shade with `cmd statusbar expand-notifications` makes the "Answer"/"Accept" action
  buttons visible in the SystemUI hierarchy. Falls back to `HEADSETHOOK` keyevent.
  Signal does NOT use Android's Telecom framework, so `cmd telecom accept-ringing-call`
  is a no-op.
- **Dismiss permission dialogs on pre-join screen**: Signal may show camera/microphone
  permission prompts when the pre-join call activity opens. `_dismiss_permission_dialogs()`
  taps "Not now" to dismiss them before looking for the "Start Call" button.
- **Non-coordinate call reject**: uses `ENDCALL` keyevent with `cmd telecom end-call`
  fallback. No screen coordinates needed.

### State Management

- **State-based polling replaces all `time.sleep()` calls**: every wait polls for a
  specific state (RPC event, logcat message, or socket data) with a timeout, rather
  than sleeping for a fixed duration.
- **`wait_for_clean_state()` uses time-based logcat filtering (`-T timestamp`), not
  `logcat -c`**: clearing the logcat buffer races with the system logger writing new
  entries. Filtering by timestamp is deterministic.
- **Kill + relaunch Signal between scenarios**: ensures WebSocket reconnection and a
  clean call state. Without this, leftover state from a previous call causes the next
  scenario to fail.
- **20 s WebSocket reconnect timeout**: after Signal restarts, signal-cli's WebSocket
  needs time to reconnect before it can receive incoming calls.

### Audio & Media Pipeline

- **Virtual audio device enumeration timing**: cubeb needs time to detect newly created
  virtual devices. The tunnel loops on `get_audio_playout_devices()` /
  `get_audio_recording_devices()` at 100 ms intervals until devices appear.
- **Platform-specific audio tools**: `sox` on macOS (CoreAudio), `paplay`/`parecord` on
  Linux (PulseAudio). The test harness auto-selects based on `platform.system()`.
- **Opus codec shifts frequencies +/-30-50 Hz**: the Goertzel detector scans a +/-100 Hz
  window around the target frequency to tolerate codec artifacts.
- **gRPC audio HAL needs 4 s settling delay**: the emulator's audio subsystem needs
  time to initialize after a gRPC connection. Without the delay, capture returns silence.
- **Retry logic for HAL flakiness**: scenario E retries once on failure to handle
  transient emulator audio issues.
- **gRPC auth: `-grpc` flag for unauthenticated, JWT token auto-discovery as fallback**:
  the emulator gRPC port may or may not require authentication depending on how it was
  launched. The client tries unauthenticated first, then discovers tokens from the
  emulator's runtime directories.

### Test Infrastructure

- **Trap `INT`/`TERM`/`EXIT` for cleanup; pre-run stale process detection via
  `lsof`/`pkill`**: ensures no orphaned daemons, logcat processes, or tunnel subprocesses
  survive across runs.
- **Export (not just set) environment variables for child processes**: `SIGNAL_CALL_TUNNEL_BIN`
  must be exported so the signal-cli daemon's subprocess spawner can find the tunnel binary.
- **Buffered I/O must drain buffer before calling `recv()`**: the `_read_lines()` fix
  (commit 6085ca0b) -- previously, buffered data from a prior `recv()` call was lost when
  the generator was re-entered, causing missed JSON-RPC responses.
- **Fail-fast by default; `--no-fail-fast` for full suite runs**: most development
  workflows want to stop at the first failure. CI or full validation runs use
  `--no-fail-fast`.
- **`LogCollector`**: per-scenario extraction from three log sources (signal-cli output,
  daemon console, logcat) with categorized diagnostics (tunnel lines, call-related, errors).
- **`--record` flag for emulator screen capture**: `adb screenrecord` during test
  execution produces video for post-mortem debugging of UI automation failures.

### Signal-cli Bugs Found via E2E Testing

These bugs were found and fixed in separate commits after the test harness was complete:

- **ICE credential mismatch**: RingRTC requires consistent peer ID across all API calls.
  Using different IDs for `createOutgoingCall` and `proceed` caused ICE to fail silently.
- **SRTP key mismatch**: identity keys are 33 bytes with a `0x05` prefix in the Signal
  protocol, but RingRTC expects 32-byte raw keys. Passing the prefixed key caused SRTP
  decryption failure.
- **Multi-device hangup**: `sendHangup` is a protocol message to other devices, not a
  local state change. The call manager was treating it as a local hangup.
- **Call ID overflow**: `BigInteger` -> `Long` cast truncated call IDs; unsigned
  serialization was needed for Rust's `u64`.
- **Accept race**: calling `acceptCall` before the tunnel reports `Ringing` state causes
  RingRTC to drop the accept. The fix defers `acceptCall` until ICE is connected.
