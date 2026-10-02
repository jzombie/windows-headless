//! Weekly canary: latest Windows ISO through the real user pipeline
//! (`setup` → `build-media` → `install`, pinned portable QEMU).
//! Ignored unless `LATEST_WIN11_ISO` is set (i.e. only runs in canary CI).
#![cfg(feature = "downloader")]
use windows_headless::{
    commands::{BuildArgs, cmd_build, cmd_install},
    timeouts,
    unattend::{DEFAULT_PASSWORD, DEFAULT_USERNAME},
};

#[tokio::test]
#[ignore]
async fn test_latest_windows_headless_pipeline() {
    let iso = std::path::PathBuf::from(
        std::env::var("LATEST_WIN11_ISO").expect("LATEST_WIN11_ISO not set"),
    );
    // Media is force-rebuilt: the cache may hold last week's image from a
    // different ISO, and build-media skips on output+sidecar presence.
    cmd_build(BuildArgs {
        win_iso: iso.clone(),
        arch: None,
        username: DEFAULT_USERNAME.to_string(),
        password: DEFAULT_PASSWORD.to_string(),
        locale: "en-US".to_string(),
        out: None,
        force: true,
    })
    .await
    .unwrap();
    // Proven sizing defaults; failure screenshots land in the VM dir and
    // are uploaded by CI. unwrap = red canary with logs attached.
    cmd_install(
        None,
        Some(iso),
        timeouts::SCREENSHOTS_EVERY_SECS,
        DEFAULT_USERNAME,
        DEFAULT_PASSWORD,
        false,
        false,
        false,
        timeouts::INSTALL_SSH_DEADLINE_SECS,
        4096,
        4,
        64,
    )
    .await
    .unwrap();
}
