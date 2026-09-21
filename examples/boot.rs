use std::time::Duration;
use windows_headless::{HeadlessVmBuilder, WindowsVersion};

/// Supervised boot: the example owns QEMU — Ctrl-C / SIGTERM shuts the
/// guest down cleanly. There is no detach path.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut vm = HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval)
        .disk_size_gb(64)
        .memory_mb(4096)
        .smp_cores(4)
        .ssh_port(2222)
        .build_and_start(
            "win11.qcow2",
            "iso/Win11_Enterprise_Eval.iso",
            "iso/virtio-win.iso",
        )
        .await?;
    println!("Waiting for Windows setup to complete over SSH...");
    vm.wait_for_ssh(Duration::from_secs(900)).await?;
    println!("Windows VM is ready! SSH on 127.0.0.1:2222 (Ctrl-C stops it)");
    tokio::signal::ctrl_c().await?;
    vm.shutdown_graceful(Duration::from_secs(90)).await?;
    println!("stopped");
    Ok(())
}
