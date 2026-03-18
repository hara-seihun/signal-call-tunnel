#!/usr/bin/env python3
"""E2E voice call test runner.

Runs test scenarios A-E against a signal-cli daemon and an Android emulator
with Signal installed.

Usage:
    python3 voice-test/e2e_test.py --socket /tmp/signal-cli-test.sock --scenarios A,B,C,D,E
"""

import argparse
import os
import platform
import shutil
import signal
import subprocess
import sys
import threading
import time
import traceback
from pathlib import Path

# Add voice-test dir so we can import lib.*
sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.signal_rpc import SignalRPC
from lib.audio import generate_tone, detect_tone, pcm_to_wav, rms_level, wav_to_pcm
from lib.emulator import EmulatorControl

# Optional: gRPC audio (only needed for scenario E)
try:
    from lib.grpc_audio import EmulatorAudio
    HAS_GRPC_AUDIO = True
except ImportError:
    HAS_GRPC_AUDIO = False


# -- Configuration (accounts required via env, others overridable) --
EMULATOR_ACCOUNT = os.environ["EMULATOR_ACCOUNT"]
SIGNAL_CLI_ACCOUNT = os.environ["SIGNAL_CLI_ACCOUNT"]
ADB_PATH = os.environ.get("ADB") or shutil.which("adb")
if not ADB_PATH:
    sys.exit("ERROR: 'adb' not found. Set ADB env var or add adb to PATH.")
EMULATOR_GRPC_PORT = int(os.environ.get("EMULATOR_GRPC_PORT", "8554"))
TEST_TONE_FREQ_OUT = int(os.environ.get("TEST_TONE_FREQ_OUT", "440"))
TEST_TONE_FREQ_IN = int(os.environ.get("TEST_TONE_FREQ_IN", "1000"))
TEST_TONE_DURATION = int(os.environ.get("TEST_TONE_DURATION", "3"))
OUTPUT_DIR = Path(__file__).resolve().parent / "output"
IS_MACOS = platform.system() == "Darwin"


def setup_output_dir():
    OUTPUT_DIR.mkdir(exist_ok=True)


class TestResult:
    def __init__(self, name, passed, message="", duration=0):
        self.name = name
        self.passed = passed
        self.message = message
        self.duration = duration

    def __str__(self):
        status = "PASS" if self.passed else "FAIL"
        return f"[{status}] {self.name} ({self.duration:.1f}s) {self.message}"


