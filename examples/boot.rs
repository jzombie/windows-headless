use windows_headless::timeouts;
use windows_headless::{HeadlessVmBuilder, PROVEN_SSH_PORT, WindowsVersion};

/// Supervised boot: the example owns QEMU — Ctrl-C / SIGTERM shuts the
/// guest down cleanly. There is no detach path.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut vm = HeadlessVmBuilder::new(WindowsVersion::Win11)
        .disk_size_gb(64)
        .memory_mb(4096)
        .smp_cores(4)
        .ssh_port(PROVEN_SSH_PORT)
        .build_and_start(
            "win11.qcow2",
            "iso/Win11_Enterprise_Eval.iso",
            "iso/virtio-win.iso",
        )
        .await?;
    println!("Waiting for Windows setup to complete over SSH...");
    vm.wait_for_ssh(timeouts::EXAMPLE_SSH_DEADLINE).await?;
    println!(
        "Windows VM is ready! SSH on 127.0.0.1:{} (Ctrl-C stops it)",
        PROVEN_SSH_PORT
    );
    tokio::signal::ctrl_c().await?;
    vm.shutdown_graceful(timeouts::EXAMPLE_SHUTDOWN_GRACE)
        .await?;
    println!("stopped");
    Ok(())
}
