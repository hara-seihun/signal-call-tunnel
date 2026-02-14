#!/usr/bin/env bash
# Master orchestrator for E2E voice call tests.
#
# Starts signal-cli daemon, ensures emulator is ready, runs test scenarios,
# and cleans up afterwards.
set -euo pipefail

usage() {
    cat <<USAGE
Usage: $(basename "$0") [OPTIONS]

Run E2E voice call tests against signal-cli and an Android emulator.

Options:
  --signal-cli-account PHONE  Phone number for signal-cli account (e.g. +1234567890)
  --emulator-account PHONE    Phone number for emulator account (e.g. +1234567890)
  -s, --scenario ID           Run a single scenario (A, B, C, D, or E)
  --scenarios LIST             Comma-separated list of scenarios (default: A,B,C,D,E)
  --record                    Record emulator screen during each scenario
  --no-fail-fast              Continue running scenarios after a failure
  -h, --help                  Show this help message

Environment variables:
  SIGNAL_CLI_ACCOUNT    Alternative to --signal-cli-account
  EMULATOR_ACCOUNT      Alternative to --emulator-account

Scenarios:
  A   Outgoing call lifecycle (signal-cli calls, emulator answers)
  B   Incoming call lifecycle (emulator calls, signal-cli accepts)
  C   Incoming call rejection (emulator calls, signal-cli rejects)
  D   Ring timeout (outgoing call, no answer)
  E   Bidirectional audio (440Hz/1000Hz tone detection via gRPC)

Examples:
  $(basename "$0") --signal-cli-account +1234567890 --emulator-account +0987654321
  $(basename "$0") -s A             # Run only scenario A
  $(basename "$0") --scenarios A,B  # Run scenarios A and B

Log files are written to voice-test/output/logs/ and per-scenario excerpts
are saved on failure for diagnosis.
USAGE
}

# --- Parse arguments ---
SCENARIOS="A,B,C,D,E"
RECORD_FLAG=""
NO_FAIL_FAST_FLAG=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        -s|--scenario)
            SCENARIOS="${2:?ERROR: --scenario requires an argument}"
            shift 2
            ;;
        --scenarios)
            SCENARIOS="${2:?ERROR: --scenarios requires an argument}"
            shift 2
            ;;
        --signal-cli-account)
            export SIGNAL_CLI_ACCOUNT="${2:?ERROR: --signal-cli-account requires an argument}"
            shift 2
            ;;
        --emulator-account)
            export EMULATOR_ACCOUNT="${2:?ERROR: --emulator-account requires an argument}"
            shift 2
            ;;
        --record)
            RECORD_FLAG="--record"
            shift
            ;;
        --no-fail-fast)
            NO_FAIL_FAST_FLAG="--no-fail-fast"
            shift
            ;;
        *)
            echo "Unknown option: $1"
            echo "Run '$(basename "$0") --help' for usage."
            exit 1
            ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

source "$SCRIPT_DIR/config.sh"

# Export variables so e2e_test.py can read them from env
export SIGNAL_CLI_ACCOUNT
export EMULATOR_ACCOUNT
export ADB

# --- State ---
DAEMON_PID=""
LOGCAT_PID=""
TEST_PID=""
CLEANUP_DONE=false

cleanup() {
    if $CLEANUP_DONE; then return; fi
    CLEANUP_DONE=true
    echo ""
    echo "=== Cleanup ==="

    # Kill the Python test runner if still going (e.g. on Ctrl+C)
    # It's the foreground process so usually gets SIGINT directly, but be safe.
    if [ -n "$TEST_PID" ] && kill -0 "$TEST_PID" 2>/dev/null; then
        echo "Stopping test runner (PID $TEST_PID)..."
        kill "$TEST_PID" 2>/dev/null || true
        wait "$TEST_PID" 2>/dev/null || true
    fi

    if [ -n "$LOGCAT_PID" ] && kill -0 "$LOGCAT_PID" 2>/dev/null; then
        echo "Stopping logcat collector (PID $LOGCAT_PID)..."
        kill "$LOGCAT_PID" 2>/dev/null || true
        wait "$LOGCAT_PID" 2>/dev/null || true
    fi

    if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "Stopping signal-cli daemon (PID $DAEMON_PID)..."
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi

    rm -f "$SIGNAL_CLI_SOCKET"

    # Kill any orphaned signal-call-tunnel processes
    pkill -f "signal-call-tunnel" 2>/dev/null || true

    echo "Cleanup done."
    echo ""
    echo "=== Log files ==="
    echo "  signal-cli + tunnel : $SIGNAL_CLI_LOG"
    echo "  daemon console      : $DAEMON_CONSOLE_LOG"
    echo "  Android logcat      : $LOGCAT_LOG"
}

# Trap EXIT (normal exit, set -e failures) plus INT/TERM (Ctrl+C, kill)
trap cleanup EXIT INT TERM

# --- Kill stale processes from previous interrupted runs ---
echo "=== Pre-run Cleanup ==="
STALE=false