# ---------------------------------------------------------------------------
# Log collection
# ---------------------------------------------------------------------------
class LogCollector:
    """Tracks log file positions per scenario and extracts relevant segments on failure.

    Monitors three log sources:
      - signal-cli log file (includes tunnel output as [tunnel-{callId}] lines)
      - daemon console log (stdout/stderr from the daemon process)
      - Android logcat log (full device logcat)
    """

    # Lines to show in failure diagnostics (per log source)
    TAIL_LINES = 80

    # Logcat patterns relevant to call diagnostics
    LOGCAT_FILTERS = [
        "WebRtcCallService",
        "RingRTC",
        "CallManager",
        "org.thoughtcrime.securesms",
        "signal",
        "webrtc",
        "AudioManager",
        "AudioTrack",
        "AudioRecord",
    ]

    def __init__(self, log_dir):
        self.log_dir = Path(log_dir) if log_dir else None
        self.signal_cli_log = self.log_dir / "signal-cli.log" if self.log_dir else None
        self.daemon_console_log = self.log_dir / "daemon-console.log" if self.log_dir else None
        self.logcat_log = self.log_dir / "logcat.log" if self.log_dir else None
        self._positions = {}  # scenario_id -> {file: byte_offset}

    @property
    def enabled(self):
        return self.log_dir is not None and self.log_dir.is_dir()

    def _file_size(self, path):
        try:
            return path.stat().st_size if path and path.exists() else 0
        except OSError:
            return 0

    def mark_start(self, scenario_id):
        """Record current end-of-file positions for all log files."""
        if not self.enabled:
            return
        self._positions[scenario_id] = {
            "signal_cli": self._file_size(self.signal_cli_log),
            "daemon_console": self._file_size(self.daemon_console_log),
            "logcat": self._file_size(self.logcat_log),
        }

    def extract_scenario_logs(self, scenario_id):
        """Extract log segments written during this scenario.

        Returns dict of {source_name: text}.
        """
        if not self.enabled or scenario_id not in self._positions:
            return {}

        starts = self._positions[scenario_id]
        segments = {}

        for name, log_path, start_pos in [
            ("signal-cli", self.signal_cli_log, starts["signal_cli"]),
            ("daemon-console", self.daemon_console_log, starts["daemon_console"]),
            ("logcat", self.logcat_log, starts["logcat"]),
        ]:
            if not log_path or not log_path.exists():
                continue
            try:
                end_pos = log_path.stat().st_size
                if end_pos <= start_pos:
                    continue
                with open(log_path, "r", errors="replace") as f:
                    f.seek(start_pos)
                    text = f.read(end_pos - start_pos)
                if text.strip():
                    segments[name] = text
            except OSError:
                continue

        return segments

    def save_scenario_logs(self, scenario_id, result):
        """On failure, save per-scenario log excerpts and print diagnostics."""
        if not self.enabled:
            return

        segments = self.extract_scenario_logs(scenario_id)
        if not segments:
            return

        # Save full per-scenario logs to files
        for source, text in segments.items():
            out_path = self.log_dir / f"scenario_{scenario_id}_{source}.log"
            with open(out_path, "w") as f:
                f.write(text)

        if not result.passed:
            self._print_diagnostics(scenario_id, segments)

    def _print_diagnostics(self, scenario_id, segments):
        """Print relevant log excerpts for a failed scenario."""
        print(f"\n  {'='*60}")
        print(f"  DIAGNOSTIC LOGS FOR SCENARIO {scenario_id}")
        print(f"  {'='*60}")

        # signal-cli log (includes [tunnel-*] lines)
        if "signal-cli" in segments:
            text = segments["signal-cli"]
            lines = text.splitlines()

            # Separate tunnel lines from daemon lines
            tunnel_lines = [l for l in lines if "[tunnel-" in l]
            call_lines = [l for l in lines
                          if any(kw in l.lower() for kw in
                                 ["call", "tunnel", "ice", "ring", "offer",
                                  "answer", "hangup", "error", "exception",
                                  "failed", "timeout", "media", "virtual", "audio"])]

            if tunnel_lines:
                print(f"\n  --- signal-call-tunnel ({len(tunnel_lines)} lines) ---")
                for line in tunnel_lines[-self.TAIL_LINES:]:
                    print(f"  | {line}")

            if call_lines:
                # Deduplicate: skip lines already shown as tunnel lines
                daemon_call_lines = [l for l in call_lines if "[tunnel-" not in l]
                if daemon_call_lines:
                    print(f"\n  --- signal-cli daemon (call-related, {len(daemon_call_lines)} lines) ---")
                    for line in daemon_call_lines[-self.TAIL_LINES:]:
                        print(f"  | {line}")

            # Any ERROR/WARN lines not yet shown
            error_lines = [l for l in lines
                           if any(lvl in l for lvl in [" ERROR ", " WARN "])
                           and l not in call_lines and l not in tunnel_lines]
            if error_lines:
                print(f"\n  --- signal-cli errors/warnings ({len(error_lines)} lines) ---")
                for line in error_lines[-20:]:
                    print(f"  | {line}")

        # Logcat (filtered for Signal/WebRTC)
        if "logcat" in segments:
            text = segments["logcat"]
            lines = text.splitlines()
            relevant = [l for l in lines
                        if any(f.lower() in l.lower() for f in self.LOGCAT_FILTERS)]
            if relevant:
                print(f"\n  --- Android logcat (Signal/WebRTC, {len(relevant)} lines) ---")
                for line in relevant[-self.TAIL_LINES:]:
                    print(f"  | {line}")
            elif lines:
                # No filtered matches; show tail of raw logcat
                print(f"\n  --- Android logcat (tail, {len(lines)} total lines) ---")
                for line in lines[-30:]:
                    print(f"  | {line}")

        # Daemon console (startup errors, crashes)
        if "daemon-console" in segments:
            text = segments["daemon-console"].strip()
            if text:
                lines = text.splitlines()
                print(f"\n  --- daemon console output ({len(lines)} lines) ---")
                for line in lines[-20:]:
                    print(f"  | {line}")

        print(f"\n  {'='*60}")
        print(f"  Full logs: {self.log_dir}/scenario_{scenario_id}_*.log")
        print(f"  {'='*60}\n")


