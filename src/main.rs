#[cfg(feature = "downloader")]
use anyhow::Context;
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use windows_headless::{DiskManager, HeadlessVmBuilder, VmConfig, WindowsVersion};

#[derive(Parser)]
#[command(name = "windows-headless", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render autounattend.xml to stdout or a vvfat dir.
    Gen {
        #[arg(long, default_value = "AdminUser")]
        username: String,
        #[arg(long, default_value = "P@ssw0rd123!")]
        password: String,
        #[arg(long, default_value = "en-US")]
        locale: String,
        #[arg(long)]
        autologon: bool,
        #[arg(long)]
        unattend_dir: Option<PathBuf>,
    },
    /// Full install: stage vvfat, create base, launch QEMU, wait for SSH.
    Run {
        /// Target disk image (default: `<vm-dir>/disk.qcow2`).
        #[arg(long)]
        disk: Option<String>,
        #[arg(long)]
        win_iso: String,
        #[arg(long)]
        virtio_iso: String,
        #[arg(long, default_value_t = 2222)]
        ssh_port: u16,
        /// `-display` spec: `none`, `cocoa`, `gtk`, `vnc=127.0.0.1:0`.
        #[arg(long, default_value = "none")]
        display: String,
        /// Guest RAM in MB (Windows 11 setup needs >= 4096).
        #[arg(long, default_value_t = 4096)]
        memory: u32,
        /// Guest SMP cores.
        #[arg(long, default_value_t = 4)]
        smp: u32,
        /// VM dir anchoring disk + unattend staging (default: project cache
        /// `vms/<auto>`; everything stays out of the repo checkout).
        #[arg(long)]
        vm_dir: Option<PathBuf>,
        /// Answer-file staging dir (default: `<vm-dir>/unattend`).
        #[arg(long)]
        unattend_dir: Option<PathBuf>,
        /// Delete a pre-existing disk image (refused while a VM is alive).
        #[arg(long)]
        force: bool,
        /// Portable tree root (extracted `qemu-portable` dir); else PATH.
        #[arg(long)]
        qemu_dir: Option<PathBuf>,
    },
    /// Offline overlay: qemu-img create -b base overlay.
    Overlay {
        base: PathBuf,
        overlay: PathBuf,
        /// Portable tree root (extracted `qemu-portable` dir); else PATH.
        #[arg(long)]
        qemu_dir: Option<PathBuf>,
    },
    /// Boot an existing disk (no install media).
    Boot {
        disk: String,
        #[arg(long, default_value_t = 2222)]
        ssh_port: u16,
        /// `-display` spec: `none`, `cocoa`, `gtk`, `vnc=127.0.0.1:0`.
        #[arg(long, default_value = "none")]
        display: String,
        /// Guest RAM in MB.
        #[arg(long, default_value_t = 4096)]
        memory: u32,
        /// Guest SMP cores.
        #[arg(long, default_value_t = 4)]
        smp: u32,
        /// VM dir for the pidfile (default: project cache `vms/win11`).
        #[arg(long)]
        vm_dir: Option<PathBuf>,
        /// Portable tree root (extracted `qemu-portable` dir); else PATH.
        #[arg(long)]
        qemu_dir: Option<PathBuf>,
    },
    /// Graceful shutdown via QMP + pidfile cleanup (no orphans).
    Stop {
        /// VM dir holding `qemu.pid` (default: project cache `vms/win11`).
        #[arg(long)]
        vm_dir: Option<PathBuf>,
        /// QMP socket of the running VM.
        #[arg(long, default_value = "/tmp/qemu-qmp.sock")]
        qmp_sock: String,
    },
    /// Download a portable QEMU release into the project cache.
    #[cfg(feature = "downloader")]
    FetchQemu {
        /// Release version (`11.1.1`); default: latest GitHub release.
        #[arg(long)]
        qemu_version: Option<String>,
        /// Cache base dir (default: `<project>/.cache/qemu-portable`).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Download ISOs into the cache dir, skipping what is already present.
    /// If Fedora's bot-wall blocks scripted downloads, fetch the file in a
    /// real browser and save it as `<dir>/virtio-win.iso` (or pass any
    /// mirror via --virtio-iso-url) — `fetch` validates and keeps it.
    #[cfg(feature = "downloader")]
    Fetch {
        /// Eval ISO URL (fresh, time-limited link from a browser).
        #[arg(long)]
        win_iso_url: Option<String>,
        /// VirtIO ISO URL override (default: Fedora stable-virtio).
        #[arg(long)]
        virtio_iso_url: Option<String>,
        /// Cache dir (default: <project>/.cache/iso or $WINDOWS_HEADLESS_ISO_DIR).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Expected SHA-256 for virtio-win.iso; re-downloads on mismatch.
        #[arg(long)]
        virtio_sha256: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Gen {
            username,
            password,
            locale,
            autologon,
            unattend_dir,
        } => {
            let cfg = VmConfig {
                username,
                password,
                locale,
                auto_logon: autologon,
                ..VmConfig::default()
            };
            let xml = cfg.generate_unattend_xml();
            if let Some(dir) = unattend_dir {
                let f = DiskManager::ensure_unattend_dir(&dir, &xml)?;
                eprintln!("wrote {}", f.display());
            } else {
                println!("{xml}");
            }
        }
        Cmd::Run {
            disk,
            win_iso,
            virtio_iso,
            ssh_port,
            display,
            memory,
            smp,
            vm_dir,
            unattend_dir,
            force,
            qemu_dir,
        } => {
            // Everything VM-related lives under vm_dir (default: project
            // cache `vms/win11`, no eviction) unless explicitly overridden.
            let vm_dir = match vm_dir {
                Some(d) => d,
                None => default_vm_dir()?,
            };
            let disk =
                disk.unwrap_or_else(|| vm_dir.join("disk.qcow2").to_string_lossy().into_owned());
            let unattend_dir = unattend_dir.unwrap_or_else(|| vm_dir.join("unattend"));
            let mut b = HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval)
                .ssh_port(ssh_port)
                .display(display)
                .memory_mb(memory)
                .smp_cores(smp)
                .workdir(vm_dir)
                .unattend_dir(unattend_dir);
            if let Some(d) = qemu_dir {
                b = b.qemu_dir(d);
            }
            // Preflight: both ISOs must be real images. A bot-check HTML
            // page saved as .iso hangs EDK2 at boot with zero block I/O —
            // fail here with the actual cause instead of a black screen.
            #[cfg(feature = "downloader")]
            for iso in [&win_iso, &virtio_iso] {
                windows_headless::downloader::validate_iso_image(std::path::Path::new(iso))
                    .with_context(|| format!("invalid ISO: {iso}"))?;
            }
            // Preflight: refuse to start over a live VM or a stale disk
            // instead of failing deep inside qemu-img with a lock error.
            // NOTE: single-VM assumption — the QMP socket path is fixed.
            if std::path::Path::new(&disk).exists() {
                if windows_headless::qmp_alive(&b.qmp_socket) {
                    anyhow::bail!(
                        "a VM is already running (disk locked, SSH forwarded); `stop` it first"
                    );
                }
                if force {
                    std::fs::remove_file(&disk)?;
                } else {
                    anyhow::bail!("disk {disk} exists from a previous run; rm it or pass --force");
                }
            }
            let mut vm = b.build_and_start(&disk, &win_iso, &virtio_iso).await?;
            let pidfile = vm.builder.pidfile();
            // Answer the ISO's "press any key to boot from CD" prompt.
            // The boot image takes a while to load (several MB off the
            // emulated CD), so the prompt can appear a minute+ after
            // launch — well past a short tap window. Tap Enter immediately,
            // then every 3s for ~150s (QMP send-key needs the usb-kbd
            // device the builder adds on ARM). Once the kernel loads,
            // stray Enters only ever accept defaults/advance, and Setup is
            // fully answered anyway. Failures are warned, not silent — a
            // dead QMP here means SSH polling will time out with a
            // screendump.
            println!("waiting for installer boot prompt (auto-pressing Enter)...");
            for i in 1..=50 {
                match vm.send_keys(&["ret"]).await {
                    Ok(()) => println!("boot-key tap {i}/50 sent"),
                    Err(e) => eprintln!("boot-key tap {i}/50 failed: {e:#}"),
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            println!("launched; waiting for SSH on 127.0.0.1:{ssh_port} ...");
            // Supervisor: the CLI owns QEMU. Ctrl-C / SIGTERM at any point
            // stops the guest — there is no detach path, ever.
            tokio::select! {
                r = vm.wait_for_ssh(Duration::from_secs(15 * 60)) => r?,
                _ = shutdown_signal() => {
                    println!("interrupted during install; stopping guest...");
                    vm.kill().await?;
                    let _ = std::fs::remove_file(pidfile);
                    return Ok(());
                }
            }
            println!("ready — ssh -p {ssh_port} AdminUser@127.0.0.1");
            println!("press Ctrl-C to shut the VM down cleanly");
            shutdown_signal().await;
            println!("shutting down guest (ACPI)...");
            vm.shutdown_graceful(Duration::from_secs(90)).await?;
            let _ = std::fs::remove_file(pidfile);
            println!("stopped");
        }
        Cmd::Overlay {
            base,
            overlay,
            qemu_dir,
        } => {
            let img = qemu_dir
                .map(|d| d.join("bin/qemu-img"))
                .unwrap_or_else(|| PathBuf::from("qemu-img"));
            DiskManager::create_overlay(&img, &base, &overlay).await?;
            println!("overlay {} over {}", overlay.display(), base.display());
        }
        Cmd::Boot {
            disk,
            ssh_port,
            display,
            memory,
            smp,
            vm_dir,
            qemu_dir,
        } => {
            let vm_dir = match vm_dir {
                Some(d) => d,
                None => default_vm_dir()?,
            };
            let mut b = HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval)
                .ssh_port(ssh_port)
                .display(display)
                .memory_mb(memory)
                .smp_cores(smp)
                .workdir(vm_dir);
            if let Some(d) = qemu_dir {
                b = b.qemu_dir(d);
            }
            let mut vm = b.boot_existing(&disk).await?;
            let pidfile = vm.builder.pidfile();
            tokio::select! {
                r = vm.wait_for_ssh(Duration::from_secs(10 * 60)) => r?,
                _ = shutdown_signal() => {
                    println!("interrupted during boot; stopping guest...");
                    vm.kill().await?;
                    let _ = std::fs::remove_file(pidfile);
                    return Ok(());
                }
            }
            println!("ephemeral VM ready; press Ctrl-C to shut it down");
            shutdown_signal().await;
            println!("shutting down guest (ACPI)...");
            vm.shutdown_graceful(Duration::from_secs(90)).await?;
            let _ = std::fs::remove_file(pidfile);
            println!("stopped");
        }
        Cmd::Stop { vm_dir, qmp_sock } => {
            use windows_headless::qmp::QmpClient;
            let vm_dir = match vm_dir {
                Some(d) => d,
                None => default_vm_dir()?,
            };
            // Best-effort pid lookup for messaging (kill fallback below).
            let pid = std::fs::read_to_string(vm_dir.join("qemu.pid"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if !windows_headless::qmp_alive(&qmp_sock) {
                // Not reachable: drop a stale pidfile if present, then report.
                let pidfile = vm_dir.join("qemu.pid");
                if pidfile.exists() {
                    let _ = std::fs::remove_file(&pidfile);
                }
                match pid {
                    Some(p) => println!(
                        "no QMP on {qmp_sock}: VM is unreachable (wedged or already gone). If `ps -p {p}` is alive, `kill {p}` to clean up."
                    ),
                    None => println!("no running VM on {qmp_sock}"),
                }
                return Ok(());
            }
            let mut qmp = QmpClient::connect(&qmp_sock).await?;
            qmp.quit().await?;
            for _ in 0..20 {
                if !windows_headless::qmp_alive(&qmp_sock) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let pidfile = vm_dir.join("qemu.pid");
            let _ = std::fs::remove_file(&pidfile);
            if windows_headless::qmp_alive(&qmp_sock) {
                anyhow::bail!(
                    "VM ignored QMP quit; kill it manually (see qemu.pid before cleanup)"
                );
            }
            println!("stopped");
        }
        #[cfg(feature = "downloader")]
        Cmd::FetchQemu { qemu_version, dir } => {
            let q = windows_headless::qemu_portable::ensure(dir, qemu_version).await?;
            println!("portable QEMU {} at {}", q.version, q.root.display());
            for bin in q.system_binaries() {
                println!("  system: {}", bin.display());
            }
            println!("  img:    {}", q.qemu_img().display());
        }
        #[cfg(feature = "downloader")]
        Cmd::Fetch {
            win_iso_url,
            virtio_iso_url,
            dir,
            virtio_sha256,
        } => {
            use windows_headless::downloader;
            let dir = match dir {
                Some(d) => d,
                None => downloader::iso_cache_dir()?,
            };
            // Explicit flag wins; else a pre-fetched time-limited link via env.
            // No page scraper: EvalCenter links require the interactive form.
            // Absent entirely: skip the Eval ISO (e.g. you already have one
            // on disk and only need virtio-win.iso).
            let mut items: Vec<(String, &str, Option<String>)> = Vec::new();
            match downloader::resolve_eval_iso_url(win_iso_url) {
                Ok(u) => items.push((u, "Win11_Enterprise_Eval.iso", None)),
                Err(e) => eprintln!("skipping Eval ISO: {e:#}"),
            }
            items.push((
                virtio_iso_url.unwrap_or_else(|| downloader::STABLE_VIRTIO_URL.to_string()),
                "virtio-win.iso",
                virtio_sha256,
            ));
            for (url, name, hash) in items {
                let dest = dir.join(name);
                // A previously cached file may be a bot-check HTML stub
                // (HTTP 200 with no ISO magic) — drop it so it re-downloads
                // instead of poisoning this run and hanging EDK2 at boot.
                if dest.exists() && downloader::validate_iso_image(&dest).is_err() {
                    eprintln!(
                        "cached {} is not an ISO image; re-downloading",
                        dest.display()
                    );
                    std::fs::remove_file(&dest)?;
                }
                match downloader::download_if_missing(&url, &dest, hash.as_deref()).await? {
                    downloader::DownloadStatus::Cached { path } => {
                        downloader::validate_iso_image(&path)?;
                        println!("cached {}", path.display())
                    }
                    downloader::DownloadStatus::Downloaded { path, sha256 } => {
                        if let Err(e) = downloader::validate_iso_image(&path) {
                            let _ = std::fs::remove_file(&path);
                            return Err(e).with_context(|| {
                                format!("downloaded {} failed validation", path.display())
                            });
                        }
                        println!("downloaded {} (sha256 {sha256})", path.display())
                    }
                }
            }
        }
    }
    Ok(())
}

/// Default VM dir: project cache `vms/win11` (created, never evicted —
/// disk images are tens of GB), or `./win11-vm` without the downloader
/// feature (no cache-manager available).
#[cfg(feature = "downloader")]
fn default_vm_dir() -> Result<PathBuf> {
    use cache_manager::CacheRoot;
    let root = match std::env::var("WINDOWS_HEADLESS_VM_DIR") {
        Ok(d) => CacheRoot::from_root(d),
        Err(_) => CacheRoot::from_discovery()?,
    };
    Ok(root.ensure_group("vms/win11")?)
}

#[cfg(not(feature = "downloader"))]
fn default_vm_dir() -> Result<PathBuf> {
    Ok(PathBuf::from("./win11-vm"))
}

/// Ctrl-C and SIGTERM both mean "stop the VM". Signals don't unwind, so
/// without this a SIGTERM'd CLI would orphan QEMU despite `kill_on_drop`.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