# Kill leftover signal-cli daemon using our test socket
if [ -S "$SIGNAL_CLI_SOCKET" ]; then
    echo "  Removing stale socket: $SIGNAL_CLI_SOCKET"
    # Find the daemon that owns it (if still alive)
    STALE_PID=$(lsof -t "$SIGNAL_CLI_SOCKET" 2>/dev/null | head -1 || true)
    if [ -n "$STALE_PID" ]; then
        echo "  Killing stale daemon (PID $STALE_PID)..."
        kill "$STALE_PID" 2>/dev/null || true
        sleep 1
    fi
    rm -f "$SIGNAL_CLI_SOCKET"
    STALE=true
fi

# Kill leftover signal-call-tunnel processes
if pgrep -f "signal-call-tunnel" >/dev/null 2>&1; then
    echo "  Killing orphaned signal-call-tunnel processes..."
    pkill -f "signal-call-tunnel" 2>/dev/null || true
    STALE=true
fi

# Kill leftover logcat collectors from our log file
if pgrep -f "logcat.*threadtime" >/dev/null 2>&1; then
    echo "  Killing stale logcat collectors..."
    pkill -f "logcat.*threadtime" 2>/dev/null || true
    STALE=true
fi

if $STALE; then
    echo "  Stale processes cleaned up. Waiting 2s..."
    sleep 2
else
    echo "  No stale processes found."
fi

# --- Build binaries ---
echo "=== Building Binaries ==="

echo "  Building signal-cli (gradlew installDist)..."
if ! (cd third-party/signal-cli && ./gradlew -q installDist); then
    echo "ERROR: signal-cli build failed"
    exit 1
fi
echo "  signal-cli: OK"

echo "  Building signal-call-tunnel (cargo build)..."
if ! (cd signal-call-tunnel && cargo build --quiet); then
    echo "ERROR: signal-call-tunnel build failed"
    exit 1
fi
echo "  signal-call-tunnel: OK"

# Check for BlackHole virtual audio drivers (macOS only)
if [ "$(uname)" = "Darwin" ]; then
    AUDIO_HAL="/Library/Audio/Plug-Ins/HAL"
    MISSING_DRIVERS=false
    if [ ! -d "$AUDIO_HAL/signal_input.driver" ]; then
        MISSING_DRIVERS=true
    fi
    if [ ! -d "$AUDIO_HAL/signal_output.driver" ]; then
        MISSING_DRIVERS=true
    fi
    if $MISSING_DRIVERS; then
        RINGRTC_DIR="$PROJECT_DIR/third-party/ringrtc"
        echo ""
        echo "ERROR: BlackHole virtual audio drivers are not installed."
        echo "       signal-call-tunnel requires pre-installed audio drivers on macOS."
        echo ""
        echo "  Run the following command once (requires root):"
        echo ""
        echo "    sudo bash $RINGRTC_DIR/bin/virtual_audio.sh \\"
        echo "      --setup --input-source signal_input --output-sink signal_output"
        echo ""
        echo "  To remove them later:"
        echo ""
        echo "    sudo bash $RINGRTC_DIR/bin/virtual_audio.sh \\"
        echo "      --teardown --input-source signal_input --output-sink signal_output"
        echo ""
        exit 1
    fi
    echo "  BlackHole audio drivers: OK"
fi

if ! command -v python3 &>/dev/null; then
    echo "ERROR: python3 not found"
    exit 1
fi
echo "  python3: $(python3 --version)"

if ! "$ADB" devices 2>/dev/null | grep -q "emulator"; then
    echo "WARNING: No emulator detected via adb. Attempting to start..."
    # The headed (non-headless) emulator binary is required for gRPC audio
    # streaming (scenario E).  The headless binary strips audio output support,
    # causing streamAudio to block forever.
    "$EMULATOR_BIN" -avd "$EMULATOR_AVD" -no-snapshot-load \
        -grpc "$EMULATOR_GRPC_PORT" &
    EMU_PID=$!
    echo "  Waiting for emulator boot (PID $EMU_PID)..."
    "$ADB" wait-for-device
    # Wait for boot to complete
    for i in $(seq 1 60); do
        BOOT=$("$ADB" shell getprop sys.boot_completed 2>/dev/null || echo "")
        if [ "$BOOT" = "1" ]; then
            break
        fi
        sleep 2
    done
    echo "  Emulator booted."
else
    echo "  Emulator: already running"
fi

# Ensure adbd runs as root (required for uiautomator dump on API 34+)
"$ADB" root 2>/dev/null || true
"$ADB" wait-for-device 2>/dev/null
echo "  adb root: $(${ADB} shell id -u 2>/dev/null || echo 'unknown')"

# Check gRPC connectivity (scenario E needs unauthenticated gRPC)
if echo "$SCENARIOS" | grep -q "E"; then
    echo "  Checking emulator gRPC on port $EMULATOR_GRPC_PORT..."
    if python3 -c "
import grpc, sys
ch = grpc.insecure_channel('localhost:$EMULATOR_GRPC_PORT')
try:
    grpc.channel_ready_future(ch).result(timeout=3)
except Exception:
    sys.exit(1)
finally:
    ch.close()
