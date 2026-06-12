use std::path::{Path, PathBuf};
use std::process::Command;

/// A build-time patch applied to the ringrtc submodule.
struct RingrtcPatch {
    /// Patch file under `patches/`.
    patch_file: &'static str,
    /// File whose contents are checked to detect an already-applied patch.
    marker_file: &'static str,
    /// String present in `marker_file` once the patch is applied.
    marker: &'static str,
    /// Human-readable note printed when the patch is applied or fails.
    description: &'static str,
}

/// Patches applied to ringrtc, in order. The pipe-backend patch is generated
/// against a tree that already has the VPIO patch applied, so VPIO must come
/// first.
const PATCHES: &[RingrtcPatch] = &[
    RingrtcPatch {
        patch_file: "patches/ringrtc-disable-vpio.patch",
        marker_file: "src/rust/src/webrtc/audio_device_module.rs",
        marker: "RINGRTC_NO_VOICE_PROCESSING",
        description: "VPIO-disable patch for virtual audio support",
    },
    RingrtcPatch {
        patch_file: "patches/ringrtc-custom-audio-backend.patch",
        marker_file: "src/rust/src/webrtc/audio_device_module.rs",
        marker: "CustomAudioDevice",
        description: "custom audio backend extension point",
    },
];

fn main() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ringrtc_dir = manifest_dir.join("../third-party/ringrtc");

    // Canonicalize so git apply works regardless of how cargo sets cwd. We run
    // from within the ringrtc directory to avoid parent-repo submodule issues.
    let ringrtc_canonical = match ringrtc_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cargo:warning=Cannot resolve ringrtc path: {e}");
            return;
        }
    };

    let mut newly_applied = Vec::new();
    for patch in PATCHES {
        if apply_patch(manifest_dir, &ringrtc_canonical, patch) {
            newly_applied.push(patch.description);
        }
    }

    // Patches are applied from this crate's build script, which cargo runs only
    // *after* it has already compiled the pristine ringrtc dependency. Because
    // the pipe-backend patch changes ringrtc's public API, the current build is
    // still linking against the unpatched rlib. Fail now with a clear message so
    // the next `cargo build` recompiles ringrtc with the patched sources, rather
    // than surfacing a confusing "unresolved import" error.
    if !newly_applied.is_empty() {
        panic!(
            "Applied ringrtc patch(es): {}. These modify ringrtc, which cargo \
             already compiled this run. Re-run the build to pick up the changes.",
            newly_applied.join(", ")
        );
    }
}

/// Returns `true` if the patch was newly applied this run, `false` if it was
/// already present or could not be applied.
fn apply_patch(manifest_dir: &Path, ringrtc_canonical: &Path, patch: &RingrtcPatch) -> bool {
    let patch_file = manifest_dir.join(patch.patch_file);
    let marker_file = ringrtc_canonical.join(patch.marker_file);

    // Cargo re-runs build.rs when the patch file or its marker source changes.
    println!("cargo::rerun-if-changed={}", patch_file.display());
    println!("cargo::rerun-if-changed={}", marker_file.display());

    if !patch_file.exists() {
        eprintln!(
            "cargo:warning=Patch file missing, skipping {}: {}",
            patch.description,
            patch_file.display()
        );
        return false;
    }

    // Check if the patch is already applied by looking for the marker (idempotent).
    if let Ok(content) = std::fs::read_to_string(&marker_file)
        && content.contains(patch.marker)
    {
        return false;
    }

    let patch_canonical: PathBuf = match patch_file.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cargo:warning=Cannot resolve patch path: {e}");
            return false;
        }
    };

    let status = Command::new("git")
        .arg("apply")
        .arg(&patch_canonical)
        .current_dir(ringrtc_canonical)
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!("cargo:warning=Applied ringrtc {}", patch.description);
            true
        }
        Ok(s) => {
            eprintln!(
                "cargo:warning=Failed to apply ringrtc {} (exit {})",
                patch.description, s
            );
            false
        }
        Err(e) => {
            eprintln!("cargo:warning=Could not run git apply: {e}");
            false
        }
    }
}
