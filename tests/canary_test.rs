//! Weekly canary: latest Eval ISO, fail fast with a framebuffer screenshot.
//! Ignored unless `LATEST_WIN11_ISO` is set (i.e. only runs in canary CI).
use windows_headless::{HeadlessVmBuilder, WindowsVersion, timeouts};

#[tokio::test]
#[ignore]
async fn test_latest_windows_headless_pipeline() {
    let iso_path = std::env::var("LATEST_WIN11_ISO").expect("LATEST_WIN11_ISO not set");
    let virtio: String =
        std::env::var("VIRTIO_ISO").unwrap_or_else(|_| "/tmp/virtio-win.iso".into());

    let vm = HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval)
        .build_and_start("/tmp/canary.qcow2", &iso_path, &virtio)
        .await
        .unwrap();

    let ssh = tokio::time::timeout(
        timeouts::CANARY_SSH_DEADLINE,
        vm.wait_for_ssh(timeouts::CANARY_SSH_DEADLINE),
    )
    .await;
    if ssh.is_err() || ssh.unwrap().is_err() {
        if let Ok(mut q) = vm.qmp().await {
            let _ = q.screendump("/tmp/ci_failure.ppm").await;
        }
        panic!("Canary FAILED: VM stuck during setup. See /tmp/ci_failure.ppm artifact.");
    }

    vm.shutdown_cleanly().await.unwrap();
}
