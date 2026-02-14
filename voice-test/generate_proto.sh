#!/usr/bin/env bash
# Compile emulator_controller.proto into Python stubs.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ANDROID_SDK="${ANDROID_SDK:-/opt/homebrew/share/android-commandlinetools}"
PROTO_DIR="$ANDROID_SDK/emulator/lib"
PROTO_FILE="$PROTO_DIR/emulator_controller.proto"
OUT_DIR="$SCRIPT_DIR/lib/proto"

if [ ! -f "$PROTO_FILE" ]; then
    echo "ERROR: Proto file not found: $PROTO_FILE"
    echo "Make sure Android emulator is installed via commandlinetools."
    exit 1
fi

mkdir -p "$OUT_DIR"

echo "Compiling $PROTO_FILE -> $OUT_DIR ..."
python3 -m grpc_tools.protoc \
    "-I$PROTO_DIR" \
    "--python_out=$OUT_DIR" \
    "--grpc_python_out=$OUT_DIR" \
    "$PROTO_FILE"

# Fix the generated import path (grpc_tools generates absolute imports)
# The grpc stub file imports the pb2 module; ensure it works as a local import.
if [ -f "$OUT_DIR/emulator_controller_pb2_grpc.py" ]; then
    # On macOS, sed -i requires '' argument
    sed -i '' 's/^import emulator_controller_pb2/from . import emulator_controller_pb2/' \
        "$OUT_DIR/emulator_controller_pb2_grpc.py" 2>/dev/null || \
    sed -i 's/^import emulator_controller_pb2/from . import emulator_controller_pb2/' \
        "$OUT_DIR/emulator_controller_pb2_grpc.py"
fi

echo "Proto stubs generated in $OUT_DIR:"
ls -la "$OUT_DIR"/emulator_controller_pb2*.py