" 2>/dev/null; then
        echo "  gRPC: reachable"
    else
        echo "  WARNING: Emulator gRPC on port $EMULATOR_GRPC_PORT is not reachable."
        echo "           If scenario E fails with UNAUTHENTICATED, restart the emulator with:"
        echo "             $EMULATOR_BIN -avd $EMULATOR_AVD -no-snapshot-load -no-window -grpc $EMULATOR_GRPC_PORT"
    fi
fi

# Verify Signal is installed
if ! "$ADB" shell pm list packages 2>/dev/null | grep -q "org.thoughtcrime.securesms"; then
    echo "ERROR: Signal is not installed on the emulator"
    exit 1
fi
echo "  Signal app: installed"

# --- Install Python deps ---
echo ""
echo "=== Python Dependencies ==="
pip install -q -r "$SCRIPT_DIR/requirements.txt"
echo "  grpcio: OK"

# --- Generate proto stubs ---
echo ""
echo "=== Proto Stubs ==="
if [ ! -f "$SCRIPT_DIR/lib/proto/emulator_controller_pb2.py" ]; then
    bash "$SCRIPT_DIR/generate_proto.sh"
else
    echo "  Proto stubs already generated (use 'bash voice-test/generate_proto.sh' to regenerate)"
fi

# --- Ensure Signal is on main screen ---
echo ""
echo "=== Preparing Signal App ==="
"$ADB" shell monkey -p org.thoughtcrime.securesms -c android.intent.category.LAUNCHER 1
sleep 2
echo "  Signal launched"

# Set media and ring volumes to max (voice call volume is set during the
# active call in scenario E via KEYCODE_VOLUME_UP key events).
"$ADB" shell cmd media_session volume --stream 2 --set 15 >/dev/null 2>&1  # ring
"$ADB" shell cmd media_session volume --stream 3 --set 15 >/dev/null 2>&1  # music
echo "  Audio volumes: ring/media set to max"

# --- Set up log collection ---
echo ""
echo "=== Log Collection ==="
mkdir -p "$LOG_DIR"
# Truncate logs from previous runs
: > "$SIGNAL_CLI_LOG"
: > "$DAEMON_CONSOLE_LOG"
: > "$LOGCAT_LOG"

# Start logcat collector (Signal app + WebRTC/RingRTC tags)
"$ADB" logcat -c 2>/dev/null || true  # Clear old logcat buffer
"$ADB" logcat -v threadtime > "$LOGCAT_LOG" 2>&1 &
LOGCAT_PID=$!
echo "  logcat collector PID: $LOGCAT_PID -> $LOGCAT_LOG"

# --- Start signal-cli daemon ---
echo ""
echo "=== Starting signal-cli Daemon ==="

# Export tunnel binary path so the daemon subprocess can find it
export SIGNAL_CALL_TUNNEL_BIN

# -vv for DEBUG+TRACE on org.asamk (includes [tunnel-{callId}] lines)
# --log-file captures detailed logs; stdout/stderr go to console log
$SIGNAL_CLI_BIN -vv -a "$SIGNAL_CLI_ACCOUNT" \
    --log-file "$SIGNAL_CLI_LOG" \
    daemon --socket "$SIGNAL_CLI_SOCKET" \
    > "$DAEMON_CONSOLE_LOG" 2>&1 &
DAEMON_PID=$!
echo "  Daemon PID: $DAEMON_PID"
echo "  Log file: $SIGNAL_CLI_LOG"

# Wait for socket to appear
echo "  Waiting for daemon socket..."
for i in $(seq 1 30); do
    if [ -S "$SIGNAL_CLI_SOCKET" ]; then
        break
    fi
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "ERROR: Daemon exited prematurely"
        exit 1
    fi
    sleep 1
done

if [ ! -S "$SIGNAL_CLI_SOCKET" ]; then
    echo "ERROR: Daemon socket did not appear within 30s"
    exit 1
fi
echo "  Daemon ready."

# --- Run tests ---
echo ""
echo "=== Running Tests ==="
SCENARIOS="${SCENARIOS:-A,B,C,D,E}"

TEST_PID=""
set +e
python3 -u "$SCRIPT_DIR/e2e_test.py" \
    --socket "$SIGNAL_CLI_SOCKET" \
    --log-dir "$LOG_DIR" \
    --scenarios "$SCENARIOS" $RECORD_FLAG $NO_FAIL_FAST_FLAG &
TEST_PID=$!
wait "$TEST_PID"
TEST_EXIT=$?
TEST_PID=""
set -e

echo ""
if [ $TEST_EXIT -eq 0 ]; then
    echo "=== ALL TESTS PASSED ==="
else
    echo "=== SOME TESTS FAILED ==="
    echo ""
    echo "Log files for diagnosis:"
    echo "  signal-cli + tunnel : $SIGNAL_CLI_LOG"
    echo "  daemon console      : $DAEMON_CONSOLE_LOG"
    echo "  Android logcat      : $LOGCAT_LOG"
    echo ""
    echo "Per-scenario log excerpts are in: $LOG_DIR/scenario_*.log"
fi

exit $TEST_EXIT
