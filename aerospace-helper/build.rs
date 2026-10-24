use std::process::Command;

fn main() {
    // Stamp the build with the short git hash (parity with the AeroSpace build's
    // 0.0.0-SNAPSHOT <hash>). Falls back to "unknown" outside a git checkout
    // (e.g. local builds from the chezmoi target tree).
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=HELPER_GIT_HASH={}", hash);
    println!("cargo:rerun-if-changed=.git/HEAD");
}
