#!/usr/bin/env bash
# Shared environment configuration for E2E voice call tests.

SIGNAL_CLI_ACCOUNT="${SIGNAL_CLI_ACCOUNT:?ERROR: SIGNAL_CLI_ACCOUNT not set (use --signal-cli-account or export SIGNAL_CLI_ACCOUNT)}"
EMULATOR_ACCOUNT="${EMULATOR_ACCOUNT:?ERROR: EMULATOR_ACCOUNT not set (use --emulator-account or export EMULATOR_ACCOUNT)}"
ANDROID_SDK="${ANDROID_SDK:-/opt/homebrew/share/android-commandlinetools}"
ADB="${ADB:-$(command -v adb || echo "$ANDROID_SDK/platform-tools/adb")}"
EMULATOR_BIN="${EMULATOR_BIN:-$(command -v emulator || echo "$ANDROID_SDK/emulator/emulator")}"
EMULATOR_AVD="${EMULATOR_AVD:-signal-test}"
EMULATOR_GRPC_PORT=8554
SIGNAL_CLI_SOCKET="/tmp/signal-cli-test.sock"
SIGNAL_CLI_BIN="./third-party/signal-cli/build/install/signal-cli/bin/signal-cli"
SIGNAL_CALL_TUNNEL_BIN="./signal-call-tunnel/target/debug/signal-call-tunnel"
TEST_TONE_FREQ_OUT=440    # signal-cli -> emulator (Hz)
TEST_TONE_FREQ_IN=1000    # emulator -> signal-cli (Hz)
TEST_TONE_DURATION=3      # seconds

# Log collection
LOG_DIR="voice-test/output/logs"
SIGNAL_CLI_LOG="$LOG_DIR/signal-cli.log"
DAEMON_CONSOLE_LOG="$LOG_DIR/daemon-console.log"
LOGCAT_LOG="$LOG_DIR/logcat.log"
