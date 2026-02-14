This is a test plan for voice calling.

# Prerequisites

- signal-cli is registered on this computer
- You have Signal on your phone (or emulator) with a separate account
- You know the phone number of both accounts
- `signal-call-tunnel` Rust binary is built (see Build section)
- **macOS**: BlackHole virtual audio drivers installed (one-time setup):
  ```bash
  cd third-party/ringrtc
  sudo bin/virtual_audio.sh --setup --input-source signal_input --output-sink signal_output
  ```
  Install `sox`: `brew install sox`
- **Linux**: PulseAudio running (virtual audio modules are created automatically)

# Using the test harness

This directory contains a fully automated test harness that runs
scenarios A-E against an Android emulator with Signal installed, requiring
no manual interaction.

## Environment

| Component | Value |
|-----------|-------|
| signal-cli account | Set via `--signal-cli-account` or `SIGNAL_CLI_ACCOUNT` env var |
| Emulator Signal account | Set via `--emulator-account` or `EMULATOR_ACCOUNT` env var |
| Emulator AVD | `signal-test` (emulator-5554) |
| Emulator gRPC | localhost:8554 |
| Python virtualenv | `signal-call-tunnel` (pyenv, Python 3.9.6) |

## Quick start

```bash
# Run all five scenarios:
bash voice-test/run_e2e.sh --signal-cli-account +1... --emulator-account +1...

# Run specific scenarios:
bash voice-test/run_e2e.sh --signal-cli-account +1... --emulator-account +1... --scenarios A,B,E
```

`run_e2e.sh` handles everything automatically:
1. Pre-flight checks (builds exist, adb reachable, Signal installed)
2. Installs Python deps (`grpcio`, `grpcio-tools`)
3. Compiles emulator gRPC proto stubs (if needed)
4. Starts the emulator if not already running
5. Launches Signal on the emulator
6. Starts a signal-cli daemon on a test socket
7. Runs the selected test scenarios
8. Cleans up (kills daemon, orphaned tunnel processes)

## Scenarios

| ID | Name | What it tests |
|----|------|---------------|
| A | Outgoing call lifecycle | signal-cli places call -> emulator answers -> signal-cli hangs up |
| B | Incoming call lifecycle | Emulator places call -> signal-cli accepts -> signal-cli hangs up |
| C | Incoming call rejection | Emulator places call -> signal-cli rejects |
| D | Ring timeout | signal-cli places call -> nobody answers -> timeout after ~60s |
| E | Bidirectional audio | Connected call with 440Hz tone via virtual audio devices, Goertzel detection via emulator gRPC audio API |

## File structure

```
voice-test/
  run_e2e.sh              # Master orchestrator
  config.sh               # Shared environment config
  requirements.txt        # Python deps (grpcio, grpcio-tools)
  generate_proto.sh       # Compile emulator_controller.proto -> Python stubs
  e2e_test.py             # Main test runner (scenarios A-E)
  lib/
    signal_rpc.py         # signal-cli JSON-RPC client
    audio.py              # Tone generation + Goertzel frequency detection + WAV I/O
    emulator.py           # ADB-based Signal UI automation
    grpc_audio.py         # Emulator gRPC audio injection/capture
    proto/                # Generated protobuf stubs (created by generate_proto.sh)
```

## Debugging audio (Scenario E)

Scenario E saves WAV files to `voice-test/output/` for manual inspection:

```bash
ls voice-test/output/*.wav
afplay voice-test/output/e_tone_out_captured.wav   # 440Hz captured from emulator speaker
afplay voice-test/output/e_playout_received.wav    # Playout recorded from virtual output device
```

---

## Likely failure points

1. **TURN credentials** -- `getTurnServerInfo()` must fetch credentials from Signal's server. Without TURN, ICE may fail behind NAT.
2. **ICE connectivity** -- Symmetric NAT on both sides with no TURN = ICE failure. On the same LAN, ICE should complete within ~100ms.
3. **signal-call-tunnel not found** -- Set `SIGNAL_CALL_TUNNEL_BIN` env var to the binary path, or ensure it is on `PATH` or in `<install-dir>/bin/`.
4. **Ring timeout** -- 60 seconds to accept before auto-hangup.
5. **Virtual audio devices not found** -- macOS: BlackHole drivers not installed (run `sudo virtual_audio.sh --setup` first). Linux: PulseAudio not running.

# Manual Testing

## Build

```bash
# Build signal-cli
(cd third-party/signal-cli && ./gradlew installDist)

# Build the Rust call tunnel binary
cd signal-call-tunnel && cargo build --release && cd ..
```

The first Rust build will automatically download the prebuilt WebRTC library
(~100 MB) from Signal's artifact server. This is cached for subsequent builds.

## Start the daemon

Terminal 1 -- start the daemon with a JSON-RPC socket and verbose logging:

```bash
./third-party/signal-cli/build/install/signal-cli/bin/signal-cli -v daemon --socket
```

This binds a Unix domain socket at `$XDG_RUNTIME_DIR/signal-cli/socket`
(typically `~/.cache/signal-cli/socket` or `/run/user/$(id -u)/signal-cli/socket`).
Logs and received message notifications print to stdout.

You can specify a custom path: `--socket /tmp/signal-cli.sock`

Terminal 2 -- send JSON-RPC commands. Set the socket path to match:

```bash
SOCKET="${XDG_RUNTIME_DIR:-$HOME/.cache}/signal-cli/socket"
```

To send a one-shot command and get the response:

```bash
echo '{"jsonrpc":"2.0","method":"METHOD","id":1,"params":{}}' | socat - UNIX-CONNECT:$SOCKET
```