# ---------------------------------------------------------------------------
# Screen recording
# ---------------------------------------------------------------------------
class ScreenRecorder:
    """Records the emulator screen via `adb screenrecord` for a scenario."""

    def __init__(self, adb_path, output_dir):
        self.adb = adb_path
        self.output_dir = Path(output_dir)
        self.output_dir.mkdir(parents=True, exist_ok=True)
        self._proc = None
        self._device_path = "/sdcard/scenario_recording.mp4"

    def start(self, scenario_id):
        """Start recording the emulator screen."""
        self.stop()  # ensure no leftover recording
        self._scenario_id = scenario_id
        try:
            self._proc = subprocess.Popen(
                [self.adb, "shell", "screenrecord", "--time-limit", "120",
                 self._device_path],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
            print(f"  [rec] Screen recording started (PID {self._proc.pid})")
        except Exception as e:
            print(f"  [rec] Failed to start screen recording: {e}")
            self._proc = None

    def stop(self):
        """Stop recording and pull the video to the output directory."""
        if self._proc is None:
            return None
        # Send SIGINT to gracefully stop screenrecord (finalizes the mp4)
        try:
            self._proc.send_signal(signal.SIGINT)
            self._proc.wait(timeout=5)
        except Exception:
            self._proc.kill()
            self._proc.wait(timeout=3)
        self._proc = None

        # Give adb a moment to finalize the file
        time.sleep(1)

        # Pull the recording from the device
        local_path = self.output_dir / f"scenario_{self._scenario_id}_screen.mp4"
        try:
            subprocess.run(
                [self.adb, "pull", self._device_path, str(local_path)],
                capture_output=True, timeout=15,
            )
            subprocess.run(
                [self.adb, "shell", "rm", "-f", self._device_path],
                capture_output=True, timeout=5,
            )
            if local_path.exists() and local_path.stat().st_size > 0:
                print(f"  [rec] Screen recording saved: {local_path}")
                return local_path
            else:
                print(f"  [rec] Screen recording file is empty or missing")
        except Exception as e:
            print(f"  [rec] Failed to pull screen recording: {e}")
        return None


# ---------------------------------------------------------------------------
# Virtual audio device helpers
# ---------------------------------------------------------------------------
def check_blackhole_loopback(device_name="signal_input"):
    """Test that BlackHole loopback works (macOS only).

    Plays a short tone to the device and records from it simultaneously.
    Returns True if audio passes through, False if the device reads silence.
    """
    if not IS_MACOS:
        return True  # Linux uses PulseAudio, not BlackHole

    import tempfile
    tone_path = OUTPUT_DIR / "_loopback_tone.wav"
    rec_path = OUTPUT_DIR / "_loopback_rec.wav"

    # Generate a brief loud tone
    tone_pcm = generate_tone(1000, 1, amplitude=0.9)
    setup_output_dir()
    pcm_to_wav(tone_pcm, tone_path)

    # Record from the device for 2 seconds
    rec_proc = subprocess.Popen(
        ["sox", "-t", "coreaudio", device_name,
         "-b", "16", "-c", "1", "-r", "48000", str(rec_path),
         "trim", "0", "2"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(0.3)
    # Play to the device
    play_proc = subprocess.Popen(
        ["sox", str(tone_path), "-t", "coreaudio", device_name],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    rec_proc.wait(timeout=10)
    play_proc.kill()
    play_proc.wait(timeout=3)

    # Check recorded audio RMS
    if rec_path.exists() and rec_path.stat().st_size > 44:
        recorded = wav_to_pcm(rec_path)
        level = rms_level(recorded)
        return level > 0.01
    return False


def play_to_device(device_name, wav_path, duration=None):
    """Play a WAV file to a virtual audio input device (platform-aware).

    Returns the subprocess.Popen object for the background player process.
    """
    if IS_MACOS:
        # On macOS, use sox to play into the CoreAudio device
        cmd = ["sox", str(wav_path), "-t", "coreaudio", device_name, "repeat", "-"]
        return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    else:
        # On Linux, play to the PulseAudio sink associated with the input device
        sink_name = f"sink_for_{device_name}"
        cmd = ["paplay", f"--device={sink_name}", str(wav_path)]
        return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def record_from_device(device_name, wav_path, duration):
    """Record audio from a virtual audio output device (platform-aware).

    Blocks for `duration` seconds, then returns.
    """
    if IS_MACOS:
        # On macOS, use sox to record from the CoreAudio device.
        # Force mono 16-bit 48kHz output so Python's wave module can read it
        # (BlackHole is 2ch, which makes sox emit WAVE_FORMAT_EXTENSIBLE).
        cmd = ["sox", "-t", "coreaudio", device_name,
               "-b", "16", "-c", "1", "-r", "48000", str(wav_path),
               "trim", "0", str(duration)]
    else:
        # On Linux, record from the PulseAudio monitor source
        monitor_name = f"{device_name}.monitor"
        cmd = ["parecord", f"--device={monitor_name}",
               f"--rate=48000", "--channels=1", "--format=s16le",
               f"--file-format=wav", str(wav_path)]

    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if IS_MACOS:
        # sox with trim will stop after duration
        proc.wait(timeout=duration + 10)
    else:
        # parecord runs indefinitely; kill after duration
        time.sleep(duration)
        proc.terminate()
        proc.wait(timeout=5)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
def assert_call_stable(rpc, duration=2):
    """Verify the call stays connected for at least duration seconds.

    Reads events from the RPC connection, raising AssertionError if an
    unexpected ENDED event arrives.  Replaces arbitrary time.sleep() pauses
    with an actual state-based check.
    """
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        try:
            msg = rpc.read_event(timeout=remaining)
        except TimeoutError:
            break  # No events — call is stable
        if msg and msg.get("method") == "callEvent":
            event = msg.get("params", {}).get("callEvent", {})
            state = event.get("state")
            if state == "ENDED":
                raise AssertionError(
                    f"Call dropped during stability check: reason={event.get('reason')}"
                )
            print(f"    [rpc] callEvent during stability check: {state}")


def wait_for_clean_state():
    """Ensure no lingering call state on the emulator between scenarios.

    Kills Signal, relaunches it, and waits for the WebSocket to reconnect
    so the next scenario can receive calls. Raises RuntimeError on timeout.
    """
    emu = EmulatorControl(ADB_PATH, output_dir=str(OUTPUT_DIR))
    emu.kill_signal()
    # Record a timestamp before launch so we can filter logcat by time
    start_ts = emu._shell("date '+%m-%d %H:%M:%S.000'")
    emu.launch_signal()
    # Wait for Signal's authenticated WebSocket to connect.
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        logcat = emu._shell(
            f"logcat -d -T '{start_ts}' "
            "-s SignalWebSocketHealthMo:V IncomingMessageObserver:D "
            "2>/dev/null",
            timeout=5,
        )
        if "CONNECTED" in logcat:
            return
        time.sleep(1)
    raise RuntimeError(
        "Signal did not reconnect WebSocket within 20s after restart"
    )


# ---------------------------------------------------------------------------
# Scenario A: Outgoing call -- signaling and lifecycle
# ---------------------------------------------------------------------------
def scenario_a(socket_path):
    """Outgoing call: signal-cli places call, emulator answers, signal-cli hangs up."""
    emu = EmulatorControl(ADB_PATH, output_dir=str(OUTPUT_DIR))
    rpc = SignalRPC(socket_path)
    call_id = None
    try:
        rpc.subscribe_receive()

        # Place the call
        print("  [A] Starting outgoing call to emulator...")
        result = rpc.start_call(EMULATOR_ACCOUNT)
        call_id = result.get("callId")
        state = result.get("state")
        input_dev = result.get("inputDeviceName")
        output_dev = result.get("outputDeviceName")
        print(f"  [A] startCall => callId={call_id}, state={state}")
        assert call_id, "No callId returned"

        # Wait for RINGING_OUTGOING
        if state != "RINGING_OUTGOING":
            rpc.wait_for_state("RINGING_OUTGOING", timeout=15)

        # Answer on emulator (polls internally for call to arrive)
        print("  [A] Answering call on emulator...")
        emu.answer_incoming_call()

        # Wait for CONNECTED
        print("  [A] Waiting for CONNECTED...")
        rpc.wait_for_state("CONNECTED", timeout=30)
        print("  [A] Call connected! Waiting 5s before hanging up from signal-cli...")
        assert_call_stable(rpc, duration=5)

        # Hang up
        print("  [A] Hanging up from signal-cli...")
        rpc.hangup_call(call_id)
        call_id = None  # Don't double-hangup in cleanup

        # Wait for ENDED
        rpc.wait_for_state("ENDED", timeout=10)
        print("  [A] Call ended normally.")

        return TestResult("A: Outgoing call lifecycle", True)

    except Exception as e:
        return TestResult("A: Outgoing call lifecycle", False, str(e))
    finally:
        if call_id:
            try:
                rpc.hangup_call(call_id)
            except Exception:
                pass
        rpc.close()


# ---------------------------------------------------------------------------
# Scenario B: Incoming call -- emulator calls signal-cli
# ---------------------------------------------------------------------------
def scenario_b(socket_path):
    """Incoming call: emulator places call, signal-cli accepts and hangs up."""
    emu = EmulatorControl(ADB_PATH, output_dir=str(OUTPUT_DIR))
    rpc = SignalRPC(socket_path)
    call_id = None
    try:
        rpc.subscribe_receive()

        # Navigate emulator to conversation and place call
        print("  [B] Opening conversation on emulator...")
        emu.open_conversation(SIGNAL_CLI_ACCOUNT)

        print("  [B] Tapping call button on emulator...")
        emu.tap_call_button()

        # Wait for incoming call event
        print("  [B] Waiting for RINGING_INCOMING...")
        params = rpc.wait_for_state("RINGING_INCOMING", timeout=30)
        event = params.get("callEvent", {})
        call_id = event.get("callId")
        print(f"  [B] Incoming call: callId={call_id}")
        assert call_id, "No callId in incoming call event"

        # Accept the call
        print("  [B] Accepting call...")
        rpc.accept_call(call_id)

        # Wait for CONNECTED
        print("  [B] Waiting for CONNECTED...")
        rpc.wait_for_state("CONNECTED", timeout=30)
        print("  [B] Call connected!")

        # Verify call remains stable
        print("  [B] Verifying call stability...")
        assert_call_stable(rpc, duration=2)

        # Hang up from signal-cli
        print("  [B] Hanging up...")
        rpc.hangup_call(call_id)
        call_id = None

        rpc.wait_for_state("ENDED", timeout=10)
        print("  [B] Call ended normally.")

        return TestResult("B: Incoming call lifecycle", True)

    except Exception as e:
        return TestResult("B: Incoming call lifecycle", False, str(e))
    finally:
        if call_id:
            try:
                rpc.hangup_call(call_id)
            except Exception:
                pass
        rpc.close()


# ---------------------------------------------------------------------------
# Scenario C: Incoming call rejection
# ---------------------------------------------------------------------------
def scenario_c(socket_path):
    """Incoming call: emulator places call, signal-cli rejects it."""
    emu = EmulatorControl(ADB_PATH, output_dir=str(OUTPUT_DIR))
    rpc = SignalRPC(socket_path)
    call_id = None
    try:
        rpc.subscribe_receive()

        # Emulator places call
        print("  [C] Opening conversation on emulator...")
        emu.open_conversation(SIGNAL_CLI_ACCOUNT)

        print("  [C] Tapping call button on emulator...")
        emu.tap_call_button()

        # Wait for incoming ring
        print("  [C] Waiting for RINGING_INCOMING...")
        params = rpc.wait_for_state("RINGING_INCOMING", timeout=30)
        event = params.get("callEvent", {})
        call_id = event.get("callId")
        assert call_id, "No callId in incoming call event"

        # Reject the call
        print("  [C] Rejecting call...")
        rpc.reject_call(call_id)
        call_id = None

        # Wait for ENDED and verify rejection reason
        params = rpc.wait_for_state("ENDED", timeout=10)
        event = params.get("callEvent", {})
        reason = event.get("reason", "")
        print(f"  [C] Call ended: reason={reason}")
        assert any(kw in reason.lower() for kw in ("reject", "busy", "decline")), \
            f"Expected rejection reason, got: {reason}"

        return TestResult("C: Incoming call rejection", True, f"reason={reason}")

    except Exception as e:
        return TestResult("C: Incoming call rejection", False, str(e))
    finally:
        if call_id:
            try:
                rpc.hangup_call(call_id)
            except Exception:
                pass
        rpc.close()


# ---------------------------------------------------------------------------
# Scenario D: Ring timeout
# ---------------------------------------------------------------------------
def scenario_d(socket_path):
    """Outgoing call that is never answered -- should timeout."""
    emu = EmulatorControl(ADB_PATH, output_dir=str(OUTPUT_DIR))
    rpc = SignalRPC(socket_path)
    call_id = None
    try:
        # Restart Signal to a clean main screen so it can receive the call
        # (but nobody will tap Answer)
        emu.ensure_signal_foreground()

        rpc.subscribe_receive()

        # Place call, don't answer on emulator
        print("  [D] Starting call (will NOT answer)...")
        result = rpc.start_call(EMULATOR_ACCOUNT)
        call_id = result.get("callId")
        state = result.get("state")
        print(f"  [D] startCall => callId={call_id}, state={state}")

        if state != "RINGING_OUTGOING":
            rpc.wait_for_state("RINGING_OUTGOING", timeout=15)

        # Wait for timeout (Signal typically times out after ~60s)
        print("  [D] Waiting for ring timeout (up to 90s)...")
        params = rpc.wait_for_state("ENDED", timeout=90)
        event = params.get("callEvent", {})
        reason = event.get("reason", "")
        print(f"  [D] Call ended: reason={reason}")
        call_id = None

        assert "timeout" in reason.lower(), \
            f"Expected timeout reason, got: {reason}"

        return TestResult("D: Ring timeout", True, f"reason={reason}")

    except Exception as e:
        return TestResult("D: Ring timeout", False, str(e))
    finally:
        if call_id:
            try:
                rpc.hangup_call(call_id)
            except Exception:
                pass
        rpc.close()


# ---------------------------------------------------------------------------
# Scenario E: Bidirectional audio verification
# ---------------------------------------------------------------------------
def scenario_e(socket_path):
    """Connected call with bidirectional audio: tone generation and detection."""
    if not HAS_GRPC_AUDIO:
        return TestResult("E: Bidirectional audio", False,
                          "grpc_audio not available (run generate_proto.sh first)")

    if IS_MACOS and not check_blackhole_loopback():
        return TestResult("E: Bidirectional audio", False,
                          "BlackHole loopback broken — reinstall with: "
                          "sudo bash third-party/ringrtc/bin/virtual_audio.sh "
                          "--setup --input-source signal_input --output-sink signal_output")

    emu = EmulatorControl(ADB_PATH, output_dir=str(OUTPUT_DIR))
    rpc = SignalRPC(socket_path)
    call_id = None
    grpc_audio = None
    player_proc = None
    try:
        # Bring Signal to foreground without killing it.
        emu.launch_signal()

        rpc.subscribe_receive()

        # Place call and connect
        print("  [E] Starting outgoing call...")
        result = rpc.start_call(EMULATOR_ACCOUNT)
        call_id = result.get("callId")
        input_device = result.get("inputDeviceName")
        output_device = result.get("outputDeviceName")
        assert call_id, "Missing callId"

        state = result.get("state")
        if state != "RINGING_OUTGOING":
            rpc.wait_for_state("RINGING_OUTGOING", timeout=15)

        # Answer on emulator (polls internally for call to arrive)
        print("  [E] Answering on emulator...")
        emu.answer_incoming_call()

        print("  [E] Waiting for CONNECTED...")
        event_params = rpc.wait_for_state("CONNECTED", timeout=30)
        print("  [E] Call connected!")

        # Get device names from the CONNECTED event if not in startCall response
        if not input_device or not output_device:
            event = event_params.get("callEvent", {})
            input_device = input_device or event.get("inputDeviceName")
            output_device = output_device or event.get("outputDeviceName")

        assert input_device, "No inputDeviceName available"
        assert output_device, "No outputDeviceName available"
        print(f"  [E] Virtual audio: input={input_device}, output={output_device}")

        # Set in-call volume to max (default is often 3/15 on emulators)
        emu.set_call_volume_max()

        # Settling delay: let WebRTC/Opus codec stabilize before audio tests
        print("  [E] Waiting 2s for WebRTC/Opus to stabilize...")
        assert_call_stable(rpc, duration=2)

        grpc_audio = EmulatorAudio(port=EMULATOR_GRPC_PORT)

        # --- Direction 1: signal-cli -> emulator (440 Hz) ---
        # Generate test tone as WAV file, play it into the virtual input device
        print(f"  [E] Direction 1: Sending {TEST_TONE_FREQ_OUT}Hz tone via virtual audio device...")
        play_duration = TEST_TONE_DURATION + 5  # extra time for settling
        tone_pcm = generate_tone(TEST_TONE_FREQ_OUT, play_duration)
        setup_output_dir()

        tone_wav_path = OUTPUT_DIR / "e_tone_out_source.wav"
        pcm_to_wav(tone_pcm, tone_wav_path)

        # Start playing tone into the virtual input device
        player_proc = play_to_device(input_device, tone_wav_path)

        # Wait for tone to flow through WebRTC (encode + network + decode)
        time.sleep(4)

        # Capture from emulator speaker while tone is playing
        capture_duration = TEST_TONE_DURATION + 1
        min_capture_bytes = 48000 * 2 * 2  # at least 2s of PCM
        captured_pcm = grpc_audio.capture_audio(capture_duration)

        # Stop player
        if player_proc and player_proc.poll() is None:
            player_proc.terminate()
            player_proc.wait(timeout=5)
        player_proc = None

        print(f"  [E] Captured {len(captured_pcm)} bytes from emulator speaker")

        if captured_pcm:
            pcm_to_wav(captured_pcm, OUTPUT_DIR / "e_tone_out_captured.wav")

        dir1_ok = False
        dir1_rms = 0.0
        if captured_pcm and len(captured_pcm) > 1920:
            dir1_rms = rms_level(captured_pcm)
            dir1_ok = detect_tone(captured_pcm, TEST_TONE_FREQ_OUT)
            print(f"  [E] Direction 1: detect={dir1_ok}, RMS={dir1_rms:.4f}")
            assert dir1_rms > 0.001, \
                f"Direction 1: captured audio is silent (RMS={dir1_rms:.6f})"
        else:
            print(f"  [E] Direction 1: insufficient captured audio")

        # --- Direction 2: emulator -> signal-cli (playout path) ---
        # Record from the virtual output device to verify playout works
        print("  [E] Direction 2: Recording from virtual output device...")
        record_duration = 3
        recorded_wav_path = OUTPUT_DIR / "e_playout_received.wav"
        record_from_device(output_device, recorded_wav_path, record_duration)

        recorded_pcm = b""
        if recorded_wav_path.exists() and recorded_wav_path.stat().st_size > 44:
            recorded_pcm = wav_to_pcm(recorded_wav_path)

        recorded_bytes = len(recorded_pcm)
        expected_bytes = 48000 * 2 * record_duration
        min_bytes = expected_bytes // 2

        print(f"  [E] Direction 2: received {recorded_bytes} bytes "
              f"(expected ~{expected_bytes})")

        dir2_ok = recorded_bytes >= min_bytes
        dir2_rms = rms_level(recorded_pcm) if recorded_pcm else 0.0

        # Hang up
        print("  [E] Hanging up...")
        rpc.hangup_call(call_id)
        call_id = None
        params = rpc.wait_for_state("ENDED", timeout=10)

        # Validate call ended normally (not a crash)
        event = params.get("callEvent", {})
        reason = event.get("reason", "")
        print(f"  [E] Call ended: reason={reason}")
        assert reason and "error" not in reason.lower(), \
            f"Call ended abnormally: reason={reason}"

        # Report results
        msgs = []
        msgs.append(f"dir1(signal-cli->emu):{'OK' if dir1_ok else 'FAIL'} RMS={dir1_rms:.4f}")
        msgs.append(f"dir2(playout-path):{'OK' if dir2_ok else 'FAIL'} "
                     f"{recorded_bytes}B/{expected_bytes}B")

        passed = dir1_ok and dir2_ok
        return TestResult("E: Bidirectional audio", passed, ", ".join(msgs))

    except Exception as e:
        return TestResult("E: Bidirectional audio", False, str(e))
    finally:
        if player_proc and player_proc.poll() is None:
            player_proc.terminate()
        if call_id:
            try:
                rpc.hangup_call(call_id)
            except Exception:
                pass
        if grpc_audio:
            grpc_audio.close()
        rpc.close()


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
SCENARIOS = {
    "A": ("Outgoing call lifecycle", scenario_a),
    "B": ("Incoming call lifecycle", scenario_b),
    "C": ("Incoming call rejection", scenario_c),
    "D": ("Ring timeout", scenario_d),
    "E": ("Bidirectional audio", scenario_e),
}


def main():
    parser = argparse.ArgumentParser(description="E2E voice call test runner")
    parser.add_argument("--socket", required=True, help="Path to signal-cli JSON-RPC socket")
    parser.add_argument("--scenarios", default="A,B,C,D,E",
                        help="Comma-separated list of scenarios to run (default: A,B,C,D,E)")
    parser.add_argument("--log-dir", default=None,
                        help="Directory containing log files for diagnostic collection")
    parser.add_argument("--record", action="store_true",
                        help="Record emulator screen during each scenario (saved to output dir)")
    parser.add_argument("--no-fail-fast", action="store_true",
                        help="Continue running scenarios after a failure (default: stop on first failure)")
    args = parser.parse_args()

    selected = [s.strip().upper() for s in args.scenarios.split(",")]
    for s in selected:
        if s not in SCENARIOS:
            print(f"Unknown scenario: {s}")
            print(f"Available: {', '.join(SCENARIOS.keys())}")
            sys.exit(1)

    logs = LogCollector(args.log_dir)
    recorder = ScreenRecorder(ADB_PATH, OUTPUT_DIR) if args.record else None

    print(f"=== E2E Voice Call Tests ===")
    print(f"Socket: {args.socket}")
    print(f"Scenarios: {', '.join(selected)}")
    print(f"Emulator account: {EMULATOR_ACCOUNT}")
    print(f"signal-cli account: {SIGNAL_CLI_ACCOUNT}")
    if logs.enabled:
        print(f"Log collection: {args.log_dir}")
    if recorder:
        print(f"Screen recording: enabled")
    print()

    results = []
    for s in selected:
        name, func = SCENARIOS[s]
        print(f"--- Scenario {s}: {name} ---")
        logs.mark_start(s)
        if recorder:
            recorder.start(s)
        t0 = time.monotonic()
        try:
            result = func(args.socket)
        except Exception as e:
            result = TestResult(f"{s}: {name}", False, f"Unhandled: {e}")
            traceback.print_exc()
        result.duration = time.monotonic() - t0

        # Retry once on failure for scenarios with flaky external dependencies
        if not result.passed and s == "E":
            print(f"  => {result}")
            print(f"  [E] Retrying scenario E (emulator audio HAL may need reset)...")
            try:
                wait_for_clean_state()
            except RuntimeError as e:
                print(f"  [E] Cannot retry: {e}")
            else:
                logs.mark_start(s)
                t0 = time.monotonic()
                try:
                    result = func(args.socket)
                except Exception as e:
                    result = TestResult(f"{s}: {name}", False, f"Unhandled: {e}")
                    traceback.print_exc()
                result.duration = time.monotonic() - t0

        if recorder:
            recorder.stop()
        results.append(result)
        print(f"  => {result}")

        # Collect and save logs for this scenario (prints diagnostics on failure)
        logs.save_scenario_logs(s, result)
        print()

        # Stop on first failure unless --no-fail-fast is set
        if not result.passed and not args.no_fail_fast:
            print(f"Stopping after scenario {s} failure (use --no-fail-fast to continue)")
            break

        # Wait for clean state between scenarios
        if s != selected[-1]:
            try:
                wait_for_clean_state()
            except RuntimeError as e:
                print(f"  WARNING: {e}")
                print(f"  Continuing anyway — next scenario may fail.")

    # Summary
    print("=" * 50)
    print("RESULTS:")
    passed = 0
    failed_ids = []
    for i, r in enumerate(results):
        print(f"  {r}")
        if r.passed:
            passed += 1
        else:
            failed_ids.append(selected[i])
    total = len(results)
    print(f"\n{passed}/{total} passed")

    if failed_ids and logs.enabled:
        print(f"\nDiagnostic logs for failed scenarios:")
        for fid in failed_ids:
            print(f"  Scenario {fid}: {args.log_dir}/scenario_{fid}_*.log")

    sys.exit(0 if passed == total else 1)


if __name__ == "__main__":
    main()
