#!/usr/bin/env bash
#
# build.sh — build signal-call-tunnel with the required ringrtc patches.
#
# The ringrtc submodule is kept pristine in git. Two patches under
# signal-call-tunnel/patches/ are applied to its working tree at build time:
#
#   1. ringrtc-disable-vpio.patch          — skips macOS VoiceProcessingIO so
#                                             virtual audio devices don't hang.
#   2. ringrtc-custom-audio-backend.patch  — adds the generic CustomAudioDevice
#                                             extension point the pipe backend
#                                             (src/pipe_audio.rs) plugs into.
#
# Order matters: the custom-audio-backend patch is generated against a tree that
# already has the VPIO patch applied.
#
# Why this script exists
# ----------------------
# signal-call-tunnel/build.rs can also apply these patches, but cargo compiles
# the ringrtc path dependency *before* running build.rs. Since the patches
# change ringrtc's public API, a direct `cargo build` on a pristine checkout
# needs to run twice (the first run applies the patches and stops). This script
# applies the patches up front so a single build succeeds, and is the canonical
# way to build the project.
#
# Usage:
#   ./build.sh                 # release build (default)
#   ./build.sh --debug         # debug build
#   ./build.sh test            # run `cargo test` instead of build
#   ./build.sh <cargo-args...> # any cargo args, e.g. `./build.sh build --locked`
#
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate_dir="$repo_root/signal-call-tunnel"
patch_dir="$crate_dir/patches"
ringrtc_dir="$repo_root/third-party/ringrtc"

# Each entry: "<patch-file>|<marker-file>|<marker-string>"
# The marker string is present in the marker file once the patch is applied,
# making application idempotent. Keep this list in sync with the PATCHES table
# in signal-call-tunnel/build.rs.
patches=(
  "ringrtc-disable-vpio.patch|src/rust/src/webrtc/audio_device_module.rs|RINGRTC_NO_VOICE_PROCESSING"
  "ringrtc-custom-audio-backend.patch|src/rust/src/webrtc/audio_device_module.rs|CustomAudioDevice"
)

apply_patches() {
  if [[ ! -d "$ringrtc_dir/.git" && ! -f "$ringrtc_dir/.git" ]]; then
    echo "error: ringrtc submodule not initialized at $ringrtc_dir" >&2
    echo "       run: git submodule update --init --recursive" >&2
    exit 1
  fi

  for entry in "${patches[@]}"; do
    IFS='|' read -r patch_file marker_file marker <<<"$entry"
    local patch_path="$patch_dir/$patch_file"
    local marker_path="$ringrtc_dir/$marker_file"

    if [[ ! -f "$patch_path" ]]; then
      echo "error: missing patch file: $patch_path" >&2
      exit 1
    fi

    if [[ -f "$marker_path" ]] && grep -q "$marker" "$marker_path"; then
      echo "ringrtc patch already applied: $patch_file"
      continue
    fi

    echo "applying ringrtc patch: $patch_file"
    if ! git -C "$ringrtc_dir" apply "$patch_path"; then
      echo "error: failed to apply $patch_file" >&2
      echo "       (is the ringrtc working tree clean and at the expected commit?)" >&2
      exit 1
    fi
  done
}

# Translate convenience flags, then default to a release build.
cargo_args=()
if [[ $# -eq 0 ]]; then
  cargo_args=(build --release)
else
  case "$1" in
    --debug)
      shift
      cargo_args=(build "$@")
      ;;
    build|test|check|clippy|run)
      cargo_args=("$@")
      # Default the build/test/check/run subcommands to release unless the
      # caller already chose a profile.
      if [[ ! " $* " == *" --release "* && ! " $* " == *" --debug "* ]]; then
        cargo_args+=(--release)
      fi
      ;;
    *)
      cargo_args=("$@")
      ;;
  esac
fi

apply_patches

echo "running: cargo ${cargo_args[*]}"
cd "$crate_dir"
exec cargo "${cargo_args[@]}"