To open a persistent connection (needed for receiving notifications like incoming calls):

```bash
socat STDIO UNIX-CONNECT:$SOCKET
```

Then type JSON-RPC requests directly. Notifications (incoming calls, state changes)
will appear interleaved with responses.

---

## Test A: Outgoing call signaling and tunnel lifecycle

### A1. Start the call

```bash
echo '{"jsonrpc":"2.0","method":"startCall","id":1,"params":{"recipient":"+1YOURPHONENUMBER"}}' \
  | socat - UNIX-CONNECT:$SOCKET
```

**Expect in response:**
```json
{"jsonrpc":"2.0","result":{"callId":...,"state":"RINGING_OUTGOING","inputDeviceName":"signal_input_...","outputDeviceName":"signal_output_..."},"id":1}
```

**Expect in daemon logs (terminal 1):**
```
Started outgoing call {callId} to {recipient}
Spawned media tunnel for call {callId}
Tunnel ready for call {callId}
```

**Expect on phone:** Incoming call notification from the signal-cli account.

### A2. Answer on your phone

Pick up the call.

**Expect in daemon logs (key lines, in order):**
```
Received answer for call {callId}
Control event: sendOffer (outgoing call offer generated by RingRTC)
Control event: sendIce   (repeated, ICE candidates from RingRTC)
Control event: stateChange state=Connecting
Control event: stateChange state=Connected
```

### A3. Verify the media tunnel is running

```bash
ps aux | grep signal-call-tunnel
```

Should show a `signal-call-tunnel` process.

Check the socket directory (path from the `startCall` response):

```bash
ls -la /tmp/sc-*/
```

Should show `ctrl.sock` for the active call.

### A4. Hang up

From signal-cli (replace CALL_ID with the actual call ID):

```bash
echo '{"jsonrpc":"2.0","method":"hangupCall","id":2,"params":{"callId":CALL_ID}}' \
  | socat - UNIX-CONNECT:$SOCKET
```

Or hang up on your phone.

**Expect in daemon logs:**
```
Call {callId} ended: local_hangup       (if you hung up from signal-cli)
Call {callId} ended: remote_hangup      (if you hung up from phone)
Media tunnel for call {callId} exited with code 0
```

---

## Test B: Incoming call signaling, accept, and tunnel lifecycle

### B1. Open a persistent connection

You need to see incoming call notifications, so open a persistent connection:

```bash
socat STDIO UNIX-CONNECT:$SOCKET
```

### B2. Call from your phone

On your phone, start a Signal voice call to the signal-cli account.

**Expect in daemon logs (terminal 1):**
```
Incoming call {callId} from {yourPhoneNumber}
Spawned media tunnel for call {callId}
Tunnel ready for call {callId}
```

**Expect on the socat connection:** A `receive` notification containing a `callMessage`
with an `offerMessage`. The `callId` is in the offer's `id` field.

### B3. List calls to get the call ID

Type into the socat session:

```json
{"jsonrpc":"2.0","method":"listCalls","id":3}
```

**Expect:** A response with a call in state `RINGING_INCOMING`. Note the `callId`.

### B4. Accept the call

Type into the socat session (replace CALL_ID):

```json
{"jsonrpc":"2.0","method":"acceptCall","id":4,"params":{"callId":CALL_ID}}
```

**Expect in response:**
```json
{"jsonrpc":"2.0","result":{"callId":...,"state":"CONNECTING","inputDeviceName":"...","outputDeviceName":"..."},"id":4}
```

**Expect in daemon logs (key lines, in order):**
```
Accepted incoming call {callId}
Control event: sendAnswer (RingRTC generated answer with DH key)
Control event: sendIce    (repeated, ICE candidates from RingRTC)
Control event: stateChange state=Connecting
Control event: stateChange state=Connected
```

**Expect on phone:** Call shows as connected.

### B5. Verify the media tunnel is running

Same as A3.

### B6. Hang up

Same as A4.

---

## Test C: Incoming call rejection

### C1. Call from your phone (same as B1-B2)

### C2. Reject it

```json
{"jsonrpc":"2.0","method":"rejectCall","id":5,"params":{"callId":CALL_ID}}
```

**Expect in daemon logs:**
```
Call {callId} ended: rejected
```

**Expect on phone:** Call ends, shown as declined/busy.

---

## Test D: Unanswered call ring timeout

### D1. Start the call (same as A1)

Place an outgoing call from signal-cli. Do **not** answer on your phone.

```bash
echo '{"jsonrpc":"2.0","method":"startCall","id":1,"params":{"recipient":"+1YOURPHONENUMBER"}}' \
  | socat - UNIX-CONNECT:$SOCKET
```

### D2. Wait 60 seconds without answering

**Expect in daemon logs after ~60s:**
```
Call {callId} ring timeout
Call {callId} ended: ring_timeout
```

**Expect on phone:** Incoming call stops ringing.

---

## Success criteria

| Stage | How to verify |
|---|---|
| Signaling (offer/answer) | `sendOffer`/`sendAnswer` control events in logs |
| Media tunnel spawn | `Spawned media tunnel` in logs, `ps aux \| grep signal-call-tunnel` shows process |
| ICE connectivity | `stateChange state=Connected` in logs |
| Key derivation | Handled internally by RingRTC (x25519 DH + HKDF); no errors in tunnel stderr |
| Virtual audio | Devices enumerated by cubeb (check tunnel logs for device selection) |
| Call connected | Phone shows connected call, tunnel process alive |
| Clean teardown | `ended` in logs, `exited with code 0`, socket dir cleaned up |
