//! Bake the git commit hash into the binary for provenance sidecars.
//! Graceful when `.git` is absent (crates.io tarball builds): emits
//! `"unknown"` instead of failing the build.
fn main() {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_GIT_SHA={sha}");
    // Dirty flag: any staged or unstaged change (untracked files excluded —
    // build outputs like target/ would otherwise always read dirty).
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.iter().all(|b| b.is_ascii_whitespace()))
        .unwrap_or(false);
    println!("cargo:rustc-env=BUILD_GIT_DIRTY={dirty}");
    // Re-run only when the commit changes, not on every build.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
}
