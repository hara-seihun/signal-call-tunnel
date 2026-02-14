use std::path::Path;
use std::process::Command;

fn main() {
    // Apply the VPIO-disable patch to ringrtc if it hasn't been applied yet.
    // This is a build-time patch: cargo re-runs build.rs when the patch file
    // or the target source file changes.
    let ringrtc_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../third-party/ringrtc");
    let patch_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("patches/ringrtc-disable-vpio.patch");

    let adm_file = ringrtc_dir.join("src/rust/src/webrtc/audio_device_module.rs");

    println!("cargo::rerun-if-changed={}", patch_file.display());
    println!("cargo::rerun-if-changed={}", adm_file.display());

    if !patch_file.exists() {
        return;
    }

    // Check if the patch is already applied by looking for the marker function.
    if let Ok(content) = std::fs::read_to_string(&adm_file) {
        if content.contains("RINGRTC_NO_VOICE_PROCESSING") {
            // Already applied
            return;
        }
    }

    // Canonicalize paths so git apply works regardless of how cargo sets cwd.
    // Run from within the ringrtc directory to avoid parent-repo submodule issues.
    let ringrtc_canonical = match ringrtc_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cargo:warning=Cannot resolve ringrtc path: {e}");
            return;
        }
    };
    let patch_canonical = match patch_file.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cargo:warning=Cannot resolve patch path: {e}");
            return;
        }
    };

    let status = Command::new("git")
        .arg("apply")
        .arg(&patch_canonical)
        .current_dir(&ringrtc_canonical)
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!("cargo:warning=Applied ringrtc VPIO-disable patch for virtual audio support");
        }
        Ok(s) => {
            eprintln!(
                "cargo:warning=Failed to apply ringrtc patch (exit {}); \
                 VPIO may hang with virtual audio devices",
                s
            );
        }
        Err(e) => {
            eprintln!("cargo:warning=Could not run git apply: {e}");
        }
    }
}
