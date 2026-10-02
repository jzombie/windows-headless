//! Command implementations behind the thin CLI in `main.rs`.
//!
//! Each `cmd_*` takes plain params (no clap types) so the logic is callable
//! from tests and other frontends. Cache-dir and arch defaults live here.
//!
//! Stream rule: stdout carries primary data only (`gen` XML, `report`
//! table, `guest exec` output). All status, progress, prompts, warnings,
//! and ready lines go to stderr — the CLI stays pipe-safe.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ipc::{MON_SOCK_NAME, QMP_SOCK_NAME, SERIAL_LOG_NAME, vm_endpoint};
use crate::qemu::{
    Display, HeadlessVmBuilder, PROVEN_SSH_HOST, PROVEN_SSH_PORT, PROVEN_VNC_DISPLAY,
    PROVEN_VNC_PASSWORD, WindowsVersion, qmp_alive, vnc_url,
};
use crate::storage::DiskManager;
use crate::timeouts;
use crate::unattend::{Architecture, VmConfig, auto_detect_platform};

/// Disk partitioning script laid onto the installer USB (`setup/part.txt`).
const PART_TXT: &str = include_str!("../setup/part.txt");

/// Guest arch: explicit `--arch` wins, else the host arch (HVF/KVM require
/// host == guest, so the default is almost always right).
pub fn resolve_arch(a: Option<Architecture>) -> Architecture {
    a.unwrap_or_else(|| auto_detect_platform().0)
}

/// Scratch dir that deletes itself on drop (all exit paths except SIGKILL).
/// `create` purges any stale contents first (previous kill's leftovers).
pub struct ScratchDir(PathBuf);

impl ScratchDir {
    pub fn create(dir: &Path) -> Result<Self> {
        if dir.exists() {
            eprintln!("purging stale scratch {}", dir.display());
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
        Ok(Self(dir.to_path_buf()))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            eprintln!(
                "warning: scratch cleanup failed ({}): {e:#}",
                self.0.display()
            );
        }
    }
}

/// Default VM dir: project cache `vms/win11` (created, never evicted —
/// disk images are tens of GB), or `./win11-vm` without the downloader
/// feature (no cache-manager available).
#[cfg(feature = "downloader")]
pub fn default_vm_dir() -> Result<PathBuf> {
    let root = crate::cache::resolve_cache_root(None, "WINDOWS_HEADLESS_VM_DIR")?;
    Ok(root.ensure_group(crate::paths::VM_GROUP)?)
}

#[cfg(not(feature = "downloader"))]
pub fn default_vm_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(crate::paths::FALLBACK_VM_DIR))
}

/// Default USB installer image: project cache `media/winusb.dmg`
/// (created, never evicted — 10 GB, slow to rebuild), or `./winusb.dmg`
/// without the downloader feature. `$WINDOWS_HEADLESS_MEDIA_DIR` overrides
/// the cache base, mirroring the ISO cache.
#[cfg(feature = "downloader")]
pub fn default_media_image() -> Result<PathBuf> {
    let root = crate::cache::resolve_cache_root(None, "WINDOWS_HEADLESS_MEDIA_DIR")?;
    Ok(root
        .ensure_group(crate::paths::MEDIA_GROUP)?
        .join(crate::paths::WINUSB_IMAGE))
}

#[cfg(not(feature = "downloader"))]
pub fn default_media_image() -> Result<PathBuf> {
    Ok(PathBuf::from(crate::paths::FALLBACK_WINUSB_IMAGE))
}

/// Expand a leading `~` (shells don't expand it inside quotes, so
/// `'~/Downloads/x.iso'` arrives literally — observed live as a bare
/// ENOENT with no filename). `~` and `~/...` only; other users untouched.
fn expand_tilde(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/").or_else(|| (s == "~").then_some(""))
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    p
}
fn stage_local_dir(base: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    fn walk(base: &Path, rel: &str, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        let dir = if rel.is_empty() {
            base.to_path_buf()
        } else {
            base.join(rel)
        };
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let rp = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if e.file_type()?.is_dir() {
                walk(base, &rp, prefix, out)?;
            } else {
                out.push((format!("{prefix}/{rp}"), std::fs::read(e.path())?));
            }
        }
        Ok(())
    }
    walk(base, "", prefix, out)
}

// ---------------------------------------------------------------------------
// gen
// ---------------------------------------------------------------------------

pub struct GenArgs {
    pub username: String,
    pub password: String,
    pub locale: String,
    pub autologon: bool,
    pub unattend_dir: Option<PathBuf>,
    pub arch: Option<Architecture>,
}

pub fn cmd_gen(a: GenArgs) -> Result<()> {
    let cfg = VmConfig {
        username: a.username,
        password: a.password,
        locale: a.locale,
        auto_logon: a.autologon,
        arch: resolve_arch(a.arch),
        ..VmConfig::default()
    };
    let xml = cfg.generate_unattend_xml();
    if let Some(dir) = a.unattend_dir {
        let f = DiskManager::ensure_unattend_dir(&dir, &xml)?;
        eprintln!("wrote {}", f.display());
    } else {
        println!("{xml}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// build-media
// ---------------------------------------------------------------------------

pub struct BuildMediaArgs {
    pub win_iso: PathBuf,
    pub arch: Option<Architecture>,
    pub virtio_iso: Option<PathBuf>,
    pub drivers_dir: Option<PathBuf>,
    pub openssh_zip: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub wimlib_bin: Option<PathBuf>,
    pub username: String,
    pub password: String,
    pub locale: String,
    /// Rebuild even when the output image + sidecar already exist.
    pub force: bool,
}

/// The `build` pipeline: `setup` (fetch deps) + `build-media` (USB image).
/// One command, one artifact. Requires the downloader feature (fetching is
/// the whole point); the hidden `build-media` stays for no-fetch flows.
#[cfg(feature = "downloader")]
pub struct BuildArgs {
    pub win_iso: PathBuf,
    pub arch: Option<Architecture>,
    pub username: String,
    pub password: String,
    pub locale: String,
    pub out: Option<PathBuf>,
    pub force: bool,
}

#[cfg(feature = "downloader")]
pub async fn cmd_build(a: BuildArgs) -> Result<()> {
    cmd_setup(None, a.arch).await?;
    cmd_build_media(BuildMediaArgs {
        win_iso: a.win_iso,
        arch: a.arch,
        virtio_iso: None,
        drivers_dir: None,
        openssh_zip: None,
        out: a.out,
        wimlib_bin: None,
        username: a.username,
        password: a.password,
        locale: a.locale,
        force: a.force,
    })
    .await
}

pub async fn cmd_build_media(a: BuildMediaArgs) -> Result<()> {
    use crate::media::{
        MediaSpec, build_installer_media, extract_iso_file_to_disk, extract_virtio_netkvm,
        list_iso_tree, split_wim,
    };
    let arch = resolve_arch(a.arch);
    let out = a.out.map(expand_tilde).unwrap_or(default_media_image()?);
    let win_iso = expand_tilde(a.win_iso);
    // Freshness skip: output + sidecar both present means a completed
    // build (sidecar is written last). `--force` rebuilds regardless.
    // Stale inputs with a fresh output are the user's explicit choice.
    if !a.force && out.is_file() && crate::provenance::Provenance::sidecar_path(&out).is_file() {
        eprintln!(
            "already built: {} (sidecar present; --force to rebuild)",
            out.display()
        );
        return Ok(());
    }

    // Invalidate first: a crashed/failed build must never leave a previous
    // sidecar next to a partial image, or the next run skips as "fresh"
    // over garbage (observed live: unusable image reported fresh).
    crate::provenance::Provenance::invalidate(&out);

    // Vendored wimlib: explicit --wimlib-bin wins, else the pinned tree
    // from `fetch-wimlib` (same pattern as `run`). Keep the tag alongside
    // for the provenance sidecar.
    let (wimlib_tag, wimlib_bin) = match a.wimlib_bin {
        Some(b) => ("explicit --wimlib-bin".to_string(), b),
        None => {
            #[cfg(feature = "downloader")]
            {
                let w = crate::qemu_portable::pinned_cached_wimlib()?;
                eprintln!("using pinned portable wimlib {}", w.version);
                (w.version.clone(), w.wimlib_bin())
            }
            #[cfg(not(feature = "downloader"))]
            {
                ("PATH".to_string(), PathBuf::from("wimlib-imagex"))
            }
        }
    };
    eprintln!("building installer media from {} ...", win_iso.display());
    // Driver inputs default to the ISO cache (`setup` fetches them):
    // explicit flags win, else `<iso-cache>/virtio-win.iso` and the
    // per-arch OpenSSH zip. Missing files bail with the fix
    // (`setup`), not a bare NotFound.
    #[cfg(feature = "downloader")]
    let iso_cache = crate::downloader::iso_cache_dir()?;
    let openssh_zip = match a.openssh_zip.map(expand_tilde) {
        Some(p) => p,
        None => {
            #[cfg(feature = "downloader")]
            {
                let p = iso_cache.join(arch.openssh_zip_name());
                if !p.exists() {
                    anyhow::bail!(
                        "{} missing (run `setup` first, or pass --openssh-zip)",
                        p.display()
                    );
                }
                eprintln!("using cached {}", p.display());
                p
            }
            #[cfg(not(feature = "downloader"))]
            {
                anyhow::bail!("pass --openssh-zip <zip>");
            }
        }
    };
    let xml = VmConfig {
        username: a.username.clone(),
        password: a.password.clone(),
        locale: a.locale.clone(),
        arch,
        ..VmConfig::default()
    }
    .generate_unattend_xml();
    // Drivers: explicit tree wins; else extract NetKVM from the
    // virtio ISO in-crate (pure Rust, no host tools). Remember which
    // ISO was used for the provenance sidecar (`None` = --drivers-dir).
    let mut virtio_used: Option<PathBuf> = None;
    let mut staged = vec![
        (crate::paths::UNATTEND_XML.to_string(), xml.into_bytes()),
        (
            crate::paths::PART_TXT_PATH.to_string(),
            PART_TXT.as_bytes().to_vec(),
        ),
        (
            format!("{}/{}", crate::paths::TOOLS_DIR, arch.openssh_zip_name()),
            std::fs::read(&openssh_zip)?,
        ),
    ];
    match (
        a.drivers_dir.map(expand_tilde),
        a.virtio_iso.map(expand_tilde),
    ) {
        (Some(dir), _) => stage_local_dir(&dir, crate::paths::DRIVERS_STAGE_DIR, &mut staged)?,
        (None, Some(iso)) => {
            staged.extend(extract_virtio_netkvm(&iso, arch)?);
            virtio_used = Some(iso);
        }
        (None, None) => {
            #[cfg(feature = "downloader")]
            {
                let p = iso_cache.join(crate::paths::VIRTIO_ISO);
                if !p.exists() {
                    anyhow::bail!(
                        "{} missing (run `setup` first, or pass --virtio-iso/--drivers-dir)",
                        p.display()
                    );
                }
                eprintln!("using cached {}", p.display());
                staged.extend(extract_virtio_netkvm(&p, arch)?);
                virtio_used = Some(p);
            }
            #[cfg(not(feature = "downloader"))]
            {
                anyhow::bail!("pass --virtio-iso <iso> or --drivers-dir <tree>");
            }
        }
    }
    // install.wim must reach wimlib as a file: stream it straight to
    // scratch on disk (7 GB). The old `read_iso_tree` + `fs::write` path
    // held the whole WIM in RAM first; the streaming extractor also
    // tolerates the UDF variants hadris-udf rejects with
    // `invalid Information Control Block` (isomage first, hadris fallback).
    eprintln!("extracting install.wim (several minutes) ...");
    if !win_iso.is_file() {
        anyhow::bail!(
            "Windows ISO {} missing (quote-shielded `~`? the shell only expands it unquoted)",
            win_iso.display()
        );
    }
    // Deterministic scratch under the project cache (not $TMPDIR):
    // a previous killed/failed run leaves it behind, so purge-then-
    // reuse. The Drop guard removes it on every exit path — success,
    // `?` early-return, panic unwind — so only SIGKILL can orphan it
    // (and the next run's purge collects that too). Peak ~14 GB
    // (install.wim + SWM parts); $TMPDIR orphans used to accumulate
    // one full set per aborted run.
    #[cfg(feature = "downloader")]
    let scratch_root = crate::downloader::iso_cache_dir()?
        .parent()
        .map(|p| p.join("scratch"))
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(not(feature = "downloader"))]
    let scratch_root: PathBuf = std::env::temp_dir();
    let scratch = scratch_root.join("build-media");
    let _scratch_guard = ScratchDir::create(&scratch)?;
    let wim_path = scratch.join("install.wim");
    extract_iso_file_to_disk(&win_iso, "sources/install.wim", &wim_path).map_err(|e| {
        let size = std::fs::metadata(&win_iso).map(|m| m.len()).unwrap_or(0);
        anyhow::anyhow!(
            "extract sources/install.wim from {} (ISO {} bytes): {e:#}\n\
             hint: verify the ISO is a complete, untruncated download \
             (compare its size/hash against your Microsoft download source, \
             then re-download or re-copy it if it differs)",
            win_iso.display(),
            size
        )
    })?;
    let wim_len = std::fs::metadata(&wim_path).map(|m| m.len()).unwrap_or(0);
    eprintln!("media: install.wim extracted ({} bytes)", wim_len);
    eprintln!("splitting install.wim with {} ...", wimlib_bin.display());
    let swm_prefix = scratch.join("install.swm");
    let swm_parts = split_wim(&wimlib_bin, &wim_path, &swm_prefix)?;
    // Provenance while install.wim still exists (wimlib info query),
    // sidecar written after the image build succeeds below.
    #[cfg(feature = "downloader")]
    let qemu_tag = crate::qemu_portable::pinned_cached()
        .map(|q| q.version)
        .unwrap_or_else(|_| "none cached".to_string());
    #[cfg(not(feature = "downloader"))]
    let qemu_tag = "unknown (no downloader feature)".to_string();
    let provenance = crate::provenance::Provenance::gather(
        &win_iso,
        &wimlib_bin,
        &wim_path,
        arch,
        &qemu_tag,
        &wimlib_tag,
        virtio_used.as_deref(),
        &a.username,
        &a.locale,
    )?;
    std::fs::remove_file(&wim_path)?;
    // List ISO tree paths without reading content — files are streamed
    // one at a time during image build (peak ~600 MB vs ~6 GB).
    let subtree_skip: (String, Vec<String>) = ("sources".into(), ["install.wim".into()].into());
    let iso_paths = list_iso_tree(
        &win_iso,
        &["efi".into(), "boot".into()],
        &[],
        Some(&subtree_skip),
    )?;
    let spec = MediaSpec {
        iso: win_iso.clone(),
        out: out.clone(),
        size_bytes: 10 * 1024 * 1024 * 1024,
        label: "WINSETUP".to_string(),
        iso_paths,
        staged,
        swm_parts,
    };
    build_installer_media(&spec)?;
    eprintln!("wrote {}", out.display());
    provenance.write_sidecar(&out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

pub struct RunArgs {
    pub disk: Option<String>,
    pub ssh_port: u16,
    pub display: crate::qemu::Display,
    /// Guest RAM in MB (parsed from Docker syntax by clap `value_parser`).
    pub memory: u32,
    /// Guest vCPUs (`--cpus`; `--smp` is an alias).
    pub cpus: u32,
    /// New-disk size in GB (install path only; parsed from Docker syntax).
    pub disk_size: u32,
    pub vm_dir: Option<PathBuf>,
    pub qemu_dir: Option<PathBuf>,
    pub username: String,
    pub password: String,
    pub shell: bool,
    /// Windows ISO: when the disk is missing, run the install chain
    /// (setup + build-media + install) instead of bailing.
    pub win_iso: Option<PathBuf>,
    /// Budget for that install's SSH wait, in seconds (`run` passes it
    /// straight through; only reached when `--win-iso` installs).
    pub install_timeout_secs: u64,
}

pub async fn cmd_run(a: RunArgs) -> Result<()> {
    let vm_dir = match a.vm_dir {
        Some(d) => d,
        None => default_vm_dir()?,
    };
    // Already running? Attach instead of booting: the qcow2 write lock
    // would reject a second QEMU with a cryptic error. A live SSH banner
    // is the ground truth (pidfile alone may be stale).
    if banner_once(a.ssh_port).await.is_some() {
        eprintln!(
            "already running — ssh -p {} {}@{PROVEN_SSH_HOST}",
            a.ssh_port, a.username
        );
        if a.shell {
            crate::ssh::interactive_shell(PROVEN_SSH_HOST, a.ssh_port, &a.username, &a.password)
                .await?;
        }
        return Ok(());
    }
    let _ = std::fs::remove_file(vm_dir.join(crate::paths::QEMU_PIDFILE));
    let disk = a.disk.unwrap_or_else(|| {
        vm_dir
            .join(crate::paths::DISK_IMAGE)
            .to_string_lossy()
            .into_owned()
    });
    // A fresh `qemu-img create` shell is ~200 KB; anything under 1 GB holds
    // no OS (a real install is tens of GB). Treat shells as missing so an
    // interrupted install re-installs instead of booting into UEFI Shell
    // forever (observed live: "guest: Shell>", zero SSH, eternal poll).
    const MIN_VIABLE_DISK: u64 = 1024 * 1024 * 1024;
    let disk_viable = std::fs::metadata(&disk)
        .map(|m| m.len() >= MIN_VIABLE_DISK)
        .unwrap_or(false);
    if !disk_viable {
        match a.win_iso.map(expand_tilde) {
            // Chain: install the disk, then fall through to boot it below.
            // Uses the core only — no shell, no Ctrl-C wait mid-chain. The
            // installer VM is torn down immediately so this command blocks
            // exactly once (in the shared tail after boot), then drops into
            // the guest shell. Explicit `--win-iso` only — never silent.
            Some(iso) => {
                eprintln!(
                    "disk {disk} missing — installing from {} (new {}G disk, {} MB RAM, {} vCPUs) ...",
                    iso.display(),
                    a.disk_size,
                    a.memory,
                    a.cpus,
                );
                let mut session = install_to_ready(InstallParams {
                    usb_image: None,
                    win_iso: Some(iso),
                    screenshots_every_secs: timeouts::SCREENSHOTS_EVERY_SECS,
                    username: a.username.clone(),
                    password: a.password.clone(),
                    recovery: false,
                    auto_updates: false,
                    install_timeout_secs: a.install_timeout_secs,
                    memory_mb: a.memory,
                    cpus: a.cpus,
                    disk_size_gb: a.disk_size,
                    display: a.display.clone(),
                })
                .await?;
                finish_install_capture(&mut session).await;
                session.child.kill().await?;
            }
            None => anyhow::bail!(
                "disk {disk} does not exist (run `install` first, pass --disk <image>, or pass --win-iso to install it now)"
            ),
        }
    }
    let mut b = HeadlessVmBuilder::new(WindowsVersion::Win11)
        .ssh_port(a.ssh_port)
        .display(a.display.clone())
        .memory_mb(a.memory)
        .smp_cores(a.cpus)
        .workdir(vm_dir);
    match a.qemu_dir {
        Some(d) => b = b.qemu_dir(d),
        None => {
            #[cfg(feature = "downloader")]
            {
                let q = crate::qemu_portable::pinned_cached()?;
                eprintln!("using pinned portable QEMU {}", q.version);
                b = b.qemu_dir(q.root);
            }
        }
    }
    eprintln!(
        "booting {disk} ({} MB RAM, {} vCPUs, display {})",
        a.memory,
        a.cpus,
        a.display.as_arg()
    );
    eprintln!(
        "waiting for SSH on {PROVEN_SSH_HOST}:{} (up to {} min) ...",
        a.ssh_port,
        timeouts::BOOT_SSH_DEADLINE.as_secs() / 60
    );
    let mut vm = b.boot_existing(&disk).await?;
    // One monitor session for the whole boot: VNC handshake plus the stall
    // watchdog below share it (single-client transports forbid overlap).
    // Same retrying handshake as install (live-but-slow QEMU refuses early
    // connects).
    let mon = crate::ipc::Monitor::connect_retry(
        &vm.builder.monitor_socket,
        timeouts::MONITOR_CONNECT_SECS,
        || vm.child.try_wait().map(|o| o.is_none()).unwrap_or(false),
    )
    .await?;
    if a.display.is_vnc() {
        mon.command(&format!("change vnc password {PROVEN_VNC_PASSWORD}"))
            .await?;
        let url = vnc_url(&a.display.as_arg()).unwrap_or_else(|| a.display.as_arg());
        eprintln!("VNC {url} (password {PROVEN_VNC_PASSWORD})");
    }
    eprintln!("QEMU launched; guest is booting ...");
    let pidfile = vm.builder.pidfile();
    let start = std::time::Instant::now();
    let mut ticker = tokio::time::interval(timeouts::BOOT_STATUS_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick so the first status line comes
    // after 30s of actual waiting, not at launch.
    ticker.tick().await;
    // Stall sensor: serial-silent deaths would otherwise poll SSH blindly.
    let mut idle = BlockIdle::new();
    tokio::select! {
        r = vm.wait_for_ssh(timeouts::BOOT_SSH_DEADLINE) => {
            // One last frame before failing: `run` has no periodic
            // screenshots and the serial is silent, so the screen is the
            // only evidence of *where* boot died (observed live: black
            // "guest has not initialized the display" vs a Windows
            // prompt — neither is guessable from the timeout alone).
            if r.is_err() {
                let _ = take_screenshot(&mon, &screenshots_dir(&vm.builder.workdir)).await;
            }
            r?;
        }
        _ = async {
            loop {
                ticker.tick().await;
                // Dead-screen watchdog: a fatal serial signature means the
                // boot already failed — report it instead of polling on.
                if let Err(e) = check_boot_failure(&vm.builder.serial_log, &screenshots_dir(&vm.builder.workdir)) {
                    eprintln!("{e:#}");
                    return;
                }
                // Stall watchdog: frozen disk I/O + no banner = parked.
                // Bail propagates out of the select arm below.
                if let Err(e) = idle.poll(&mon).await {
                    eprintln!("{e:#}");
                    return;
                }
                // Narrate the boot: last serial line says what the guest
                // is doing (recovery, Boot Manager, Setup pass...).
                let doing = last_serial_line(&vm.builder.serial_log);
                if doing.is_empty() {
                    eprintln!(
                        "... still waiting for SSH ({}s elapsed)",
                        start.elapsed().as_secs()
                    );
                } else {
                    eprintln!(
                        "... still waiting for SSH ({}s elapsed) — guest: {doing}",
                        start.elapsed().as_secs()
                    );
                }
            }
        } => {
            // Wedged/fatal-signature guest: same evidence gap as the
            // timeout arm — capture the screen before it is torn down.
            let _ = take_screenshot(&mon, &screenshots_dir(&vm.builder.workdir)).await;
            eprintln!("stopping dead guest...");
            vm.kill().await?;
            let _ = std::fs::remove_file(pidfile);
            anyhow::bail!("guest failed to boot (see serial evidence above)");
        }
        _ = shutdown_signal() => {
            eprintln!("interrupted during boot; stopping guest...");
            if let Err(e) = vm.kill().await {
                eprintln!("warning: kill failed: {e:#}");
            }
            let _ = std::fs::remove_file(pidfile);
            return Ok(());
        }
    }
    eprintln!(
        "ready — ssh -p {} {}@{PROVEN_SSH_HOST}",
        a.ssh_port, a.username,
    );
    // Cooked mode, SIGINT/SIGTERM via shutdown_signal — the term-wm way.
    // Raw mode was tried here (Ctrl-C as byte 0x03): under a multiplexer
    // the byte path proved unreliable while kernel signals always land.
    // Echo-trash during the wait is cosmetic; a non-exiting shutdown is
    // functional. Never trade the latter for the former again.
    // Ctrl-C means kill, immediately. No ACPI grace period by explicit
    // user decision: a forgotten snapshot is the user's fault, and a
    // 90-second wait that reads as a hang is worse than a dirty bit that
    // chkdsk replays. The overlay/golden design absorbs the risk.
    shell_then_wait(
        a.shell,
        PROVEN_SSH_HOST,
        a.ssh_port,
        &a.username,
        &a.password,
        "press Ctrl-C to stop the VM (immediate kill, no graceful wait)...",
    )
    .await?;
    eprintln!("killing guest...");
    if let Err(e) = vm.kill().await {
        eprintln!("warning: kill failed: {e:#}");
    }
    let _ = std::fs::remove_file(pidfile);
    eprintln!("stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

/// Live installer session: QEMU still running after the OS reports ready.
/// The caller owns the tail (capture a final frame, optionally offer a
/// shell, wait for Ctrl-C, kill) via the shared helpers below — `cmd_install`
/// supervises it, `cmd_run --win-iso` tears it down immediately and boots
/// the disk instead. One core, no pasted tails.
pub struct InstallSession {
    pub child: tokio::process::Child,
    pub mon: std::sync::Arc<crate::ipc::Monitor>,
    pub vm_dir: PathBuf,
    pub screenshots: Option<tokio::task::JoinHandle<()>>,
}

/// Shared middle of the run/install tails (defined once, called twice):
/// optional guest shell, then block for Ctrl-C/SIGTERM. Killing stays with
/// the caller (run owns a `HeadlessVm` + pidfile, install owns a raw
/// `Child`), so this helper only covers the identical part.
async fn shell_then_wait(
    shell: bool,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    prompt: &str,
) -> Result<()> {
    if shell {
        crate::ssh::interactive_shell(host, port, username, password).await?;
    }
    eprintln!("{prompt}");
    shutdown_signal().await;
    Ok(())
}

/// Shared installer capture step (defined once): stop the time-lapse, keep
/// one final desktop frame as evidence. Called by both the standalone
/// `install` tail and the `run --win-iso` chain teardown.
async fn finish_install_capture(session: &mut InstallSession) {
    if let Some(h) = session.screenshots.take() {
        h.abort();
    }
    // Final frame after victory: the desktop as proven, not mid-install.
    let _ = take_screenshot(&session.mon, &screenshots_dir(&session.vm_dir)).await;
}

/// Inputs for the install-time QEMU argv (pure constructor below): every
/// runtime-resolved path plus the user sizing, so tests can assert the
/// flags reach QEMU without spawning anything.
pub(crate) struct InstallQemuArgv<'a> {
    pub machine: &'a str,
    pub accel: &'a str,
    pub cpu: &'a str,
    pub memory_mb: u32,
    pub cpus: u32,
    pub display: Display,
    pub code_tpl: &'a Path,
    pub vars_fd: &'a Path,
    pub serial_log: &'a str,
    pub mon_sock: &'a str,
    pub qmp_sock: &'a str,
    pub usb_image: &'a Path,
    pub target: &'a str,
    pub arch: Architecture,
}

/// `qemu-img create` args (binary excluded): carries the user disk size.
/// `-q` silences the `Formatting ...` line (our own banner says it).
pub(crate) fn qemu_img_create_args(target: &str, disk_size_gb: u32) -> Vec<String> {
    vec![
        "create".into(),
        "-q".into(),
        "-f".into(),
        "qcow2".into(),
        target.into(),
        format!("{disk_size_gb}G"),
    ]
}

/// Installer QEMU argv (binary excluded): `-m`/`-smp` carry the user sizing.
pub(crate) fn install_qemu_argv(s: &InstallQemuArgv) -> Vec<String> {
    let mut argv = vec![
        "-M".into(),
        s.machine.to_string(),
        "-accel".into(),
        s.accel.to_string(),
        "-cpu".into(),
        s.cpu.to_string(),
        "-m".into(),
        s.memory_mb.to_string(),
        "-smp".into(),
        s.cpus.to_string(),
        "-drive".into(),
        format!(
            "if=pflash,format=raw,file={},readonly=on",
            s.code_tpl.display()
        ),
        "-drive".into(),
        format!("if=pflash,format=raw,file={}", s.vars_fd.display()),
        "-display".into(),
        s.display.to_string(),
        "-serial".into(),
        format!("file:{}", s.serial_log),
        "-drive".into(),
        format!(
            "file={},format=raw,if=none,id=usbstick",
            s.usb_image.display()
        ),
        "-netdev".into(),
        format!("user,id=n1,hostfwd=tcp::{}-:22", PROVEN_SSH_PORT),
        "-device".into(),
        "virtio-net-pci,netdev=n1".into(),
        "-monitor".into(),
        crate::ipc::qemu_arg(s.mon_sock),
        "-qmp".into(),
        crate::ipc::qemu_arg(s.qmp_sock),
    ];
    // xHCI first: neither q35 nor virt ships a USB bus, so usb-storage and
    // the ARM64 input devices below would be bus-less without it.
    argv.push("-device".into());
    argv.push("qemu-xhci,id=xhci".into());
    // ARM64 virt ships with NO input/display devices of its own:
    // USB keyboard (send-key needs it) + USB tablet (VNC
    // pointer needs it), and ramfb as the one framebuffer (single
    // console: VNC and screendump keep working after Windows takes
    // over, unlike virtio-gpu). x86/pc has PS/2 + VGA by default.
    if s.arch == Architecture::Arm64 {
        argv.push("-device".into());
        argv.push("usb-kbd".into());
        argv.push("-device".into());
        argv.push("usb-tablet".into());
        argv.push("-device".into());
        argv.push("ramfb".into());
    }
    // Removable USB media (never ide-hd). Two separate things decide
    // whether Setup finds its media and where it installs — they were
    // conflated here before, and both were verified live:
    //   * partition type (media.rs): WinPE letters a Microsoft Basic Data
    //     FAT32 volume, but mounts an ESP — or any GUID it won't letter —
    //     Hidden with no drive letter, so Setup never sees
    //     sources/install.swm and parks at "Install driver to show hardware";
    //   * enumeration order: USB storage lands behind the NVMe (Disk 0 =
    //     the target), while ide-hd came first (Disk 0 = the 10 GB media),
    //     and the unattend installs to Disk 0.
    argv.push("-device".into());
    argv.push("usb-storage,drive=usbstick,removable=true,bootindex=1".into());
    argv.push("-drive".into());
    argv.push(format!("file={},format=qcow2,if=none,id=tgt", s.target));
    argv.push("-device".into());
    argv.push("nvme,drive=tgt,serial=nvme1,bootindex=0".into());
    argv
}

/// One orchestrator for all install stages: grouping the params in a
/// struct beats counting them in a lint attribute.
pub struct InstallParams {
    pub usb_image: Option<PathBuf>,
    pub win_iso: Option<PathBuf>,
    pub screenshots_every_secs: u64,
    pub username: String,
    pub password: String,
    pub recovery: bool,
    pub auto_updates: bool,
    pub install_timeout_secs: u64,
    pub memory_mb: u32,
    pub cpus: u32,
    pub disk_size_gb: u32,
    pub display: crate::qemu::Display,
}

/// Proven NVMe+USB unattended install: installer USB + NVMe target,
/// answer file, probe-gated SSH wait. Both platforms present the media
/// as removable USB storage (bootindex 1) behind xHCI and the target as
/// NVMe (bootindex 0): that is the topology the macOS run proved, and
/// Linux now shares it (its old IDE-HD branch enumerated the 10 GB media
/// first, as Disk 0, while the unattend installs to Disk 0).
///
/// Core only: leaves the installer QEMU running and returns it. No shell,
/// no Ctrl-C wait, no kill — the caller picks the tail via
/// `shell_then_wait` + `finish_install_capture`.
///
/// Run from the repo root (USB media must already exist).
async fn install_to_ready(p: InstallParams) -> Result<InstallSession> {
    // Sockets, serial log, SSH port and VNC settings match what `run`
    // boots: every endpoint is scoped to this VM dir (never /tmp —
    // world-writable-sticky, symlink squatting, cross-VM collisions).
    let (arch, _) = auto_detect_platform();

    let vm_dir = default_vm_dir()?;
    let mon_sock = vm_endpoint(&vm_dir, MON_SOCK_NAME);
    let qmp_sock = vm_endpoint(&vm_dir, QMP_SOCK_NAME);
    let serial_log = vm_endpoint(&vm_dir, SERIAL_LOG_NAME);
    // Install into the same disk `run` boots by default, so plain `run`
    // starts what this installed (no --disk needed).
    let target = vm_dir.join(crate::paths::DISK_IMAGE);
    let target_s = target.to_string_lossy().into_owned();
    // One NVRAM file shared with `run`. The install refreshes it from the
    // template (a reused VARS carries stale boot entries that shadow the
    // installer's ISO — BlInitializeLibrary 0xc00000bb), then keeps it
    // across Setup's own reboots; `run` never refreshes it, so the guest's
    // firmware state survives its boots.
    let vars_fd = vm_dir.join(crate::paths::VARS_FILE);
    let run_log = vm_dir.join(crate::paths::INSTALL_LOG);
    let usb_image = p.usb_image.unwrap_or(default_media_image()?);
    if !usb_image.is_file() {
        match p.win_iso {
            // Chain: fetch deps + build the USB image, then install from it.
            // Explicit `--win-iso` only — never silent (setup downloads).
            #[cfg(feature = "downloader")]
            Some(iso) => {
                eprintln!(
                    "USB installer media missing — building it from {} (one-time; downloads tools, then builds a ~7 GB image) ...",
                    iso.display()
                );
                cmd_setup(None, None).await?;
                cmd_build_media(BuildMediaArgs {
                    win_iso: iso,
                    arch: None,
                    virtio_iso: None,
                    drivers_dir: None,
                    openssh_zip: None,
                    out: Some(usb_image.clone()),
                    wimlib_bin: None,
                    username: p.username.clone(),
                    password: p.password.clone(),
                    locale: "en-US".to_string(),
                    // Chain runs only when the image is absent — nothing to force.
                    force: false,
                })
                .await?;
            }
            #[cfg(not(feature = "downloader"))]
            Some(_) => anyhow::bail!("--win-iso chaining needs the downloader feature"),
            None => anyhow::bail!(
                "USB installer image {} missing — build it with `build-media --win-iso <iso>`, pass --usb-image <image>, or pass --win-iso to build it now",
                usb_image.display()
            ),
        }
    }

    // Portable QEMU tree: the pinned release under the downloader
    // feature, else the pinned version dir under `./.cache/qemu-portable`.
    #[cfg(feature = "downloader")]
    let qdir = crate::qemu_portable::pinned_cached()?.root;
    #[cfg(not(feature = "downloader"))]
    let qdir = {
        // pins + paths (not qemu_portable): that module is downloader-gated.
        // The tag is the cache dir name, verbatim — no mangling.
        use crate::paths::{FALLBACK_QEMU_CACHE, PORTABLE_BIN_DIR, QEMU_IMG_NAME, QEMU_INNER};
        use crate::pins::QEMU_PORTABLE_TAG;
        let dir = std::env::current_dir()?
            .join(FALLBACK_QEMU_CACHE)
            .join(QEMU_PORTABLE_TAG)
            .join(QEMU_INNER);
        anyhow::ensure!(
            dir.join(PORTABLE_BIN_DIR).join(QEMU_IMG_NAME).is_file(),
            "pinned portable QEMU {QEMU_PORTABLE_TAG} not cached in {} (no downloader feature to fetch it)",
            dir.display()
        );
        dir
    };
    let sys_bin = qdir.join(format!("bin/{}", arch.qemu_binary()));
    let img_bin = qdir
        .join(crate::paths::PORTABLE_BIN_DIR)
        .join(crate::paths::QEMU_IMG_NAME);
    let share = qdir.join("share/qemu");
    let (code_names, vars_names) = arch.firmware_names();
    let code_tpl = code_names
        .iter()
        .map(|n| share.join(n))
        .find(|p| p.is_file());
    let vars_tpl = vars_names
        .iter()
        .map(|n| share.join(n))
        .find(|p| p.is_file());
    let (code_tpl, vars_tpl) = match (code_tpl, vars_tpl) {
        (Some(c), Some(v)) => (c, v),
        _ => anyhow::bail!("no UEFI firmware for {arch:?} in {}", share.display()),
    };
    for p in [&sys_bin, &img_bin] {
        if !p.exists() {
            anyhow::bail!("missing required file: {}", p.display());
        }
    }
    // CODE runs straight from the portable tree (readonly, same bytes —
    // no scratch copy). VARS is refreshed per install from the template:
    // a zeroed NVRAM file breaks OVMF initialisation (BlInitializeLibrary
    // 0xc00000bb) because the firmware needs the template's runtime
    // services data to present correct UEFI protocols to the Boot Manager.
    let _ = std::fs::remove_file(&vars_fd);
    std::fs::copy(&vars_tpl, &vars_fd).with_context(|| {
        format!(
            "copy VARS template {} -> {}",
            vars_tpl.display(),
            vars_fd.display()
        )
    })?;
    let _ = std::fs::remove_file(&target);
    eprintln!(
        "installing Windows to {target_s} (new {}G disk, {} MB RAM, {} vCPUs) ...",
        p.disk_size_gb, p.memory_mb, p.cpus
    );
    let st = tokio::process::Command::new(&img_bin)
        .args(qemu_img_create_args(&target_s, p.disk_size_gb))
        .status()
        .await?;
    if !st.success() {
        anyhow::bail!("qemu-img create failed");
    }
    for s in [&mon_sock, &qmp_sock] {
        crate::ipc::stale_remove(s);
    }
    let out = std::fs::File::create(&run_log)?;
    eprintln!("QEMU output log: {}", run_log.display());
    // Same probe as `run`/boot argv (unattend::resolve_accel): Linux
    // without usable /dev/kvm (OrbStack, containers) falls back to tcg
    // instead of dying on `-accel kvm`. CPU follows the accel: `host`
    // passthrough on hardware accel, `max` under TCG (the `virt` default
    // is too old for Windows 11 ARM64).
    let (_, accel) = auto_detect_platform();
    let cpu = if accel == "tcg" { "max" } else { "host" };
    // (No extra warning here: resolve_accel already warned via tracing.)
    let machine = crate::qemu::machine_for(arch);
    let mut child = tokio::process::Command::new(&sys_bin);
    child.args(install_qemu_argv(&InstallQemuArgv {
        machine,
        accel,
        cpu,
        memory_mb: p.memory_mb,
        cpus: p.cpus,
        display: p.display.clone(),
        code_tpl: &code_tpl,
        vars_fd: &vars_fd,
        serial_log: &serial_log,
        mon_sock: &mon_sock,
        qmp_sock: &qmp_sock,
        usb_image: &usb_image,
        target: &target_s,
        arch,
    }));
    child
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .kill_on_drop(true);
    // Own process group (Unix): same tty-SIGINT rationale as qemu.rs spawn —
    // the installer's Ctrl-C must reach only the CLI, never QEMU directly.
    // Windows: CREATE_NEW_PROCESS_GROUP, same rationale (see qemu.rs).
    #[cfg(unix)]
    {
        child.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// Win32 `CREATE_NEW_PROCESS_GROUP` (no winapi dep for one flag).
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        child.as_std_mut().creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = child.spawn()?;
    let child_pid = child.id();
    eprintln!(
        "QEMU started (pid {child_pid:?}); waiting for the monitor (up to {}s) ...",
        timeouts::MONITOR_CONNECT_SECS
    );
    // Retry the REAL handshake, never a probe connect: QEMU's listen
    // backlog is a single slot and a probe occupies it, refusing the real
    // handshake with ECONNREFUSED on a live QEMU (observed). Retry while
    // the child lives, bail on death — never on a live child, or
    // kill_on_drop destroys the evidence (the old 3 s bail did exactly
    // that to slow hvf starters).
    let mon =
        match crate::ipc::Monitor::connect_retry(&mon_sock, timeouts::MONITOR_CONNECT_SECS, || {
            child.try_wait().map(|o| o.is_none()).unwrap_or(false)
        })
        .await
        {
            Ok(m) => m,
            Err(e) => {
                let status = child
                    .try_wait()
                    .map(|o| {
                        o.map(|s| s.to_string())
                            .unwrap_or_else(|| "still alive".into())
                    })
                    .unwrap_or_else(|e| format!("status unknown ({e:#})"));
                let _ = child.kill().await;
                anyhow::bail!(
                    "QEMU (pid {child_pid:?}, status {status}): {e:#}; check {}",
                    run_log.display()
                );
            }
        };
    // VNC-only step (same guard as the boot path): cocoa/gtk/sdl have
    // no VNC server to p.password-protect.
    if p.display.is_vnc() {
        mon.command(&format!("change vnc p.password {PROVEN_VNC_PASSWORD}"))
            .await?;
    }
    let mon = std::sync::Arc::new(mon);
    let url = vnc_url(&p.display.as_arg()).unwrap_or_else(|| p.display.as_arg());
    if p.display.is_vnc() {
        eprintln!(
            "Windows installer is running — watch it at {url} (VNC p.password: {PROVEN_VNC_PASSWORD})."
        );
    } else {
        eprintln!("Windows installer is running on the {url} display.");
    }
    eprintln!(
        "This takes a while (much longer under TCG emulation); SSH answers when Setup finishes."
    );
    let screenshots = if p.screenshots_every_secs > 0 {
        let dir = screenshots_dir(&vm_dir);
        eprintln!(
            "Saving a screenshot every {}s until then → {}",
            p.screenshots_every_secs,
            dir.display()
        );
        Some(spawn_screenshots(
            std::sync::Arc::clone(&mon),
            dir,
            Duration::from_secs(p.screenshots_every_secs),
        ))
    } else {
        None
    };
    let shots_dir = screenshots_dir(&vm_dir);
    let scope = GuestScope {
        mon: std::sync::Arc::clone(&mon),
        shots_dir,
        serial_log: serial_log.clone(),
    };
    tokio::select! {
        r = wait_install_ready(
            PROVEN_SSH_PORT,
            // A progressing install, not a parked one: the wait's own
            // no-I/O watchdog (`timeouts::IDLE_BAIL_SECS`) already fails
            // fast on a guest that stops moving, so this only bounds how
            // long a *slow* host may keep applying the image (hours on a
            // ~1 MB/s machine).
            Duration::from_secs(p.install_timeout_secs),
            &p.username,
            &p.password,
            p.recovery,
            p.auto_updates,
            &scope,
        ) => {
            let banner = r?;
            // Committed proof: one tested-isos.jsonl line from the image's
            // sidecar, plus a README table refresh when the markers exist.
            // Best-effort throughout — images built before sidecars existed
            // still install fine, they just leave no record. (The probe
            // already authenticated as `p.username`, so the CLI value is
            // authoritative for the ready line.)
            match crate::provenance::TestRecord::load_sidecar(&usb_image) {
                Ok(prov) => {
                    match crate::provenance::TestRecord::append(prov, &banner) {
                        Ok(_) => {
                            use crate::provenance::TestRecord;
                            let readme = PathBuf::from(README_FILE);
                            if readme.is_file() {
                                let log = PathBuf::from(TestRecord::LOG);
                                if let Err(e) = cmd_report(ReportArgs { log, readme: Some(readme) }) {
                                    eprintln!("warning: README table refresh failed: {e:#}");
                                }
                            }
                        }
                        Err(e) => eprintln!("warning: test record append failed: {e:#}"),
                    }
                }
                Err(e) => eprintln!("warning: no sidecar, skipping test record: {e:#}"),
            }
            eprintln!("ready — ssh -p {} {}@{PROVEN_SSH_HOST}", PROVEN_SSH_PORT, p.username);
        }
        _ = shutdown_signal() => {
            eprintln!("interrupted; stopping guest...");
            if let Some(h) = &screenshots {
                h.abort();
            }
            child.kill().await?;
            // Interrupt is NOT success: bail so callers (`run --win-iso`)
            // stop the chain instead of booting a half-installed disk.
            anyhow::bail!("interrupted during install; disk left half-written (rerun to start over)");
        }
    }
    Ok(InstallSession {
        child,
        mon,
        vm_dir,
        screenshots,
    })
}

/// Standalone `install`: full install, then supervise the VM for a human
/// via the shared tail (final frame + optional shell + Ctrl-C wait + kill).
/// `shell` only affects this tail, never the install itself.
pub async fn cmd_install(p: InstallParams, shell: bool) -> Result<()> {
    let username = p.username.clone();
    let password = p.password.clone();
    let mut session = install_to_ready(p).await?;
    finish_install_capture(&mut session).await;
    shell_then_wait(
        shell,
        PROVEN_SSH_HOST,
        PROVEN_SSH_PORT,
        &username,
        &password,
        "press Ctrl-C to shut the VM down cleanly",
    )
    .await?;
    session.child.kill().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

pub struct ReportArgs {
    pub log: PathBuf,
    /// README to splice the table into (between TESTED markers). If absent,
    /// the table prints to stdout for manual pasting.
    pub readme: Option<PathBuf>,
}

/// README filename refreshed automatically by `install` (repo root,
/// current dir at runtime). Marker-gated: absent markers mean warn, never
/// blind rewrite.
pub const README_FILE: &str = "README.md";

const REPORT_BEGIN: &str = "<!-- TESTED-BEGIN -->";
const REPORT_END: &str = "<!-- TESTED-END -->";

fn epoch_to_date(epoch: u64) -> String {
    // No chrono dep: days since epoch → approximate YYYY-MM-DD via a tiny
    // civil-date conversion (Howard Hinnant's algorithm).
    let days = (epoch / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

/// Release page URL for a tag in our portable repo (used to link generated
/// report cells to the exact artifacts consumed).
fn release_tag_url(tag: &str) -> String {
    format!(
        "https://github.com/{}/releases/tag/{tag}",
        crate::pins::RELEASE_REPO
    )
}

/// Render `tested-isos.jsonl` as a markdown table: the human-readable face
/// of the machine-readable log. Short commit + `*` when the build was dirty
/// (hash alone doesn't reproduce it); virtio as short SHA of the consumed
/// ISO (name when `--drivers-dir` bypassed the ISO). Newest first: the log
/// is append-only, so rows are collected then reversed for display.
/// QEMU/wimlib/virtio cells link to their exact release pages.
pub fn render_report(log: &Path) -> Result<String> {
    let text = std::fs::read_to_string(log).with_context(|| format!("read {}", log.display()))?;
    let mut rows = vec![
        "| Date | ISO | Arch | Accel | Host | QEMU | wimlib | virtio | Commit | Result |"
            .to_string(),
        "|---|---|---|---|---|---|---|---|---|---|".to_string(),
    ];
    let mut entries = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let r: crate::provenance::TestRecord = serde_json::from_str(line)
            .with_context(|| format!("{} line {}", log.display(), n + 1))?;
        let p = &r.provenance;
        let commit_short = p.build_commit.chars().take(7).collect::<String>();
        // Clean-tree hashes exist upstream — link them. Dirty-tree hashes
        // (`*`) don't, so they stay plain text (a link would 404).
        let commit = if p.build_dirty {
            format!("{commit_short}*")
        } else {
            format!(
                "[{commit_short}]({}/commit/{})",
                env!("CARGO_PKG_REPOSITORY"),
                p.build_commit
            )
        };
        let virtio = match &p.deps.virtio_sha256 {
            Some(sha) => format!(
                "[{}]({})",
                sha.chars().take(7).collect::<String>(),
                release_tag_url(crate::pins::DRIVERS_RELEASE_TAG)
            ),
            None => p.deps.virtio_iso_name.clone(),
        };
        entries.push(format!(
            "| {} | {} | {} | {} | {} {} | [{}]({}) | [{}]({}) | {} | {} | {} |",
            epoch_to_date(r.tested_at_epoch),
            p.source_iso.file_name,
            p.guest.arch,
            p.guest.accel,
            p.host.os,
            p.host.os_version,
            p.deps.qemu_tag,
            release_tag_url(&p.deps.qemu_tag),
            p.deps.wimlib_tag,
            release_tag_url(&p.deps.wimlib_tag),
            virtio,
            commit,
            r.outcome,
        ));
    }
    entries.reverse();
    rows.extend(entries);
    Ok(rows.join("\n") + "\n")
}

pub fn cmd_report(a: ReportArgs) -> Result<()> {
    let table = render_report(&a.log)?;
    match a.readme {
        None => print!("{table}"),
        Some(readme) => {
            let text = std::fs::read_to_string(&readme)?;
            let (head, rest) = text
                .split_once(REPORT_BEGIN)
                .with_context(|| format!("{} has no {REPORT_BEGIN} marker", readme.display()))?;
            let (_, tail) = rest
                .split_once(REPORT_END)
                .with_context(|| format!("{} has no {REPORT_END} marker", readme.display()))?;
            let updated = format!("{head}{REPORT_BEGIN}\n{table}{REPORT_END}{tail}");
            std::fs::write(&readme, updated)?;
            eprintln!("updated {}", readme.display());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// guest
// ---------------------------------------------------------------------------

/// Guest verbs (work on any booted machine via SSH). Same verbs the
/// install flow uses internally — exposed here for live machines.
#[derive(clap::Subcommand)]
pub enum GuestOp {
    /// Execute one command, print stdout.
    Exec {
        /// Command line (joined with spaces).
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Rotate the guest password (verified by fresh login).
    SetPassword {
        /// New password (never logged).
        new_password: String,
    },
    /// Set Automatic Repair on/off.
    SetRecovery {
        /// `true` keeps it, `false` disables it.
        #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
        enabled: bool,
    },
    /// Set automatic Windows updates on/off.
    SetUpdates {
        /// `true` enables wuauserv, `false` disables it.
        #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
        enabled: bool,
    },
    /// Ask the guest whether Setup is finished (READY/BUSY verdict).
    Probe,
}

pub struct GuestArgs {
    pub op: GuestOp,
    pub ssh_port: u16,
    pub username: String,
    pub password: String,
}

pub async fn cmd_guest(a: GuestArgs) -> Result<()> {
    use crate::qemu::PROVEN_SSH_HOST;
    let host = PROVEN_SSH_HOST;
    match a.op {
        GuestOp::Exec { cmd } => {
            let out =
                crate::ssh::ssh_exec(host, a.ssh_port, &a.username, &a.password, &cmd.join(" "))
                    .await?;
            print!("{out}");
        }
        GuestOp::SetPassword { new_password } => {
            crate::ssh::change_guest_password(
                host,
                a.ssh_port,
                &a.username,
                &a.password,
                &new_password,
            )
            .await?;
            eprintln!("password rotated (verified by fresh login)");
        }
        GuestOp::SetRecovery { enabled } => {
            crate::ssh::set_recovery_enabled(host, a.ssh_port, &a.username, &a.password, enabled)
                .await?;
        }
        GuestOp::SetUpdates { enabled } => {
            crate::ssh::set_auto_updates(host, a.ssh_port, &a.username, &a.password, enabled)
                .await?;
        }
        GuestOp::Probe => {
            let done =
                crate::ssh::probe_setup_done(host, a.ssh_port, &a.username, &a.password).await?;
            eprintln!("{}", if done { "READY" } else { "BUSY" });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// overlay / stop
// ---------------------------------------------------------------------------

pub async fn cmd_overlay(
    base: Option<PathBuf>,
    overlay: PathBuf,
    qemu_dir: Option<PathBuf>,
) -> Result<()> {
    // Base defaults to the disk `run` boots — overlays are deltas over it.
    let base = match base {
        Some(b) => b,
        None => default_vm_dir()?.join(crate::paths::DISK_IMAGE),
    };
    if !base.is_file() {
        anyhow::bail!("base {} missing (run `install` first)", base.display());
    }
    let img = qemu_dir
        .map(|d| {
            d.join(crate::paths::PORTABLE_BIN_DIR)
                .join(crate::paths::QEMU_IMG_NAME)
        })
        .unwrap_or_else(|| PathBuf::from(crate::paths::QEMU_IMG_NAME));
    DiskManager::create_overlay(&img, &base, &overlay).await?;
    eprintln!("overlay {} over {}", overlay.display(), base.display());
    Ok(())
}

pub async fn cmd_stop(vm_dir: Option<PathBuf>, qmp_sock: Option<String>) -> Result<()> {
    use crate::qmp::QmpClient;
    let vm_dir = match vm_dir {
        Some(d) => d,
        None => default_vm_dir()?,
    };
    // Default QMP endpoint lives next to the disk (never /tmp); explicit
    // --qmp-sock overrides (URLs/paths as given).
    let qmp_sock = qmp_sock.unwrap_or_else(|| vm_endpoint(&vm_dir, QMP_SOCK_NAME));
    // Best-effort pid lookup for messaging (kill fallback below).
    let pid = std::fs::read_to_string(vm_dir.join(crate::paths::QEMU_PIDFILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if !qmp_alive(&qmp_sock).await {
        // Not reachable: drop a stale pidfile if present, then report.
        let pidfile = vm_dir.join(crate::paths::QEMU_PIDFILE);
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
    // Drop the session before probing: single-client transports refuse a
    // probe while our own session is open (false "dead").
    drop(qmp);
    for _ in 0..timeouts::QMP_QUIT_POLL_COUNT {
        if !qmp_alive(&qmp_sock).await {
            break;
        }
        tokio::time::sleep(timeouts::QMP_QUIT_POLL_INTERVAL).await;
    }
    let pidfile = vm_dir.join(crate::paths::QEMU_PIDFILE);
    let _ = std::fs::remove_file(&pidfile);
    if qmp_alive(&qmp_sock).await {
        anyhow::bail!(
            "VM ignored QMP quit; kill it manually (see {} before cleanup)",
            crate::paths::QEMU_PIDFILE
        );
    }
    eprintln!("stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// downloader-backed commands
// ---------------------------------------------------------------------------

/// Download one file into place (skip when cached and valid). Shared by
/// `fetch` and `setup` so both print the same `cached|downloaded` lines.
/// `force` re-downloads even a valid cache entry (for URL-rotated
/// artifacts like the Eval ISO); `setup` always passes false.
#[cfg(feature = "downloader")]
pub async fn fetch_one(
    url: &str,
    dest: &std::path::Path,
    hash: Option<&str>,
    force: bool,
) -> Result<()> {
    use crate::downloader;
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dest.display().to_string());
    let is_iso = name.ends_with(".iso");
    // A previously cached file may be a bot-check HTML stub
    // (HTTP 200 with no ISO magic) — drop it so it re-downloads
    // instead of poisoning this run and hanging EDK2 at boot.
    if is_iso && dest.exists() && downloader::validate_iso_image(dest).is_err() {
        eprintln!(
            "cached {} is not an ISO image; re-downloading",
            dest.display()
        );
        std::fs::remove_file(dest)?;
    }
    match downloader::download_if_missing(url, dest, hash, force).await? {
        downloader::DownloadStatus::Cached { path } => {
            if is_iso {
                downloader::validate_iso_image(&path)?;
            }
            eprintln!("cached {}", path.display())
        }
        downloader::DownloadStatus::Downloaded { path, sha256 } => {
            if is_iso && let Err(e) = downloader::validate_iso_image(&path) {
                let _ = std::fs::remove_file(&path);
                return Err(e)
                    .with_context(|| format!("downloaded {} failed validation", path.display()));
            }
            eprintln!("downloaded {} (sha256 {sha256})", path.display())
        }
    }
    Ok(())
}

#[cfg(feature = "downloader")]
pub async fn cmd_fetch_qemu(
    qemu_tag: Option<String>,
    dir: Option<PathBuf>,
    allow_unverified: bool,
) -> Result<()> {
    let q = crate::qemu_portable::ensure(dir, qemu_tag, allow_unverified).await?;
    eprintln!("portable QEMU {} at {}", q.version, q.root.display());
    for bin in q.system_binaries() {
        eprintln!("  system: {}", bin.display());
    }
    eprintln!("  img:    {}", q.qemu_img().display());
    Ok(())
}

#[cfg(feature = "downloader")]
pub struct FetchArgs {
    pub win_iso_url: Option<String>,
    pub win_iso_sha256: Option<String>,
    pub virtio_iso_url: Option<String>,
    pub dir: Option<PathBuf>,
    pub virtio_sha256: Option<String>,
    pub openssh: bool,
    pub arch: Option<Architecture>,
    /// Re-download even valid cache entries (for URL-rotated artifacts
    /// like the Eval ISO, whose slot name can't see upstream changes).
    pub force: bool,
}

#[cfg(feature = "downloader")]
pub async fn cmd_fetch(a: FetchArgs) -> Result<()> {
    use crate::downloader;
    let dir = match a.dir {
        Some(d) => d,
        None => downloader::iso_cache_dir()?,
    };
    // Explicit flag wins; else a pre-fetched time-limited link via env.
    // No page scraper: EvalCenter links require the interactive form.
    // Absent entirely: skip the Eval ISO (e.g. you already have one
    // on disk and only need virtio-win.iso).
    let mut items: Vec<(String, &str, Option<String>)> = Vec::new();
    match downloader::resolve_eval_iso_url(a.win_iso_url) {
        Ok(u) => {
            // Explicit flag wins, else $WIN11_EVAL_ISO_SHA256; absent = TLS only.
            let eval_hash = a
                .win_iso_sha256
                .or_else(|| std::env::var("WIN11_EVAL_ISO_SHA256").ok());
            items.push((u, crate::paths::EVAL_ISO_NAME, eval_hash))
        }
        Err(e) => eprintln!("skipping Windows ISO: {e:#}"),
    }
    // virtio-win.iso: explicit URL wins, else our vendored release
    // asset (hash from the release sidecar). No fallback: a failed
    // lookup means the pin is wrong or GitHub is unreachable, and
    // silently substituting unpinned upstream bytes would hide either.
    let (virtio_url, virtio_hash) = match a.virtio_iso_url {
        Some(u) => {
            if a.virtio_sha256.is_none() {
                tracing::warn!(
                    "custom --virtio-iso-url without --virtio-sha256: virtio-win.iso will be TLS-only (no hash verification)"
                );
            }
            (u, a.virtio_sha256)
        }
        None => {
            let (u, h) = downloader::virtio_release_info().await.with_context(|| {
                format!(
                    "virtio release lookup failed for {} (check the pin and GitHub API rate limits)",
                    downloader::VIRTIO_RELEASE_TAG
                )
            })?;
            (u, a.virtio_sha256.or(Some(h)))
        }
    };
    items.push((virtio_url, crate::paths::VIRTIO_ISO, virtio_hash));
    if a.openssh {
        let arch = resolve_arch(a.arch);
        items.push((
            downloader::openssh_zip_url(arch),
            arch.openssh_zip_name(),
            Some(arch.openssh_sha256().to_string()),
        ));
    }
    for (url, name, hash) in items {
        fetch_one(&url, &dir.join(name), hash.as_deref(), a.force).await?;
    }
    Ok(())
}

#[cfg(feature = "downloader")]
pub async fn cmd_setup(virtio_iso_url: Option<String>, arch: Option<Architecture>) -> Result<()> {
    use crate::downloader;
    let arch = resolve_arch(arch);
    let q = crate::qemu_portable::ensure(None, None, false).await?;
    eprintln!("portable QEMU {} at {}", q.version, q.root.display());
    let w = crate::qemu_portable::ensure_wimlib(None, None, false).await?;
    eprintln!("portable wimlib {} at {}", w.version, w.root.display());
    let dir = downloader::iso_cache_dir()?;
    let (virtio_url, virtio_hash) = match virtio_iso_url {
        Some(u) => {
            tracing::warn!(
                "custom virtio ISO URL without --virtio-sha256: virtio-win.iso will be TLS-only (no hash verification)"
            );
            (u, None)
        }
        None => {
            let (u, h) = downloader::virtio_release_info().await?;
            (u, Some(h))
        }
    };
    fetch_one(
        &virtio_url,
        &dir.join(crate::paths::VIRTIO_ISO),
        virtio_hash.as_deref(),
        false,
    )
    .await?;
    fetch_one(
        &downloader::openssh_zip_url(arch),
        &dir.join(arch.openssh_zip_name()),
        Some(arch.openssh_sha256()),
        false,
    )
    .await?;
    eprintln!("setup complete (Windows ISO still yours: pass --win-iso to build-media)");
    Ok(())
}

#[cfg(feature = "downloader")]
pub async fn cmd_fetch_wimlib(
    wimlib_tag: Option<String>,
    dir: Option<PathBuf>,
    allow_unverified: bool,
) -> Result<()> {
    let w = crate::qemu_portable::ensure_wimlib(dir, wimlib_tag, allow_unverified).await?;
    eprintln!("portable wimlib {} at {}", w.version, w.root.display());
    eprintln!("  bin: {}", w.wimlib_bin().display());
    Ok(())
}

/// Verify every pinned external artifact WITHOUT downloading archives or
/// installing anything (CI-friendly: KBs of metadata, plus two ~5 MB OpenSSH
/// zips). Checks, per artifact:
/// 1. the release asset exists (availability for that environment), and
/// 2. its hash matches the code-pinned expectation.
///
/// Hash-only sources are preferred wherever they exist (`SHA256SUMS.txt`,
/// virtio `.sha256` sidecar); only OpenSSH (no upstream sums) downloads the
/// payload itself. Prints one line per check; returns an error summarizing
/// all failures so a single CI run reports everything at once.
#[cfg(feature = "downloader")]
pub async fn cmd_verify_pins() -> Result<()> {
    use crate::downloader;
    use crate::qemu_portable::{parse_sums, release_by_tag};
    let mut failures: Vec<String> = Vec::new();
    let ok = |label: &str| eprintln!("pin ok: {label}");

    // Portable QEMU + wimlib: all three platform markers, sums hash vs pin.
    for (product, tag, prefix, is_wimlib) in [
        (
            "qemu",
            crate::pins::QEMU_PORTABLE_TAG,
            crate::paths::QEMU_ASSET_PREFIX,
            false,
        ),
        (
            "wimlib",
            crate::pins::WIMLIB_PORTABLE_TAG,
            crate::paths::WIMLIB_ASSET_PREFIX,
            true,
        ),
    ] {
        let assets = match release_by_tag(tag).await {
            Ok(a) => a,
            Err(e) => {
                failures.push(format!("{product} {tag}: release lookup failed: {e:#}"));
                continue;
            }
        };
        let sums_url = match assets
            .iter()
            .find(|(n, _)| n == crate::paths::SHA256SUMS_FILE)
            .map(|(_, u)| u.clone())
        {
            Some(u) => u,
            None => {
                failures.push(format!(
                    "{product} {tag}: no {}",
                    crate::paths::SHA256SUMS_FILE
                ));
                continue;
            }
        };
        let sums_body = match downloader::fetch_text(&sums_url).await {
            Ok(b) => b,
            Err(e) => {
                failures.push(format!("{product} {tag}: sums fetch failed: {e:#}"));
                continue;
            }
        };
        for marker in [
            crate::paths::MARKER_MACOS_ARM64,
            crate::paths::MARKER_LINUX_X86_64,
            crate::paths::MARKER_LINUX_AARCH64,
        ] {
            let label = format!("{product} {tag} [{marker}]");
            let Some(asset) = assets
                .iter()
                .find(|(n, _)| n.contains(marker) && n.starts_with(prefix))
                .map(|(n, _)| n.clone())
            else {
                failures.push(format!("{label}: no release asset"));
                continue;
            };
            let Some(expected) = (if is_wimlib {
                crate::qemu_portable::pinned_wimlib_sha256(marker)
            } else {
                crate::qemu_portable::pinned_qemu_sha256(marker)
            }) else {
                failures.push(format!("{label}: no pinned hash for marker"));
                continue;
            };
            match parse_sums(&sums_body, &asset) {
                Some(h) if h.eq_ignore_ascii_case(expected) => ok(&label),
                Some(h) => failures.push(format!(
                    "{label}: sums hash {h} != pinned {expected} (stale pin? asset rotated?)"
                )),
                None => failures.push(format!(
                    "{label}: {asset} missing from {}",
                    crate::paths::SHA256SUMS_FILE
                )),
            }
        }
    }

    // virtio-win.iso: sidecar must be a valid 64-hex digest AND match the
    // code pin (closes the TOFU gap: a forged sidecar+ISO pair fails here).
    match downloader::virtio_release_info().await {
        Ok((url, sha)) => {
            ok(&format!("virtio-win.iso [{sha}] ({url})"));
        }
        Err(e) => failures.push(format!("virtio-win.iso: release lookup failed: {e:#}")),
    }
    // OpenSSH: no upstream sums — download both zips (~5 MB) to a tempdir
    // and hash against the arch-strict pins.
    let tmp = tempfile::tempdir().context("verify-pins tempdir")?;
    for arch in [crate::Architecture::Amd64, crate::Architecture::Arm64] {
        let label = format!("{} ({})", arch.openssh_zip_name(), arch.as_xml_str());
        let dest = tmp.path().join(arch.openssh_zip_name());
        match downloader::download(&downloader::openssh_zip_url(arch), &dest).await {
            Ok(actual) => {
                let _ = std::fs::remove_file(&dest);
                if actual.eq_ignore_ascii_case(arch.openssh_sha256()) {
                    ok(&label);
                } else {
                    failures.push(format!(
                        "{label}: downloaded hash {actual} != pinned {}",
                        arch.openssh_sha256()
                    ));
                }
            }
            Err(e) => failures.push(format!("{label}: download failed: {e:#}")),
        }
    }

    if failures.is_empty() {
        eprintln!("verify-pins: all pinned artifacts available and hash-matched");
        Ok(())
    } else {
        for f in &failures {
            eprintln!("pin FAIL: {f}");
        }
        anyhow::bail!("verify-pins: {} check(s) failed", failures.len())
    }
}

// ---------------------------------------------------------------------------
// VM helpers (shared by run / install)
// ---------------------------------------------------------------------------

/// Last meaningful line of the guest serial log (EDK2/Windows boot chatter).
/// Shown in the boot-wait ticker so a slow boot narrates itself instead of
/// printing bare elapsed seconds (observed live: 450 s of silence during a
/// recovery boot). Empty when the log has nothing yet.
///
/// Filters EDK2 memory-map/TPM noise (`ConvertPages`, `Tcg2`) that repeats
/// for minutes and says nothing — without this the ticker quotes firmware
/// trivia as status (observed live).
fn last_serial_line(serial_log: &str) -> String {
    /// Substrings marking firmware noise, never boot progress.
    const NOISE: &[&str] = &[
        "ConvertPages: failed to find range",
        "Tpm2SubmitCommand",
        "Tcg2",
        "Tpm2GetCapability",
    ];
    let text = std::fs::read_to_string(serial_log).unwrap_or_default();
    // Serial carries ANSI escapes and \r; take the last line with text
    // (reverse scan stops at the first hit instead of walking the log).
    fn stripped(line: &str, noise: &[&str]) -> Option<String> {
        // Strip common ANSI CSI sequences (best-effort, no regex dep).
        let mut s = String::with_capacity(line.len());
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                s.push(c);
            }
        }
        let s = s.trim().to_string();
        (!s.is_empty() && !noise.iter().any(|n| s.contains(n))).then_some(s)
    }
    let line = text
        .replace('\r', "\n")
        .lines()
        .rev()
        .filter_map(|l| stripped(l, NOISE))
        .next()
        .unwrap_or_default();
    // Cap length: some lines are progress bars several KB wide.
    if line.len() > 100 {
        format!("{}...", &line[..100])
    } else {
        line
    }
}
/// Fatal boot-failure patterns in the guest serial log (EDK2/Windows).
/// Patterns live in `templates/boot-fatal-patterns.txt` (one substring per
/// line), not in code — new dead screens are learned by editing a text
/// file, not shipping a release. Conservative by design: only signatures
/// that never appear in a healthy boot. A hit means VNC would show a dead
/// screen — fail fast with the evidence instead of polling SSH.
fn fatal_boot_patterns() -> Vec<String> {
    const TPL: &str = include_str!("../templates/boot-fatal-patterns.txt");
    TPL.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn check_boot_failure(serial_log: &str, shots_dir: &Path) -> Result<()> {
    let fatal = fatal_boot_patterns();
    let text = std::fs::read_to_string(serial_log).unwrap_or_default();
    // Only the tail matters (log persists across runs); keep the last 8 KB.
    let tail = if text.len() > 8192 {
        &text[text.len() - 8192..]
    } else {
        &text
    };
    for pat in &fatal {
        if tail.contains(pat) {
            anyhow::bail!(
                "guest failed to boot (serial shows {pat:?}); VNC {url} or latest screenshot in {shots} for the screen",
                url = vnc_url(PROVEN_VNC_DISPLAY).unwrap_or_else(|| PROVEN_VNC_DISPLAY.to_string()),
                shots = shots_dir.display(),
            );
        }
    }
    Ok(())
}

/// Stall sensor for serial-silent deaths (WinRE screens, EULA prompts —
/// anything GUI-only that the patterns file structurally cannot see).
/// Samples QMP blockstats: a booting guest does disk I/O constantly, so
/// completely frozen counters + no SSH banner means parked at an input
/// screen. Warns at [`timeouts::IDLE_WARN_SECS`], bails at
/// [`timeouts::IDLE_BAIL_SECS`] with the evidence.
/// Thread-safe by construction: all state lives in the struct.
struct BlockIdle {
    last_total: u64,
    last_change: std::time::Instant,
    warned: bool,
}

impl BlockIdle {
    fn new() -> Self {
        Self {
            last_total: 0,
            last_change: std::time::Instant::now(),
            warned: false,
        }
    }

    /// Sum all `rd_bytes`/`wr_bytes` in HMP `info blockstats` output.
    /// Best-effort parse (unparseable → None, never fail the wait on it).
    fn parse_total(text: &str) -> Option<u64> {
        let mut total = 0u64;
        let mut found = false;
        for key in ["rd_bytes=", "wr_bytes="] {
            for part in text.split(key).skip(1) {
                let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = digits.parse::<u64>() {
                    total = total.saturating_add(n);
                    found = true;
                }
            }
        }
        found.then_some(total)
    }

    /// Feed one sample. Returns a message when a threshold trips
    /// (warn-once, then bail); `Err` on bail so callers `?` straight out.
    async fn poll(&mut self, mon: &crate::ipc::Monitor) -> Result<()> {
        let text = mon.command("info blockstats").await.unwrap_or_default();
        let Some(total) = Self::parse_total(&text) else {
            return Ok(());
        };
        if total != self.last_total {
            self.last_total = total;
            self.last_change = std::time::Instant::now();
            self.warned = false;
            return Ok(());
        }
        let idle = self.last_change.elapsed().as_secs();
        if idle >= timeouts::IDLE_BAIL_SECS {
            anyhow::bail!(
                "guest appears stuck: no disk I/O for {idle}s and no SSH — likely parked at an input screen (recovery, EULA). VNC {url} or latest screenshot for the screen",
                url = vnc_url(PROVEN_VNC_DISPLAY).unwrap_or_else(|| PROVEN_VNC_DISPLAY.to_string()),
            );
        }
        if idle >= timeouts::IDLE_WARN_SECS && !self.warned {
            self.warned = true;
            eprintln!(
                "warning: no disk I/O for {idle}s and no SSH yet — guest may be stuck at an input screen"
            );
        }
        Ok(())
    }
}

/// Time-lapse dir name under the VM dir, and frame filename prefix.
/// Single source: [`screenshots_dir`] builds the path, `take_screenshot` names files.
pub const SCREENSHOTS_DIR: &str = "screenshots";
const SCREENSHOT_PREFIX: &str = "screenshot-";

fn screenshots_dir(vm_dir: &Path) -> PathBuf {
    vm_dir.join(SCREENSHOTS_DIR)
}

/// One screendump frame: `screendump` to PPM, converted in-crate to PNG
/// (README-displayable; tens to low hundreds of KB depending on screen
/// content vs 1.4 MB raw), PPM deleted. Frames land as
/// `<screenshots_dir>/screenshot-<epoch>.png`.
/// Best-effort (logs, never fails): the monitor may be busy mid-command
/// and a missed frame is not worth failing an install over.
/// Returns the PNG path on success so callers can announce it.
async fn take_screenshot(mon: &crate::ipc::Monitor, screenshots_dir: &Path) -> Option<PathBuf> {
    if std::fs::create_dir_all(screenshots_dir).is_err() {
        return None;
    }
    let stem = screenshots_dir.join(format!(
        "{SCREENSHOT_PREFIX}{}",
        crate::provenance::now_epoch()
    ));
    let ppm = stem.with_extension("ppm");
    let png_path = stem.with_extension("png");
    // Quote the path: HMP splits unquoted args on whitespace and the cache
    // path routinely contains spaces (observed live: screendump "succeeded"
    // but the file never appeared, failing the PNG step with ENOENT).
    let shot = mon
        .command(&format!("screendump \"{}\"", ppm.display()))
        .await;
    if let Err(e) = shot {
        eprintln!("warning: screendump failed: {e:#}");
        return None;
    }
    let result = std::fs::read(&ppm)
        .map_err(anyhow::Error::from)
        .and_then(|d| {
            let (w, h, rgb) = parse_ppm_p6(&d)?;
            write_png(&png_path, w, h, &rgb)
        });
    // Always clean up the raw PPM: success deletes it anyway, and a failed
    // conversion must not leak multi-MB dumps on every cadence tick.
    let _ = std::fs::remove_file(&ppm);
    match result {
        Ok(_) => Some(png_path),
        Err(e) => {
            eprintln!("warning: ppm->png failed ({e:#})");
            None
        }
    }
}

/// Parse binary P6 PPM → (width, height, RGB bytes). QEMU screendump
/// writes plain `P6\n<w> <h>\n255\n` + pixels; the parser also tolerates
/// comments and stray whitespace per the spec.
fn parse_ppm_p6(data: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut pos = 0;
    let mut token = || -> Result<String> {
        // Skip whitespace and `#` comments.
        loop {
            while pos < data.len() && data[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos < data.len() && data[pos] == b'#' {
                while pos < data.len() && data[pos] != b'\n' {
                    pos += 1;
                }
                continue;
            }
            break;
        }
        let start = pos;
        while pos < data.len() && !data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if start == pos {
            anyhow::bail!("truncated PPM header");
        }
        Ok(String::from_utf8_lossy(&data[start..pos]).into_owned())
    };
    if token()? != "P6" {
        anyhow::bail!("not a P6 PPM");
    }
    let w: u32 = token()?.parse().context("PPM width")?;
    let h: u32 = token()?.parse().context("PPM height")?;
    let max: u32 = token()?.parse().context("PPM maxval")?;
    if max != 255 {
        anyhow::bail!("unsupported PPM maxval {max} (want 255)");
    }
    // Exactly one whitespace byte separates header from pixels.
    if pos < data.len() && data[pos].is_ascii_whitespace() {
        pos += 1;
    }
    let expect = w as usize * h as usize * 3;
    if data.len() - pos < expect {
        anyhow::bail!(
            "truncated PPM pixels (have {}, want {expect})",
            data.len() - pos
        );
    }
    Ok((w, h, data[pos..pos + expect].to_vec()))
}

fn write_png(path: &Path, w: u32, h: u32, rgb: &[u8]) -> Result<()> {
    // Screenshots are flat UI: usually < 256 distinct colors, so exact
    // palette encoding (no dithering, no loss) beats RGB by ~7x
    // (measured: 42 KB RGB → 5.7 KB palette on a Setup screen).
    // Falls back to RGB when the frame is richer than 256 colors.
    use std::collections::HashMap;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut index_of: HashMap<[u8; 3], u8> = HashMap::new();
    let mut indexed = Vec::with_capacity(rgb.len() / 3);
    let mut fits = true;
    let (chunks, remainder) = rgb.as_chunks::<3>();
    if !remainder.is_empty() {
        fits = false;
    }
    for px in chunks {
        if !fits {
            break;
        }
        let key = [px[0], px[1], px[2]];
        let idx = match index_of.get(&key) {
            Some(&i) => i,
            None => {
                if palette.len() >= 256 {
                    fits = false;
                    break;
                }
                let i = palette.len() as u8;
                palette.push(key);
                index_of.insert(key, i);
                i
            }
        };
        indexed.push(idx);
    }
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Best);
    if fits {
        let flat: Vec<u8> = palette.iter().flatten().copied().collect();
        enc.set_color(png::ColorType::Indexed);
        enc.set_palette(flat);
        enc.write_header()?.write_image_data(&indexed)?;
    } else {
        enc.set_color(png::ColorType::Rgb);
        enc.write_header()?.write_image_data(rgb)?;
    }
    Ok(())
}

/// Background time-lapse: one frame every `every` until aborted.
/// Returns the join handle; callers `.abort()` it when the wait ends.
fn spawn_screenshots(
    mon: std::sync::Arc<crate::ipc::Monitor>,
    screenshots_dir: PathBuf,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // First frame immediately (proves the display is alive early),
        // then on cadence. Numbered, basename-only (the dir was announced
        // once above), so the log reads as progress toward the SSH answer.
        let mut n = 0u64;
        let announce = |n: u64, p: PathBuf| {
            eprintln!(
                "screenshot {n}: {}",
                p.file_name().unwrap_or_default().to_string_lossy()
            );
        };
        if let Some(p) = take_screenshot(&mon, &screenshots_dir).await {
            n += 1;
            announce(n, p);
        }
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // skip the immediate tick
        loop {
            ticker.tick().await;
            if let Some(p) = take_screenshot(&mon, &screenshots_dir).await {
                n += 1;
                announce(n, p);
            }
        }
    })
}

/// One banner read ([`timeouts::SSH_BANNER_CONNECT_BUDGET`] +
/// [`timeouts::SSH_BANNER_READ_BUDGET`]). `None` = no sshd right now.
async fn banner_once(port: u16) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let stream = tokio::time::timeout(
        timeouts::SSH_BANNER_CONNECT_BUDGET,
        tokio::net::TcpStream::connect((PROVEN_SSH_HOST, port)),
    )
    .await
    .ok()?
    .ok()?;
    let mut buf = [0u8; 256];
    let mut stream = stream;
    let n = tokio::time::timeout(timeouts::SSH_BANNER_READ_BUDGET, stream.read(&mut buf))
        .await
        .ok()?
        .ok()?;
    if n == 0 {
        return None;
    }
    let b = String::from_utf8_lossy(&buf[..n]);
    b.starts_with("SSH-").then(|| b.trim().to_string())
}

/// Guest readiness: banner phases for display, registry probe for truth.
/// The FIRST banner only means Setup reached a boot with SSH up — Windows
/// then typically reboots into further passes. Victory is declared when
/// the guest itself reports Setup done (no OOBE in progress, no reboot
/// pending), not after any fixed time window. Returns the banner (for the
/// `tested-isos.jsonl` record).
///
/// Always probes with the default password (the media is always built with
/// it — Setup rejects weak ones mid-install). A custom `password` is
/// rotated in after READY, before the ready line, via
/// `change_guest_password` (which verifies the new login). The custom
/// password never touches the answer file.
/// Shared observability for the install/boot waits: the one monitor
/// session plus the paths it watches. Bundled so the wait fns don't
/// sprawl argument lists (clippy limit is 7).
#[derive(Clone)]
struct GuestScope {
    mon: std::sync::Arc<crate::ipc::Monitor>,
    shots_dir: PathBuf,
    serial_log: String,
}

async fn wait_install_ready(
    port: u16,
    deadline: Duration,
    user: &str,
    password: &str,
    recovery: bool,
    auto_updates: bool,
    scope: &GuestScope,
) -> Result<String> {
    use crate::ssh::probe_setup_done;
    use crate::unattend::DEFAULT_PASSWORD;
    let start = std::time::Instant::now();
    // Phase 1: first banner.
    let mut idle = BlockIdle::new();
    let banner = loop {
        if start.elapsed() > deadline {
            anyhow::bail!(
                "timed out waiting for SSH banner on {PROVEN_SSH_HOST}:{port} after {} s (raise --install-timeout-secs if the host is just slow)",
                deadline.as_secs()
            );
        }
        if let Some(b) = banner_once(port).await {
            eprintln!("SSH banner: {b} (first contact — Setup still working)");
            break b;
        }
        // Dead-screen watchdog (same as run): fail fast with evidence.
        check_boot_failure(&scope.serial_log, &scope.shots_dir)?;
        // Stall watchdog: frozen disk I/O + no banner = parked.
        idle.poll(&scope.mon).await?;
        tokio::time::sleep(timeouts::SSH_POLL_INTERVAL).await;
    };
    // Phase 2: ask the guest until it reports done. Banner loss between
    // probes means a reboot (reported); probe errors mean sshd is
    // mid-restart (retried, not fatal). Each probe gets a hard budget: a
    // stalled handshake mid-reboot must surface, never hang the loop.
    // Separate counter for the overloaded state: banner stable but every
    // exec timing out means the guest is too saturated to cold-start
    // PowerShell (Setup still applying), not rebooting. Hammering it with
    // a probe every 10 s only stacks more powershell.exe on the pile, so
    // the cadence backs off while stalled.
    let mut reboots = 0u32;
    let mut was_up = true;
    let mut exec_stalls = 0u32;
    loop {
        if start.elapsed() > deadline {
            anyhow::bail!(
                "timed out waiting for guest readiness on {PROVEN_SSH_HOST}:{port} after {} s (raise --install-timeout-secs if the host is just slow)",
                deadline.as_secs()
            );
        }
        // The probe itself gets a hard budget: a stalled handshake
        // mid-reboot would otherwise hang the loop silently (observed as
        // "long polling, never noticing SSH is up"). Timeout = not yet.
        match tokio::time::timeout(
            timeouts::READINESS_PROBE_BUDGET,
            probe_setup_done(PROVEN_SSH_HOST, port, user, DEFAULT_PASSWORD),
        )
        .await
        {
            Ok(Ok(true)) => {
                if password != DEFAULT_PASSWORD {
                    eprintln!("rotating guest password ...");
                    crate::ssh::change_guest_password(
                        PROVEN_SSH_HOST,
                        port,
                        user,
                        DEFAULT_PASSWORD,
                        password,
                    )
                    .await?;
                }
                // Recovery policy (default off, docker-like): a damaged disk
                // fails fast instead of parking at "Diagnosing your PC".
                // Runs as the end-state credential (post-rotation).
                if let Err(e) = crate::ssh::set_recovery_enabled(
                    PROVEN_SSH_HOST,
                    port,
                    user,
                    password,
                    recovery,
                )
                .await
                {
                    eprintln!("warning: bcdedit failed ({e:#}); recovery state unchanged");
                }
                // Update policy (default off, docker-like): wuauserv runs as
                // SYSTEM and would otherwise reboot the box on Microsoft's
                // schedule with no logon. Disabled = deterministic; manual
                // updates over SSH still work anytime.
                if !auto_updates
                    && let Err(e) =
                        crate::ssh::set_auto_updates(PROVEN_SSH_HOST, port, user, password, false)
                            .await
                {
                    eprintln!("warning: wuauserv disable failed ({e:#})");
                }
                eprintln!(
                    "guest reports Setup complete across {reboots} reboot(s) — install complete"
                );
                return Ok(banner);
            }
            Ok(Ok(false)) => {
                exec_stalls = 0;
            }
            Ok(Err(e)) => {
                exec_stalls += 1;
                // Five straight exec timeouts with the banner up is not a
                // reboot (banner would drop) — it is saturation. Say so
                // once, then stay quiet while backing off below.
                if exec_stalls == 5 {
                    eprintln!(
                        "exec stalled 5x with SSH stable — guest is overloaded (Setup still applying), not rebooting; backing off probes ..."
                    );
                } else if exec_stalls < 5 {
                    eprintln!("probe failed (sshd mid-reboot?): {e:#}");
                }
            }
            Err(_) => {
                exec_stalls += 1;
                if exec_stalls == 5 {
                    eprintln!(
                        "probe timed out 5x with SSH stable — guest is overloaded, backing off probes ..."
                    );
                } else if exec_stalls < 5 {
                    eprintln!("probe timed out (sshd busy?) — retrying ...");
                }
            }
        }
        let up = banner_once(port).await.is_some();
        match (was_up, up) {
            (true, false) => {
                eprintln!(
                    "SSH lost — reboot #{} in progress, waiting for return ...",
                    reboots + 1
                );
            }
            (false, true) => {
                reboots += 1;
                exec_stalls = 0;
                eprintln!("SSH back (reboot #{reboots} finished) — resuming probes ...");
            }
            _ => {}
        }
        was_up = up;
        // Back off while stalled (10 s → up to 60 s): each probe spawns a
        // PowerShell cold start, and hammering a saturated guest buries it.
        let backoff = 1 + (exec_stalls / 5).min(5);
        tokio::time::sleep(timeouts::READINESS_PROBE_INTERVAL * backoff).await;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn install_fixture(memory_mb: u32, cpus: u32) -> (Vec<String>, Vec<String>) {
        let dir = std::env::temp_dir();
        let code = dir.join("code.fd");
        let vars = dir.join("vars.fd");
        let usb = dir.join("usb.dmg");
        let spec = InstallQemuArgv {
            machine: "virt",
            accel: "tcg",
            cpu: "max",
            memory_mb,
            cpus,
            display: Display::Vnc("vnc=127.0.0.1:10,password=on".to_string()),
            code_tpl: &code,
            vars_fd: &vars,
            serial_log: "serial.log",
            mon_sock: "mon.sock",
            qmp_sock: "qmp.sock",
            usb_image: &usb,
            target: "disk.qcow2",
            arch: Architecture::Arm64,
        };
        (
            install_qemu_argv(&spec),
            qemu_img_create_args("disk.qcow2", 128),
        )
    }

    #[test]
    fn install_sizing_reaches_qemu_argv() {
        // The flags a user passes must be the bytes QEMU gets: no QEMU
        // needed, just the pure argv constructors.
        let (argv, img) = install_fixture(8192, 8);
        let at = |flag: &str| argv[argv.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(at("-m"), "8192");
        assert_eq!(at("-smp"), "8");
        assert_eq!(img.last().unwrap(), "128G");
        // Defaults stay proven-shaped.
        let (argv, img) = install_fixture(4096, 4);
        let at = |flag: &str| argv[argv.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(at("-m"), "4096");
        assert_eq!(at("-smp"), "4");
        assert_eq!(at("-display"), "vnc=127.0.0.1:10,password=on");
        let img64 = qemu_img_create_args("disk.qcow2", 64);
        assert_eq!(img64.last().unwrap(), "64G");
        assert_eq!(img[..5], img64[..5]);
    }

    #[test]
    fn install_cocoa_display_passes_through() {
        // Non-VNC displays reach QEMU verbatim (the VNC password step is
        // guarded separately); cocoa installs show a window instead.
        let dir = std::env::temp_dir();
        let code = dir.join("code.fd");
        let vars = dir.join("vars.fd");
        let usb = dir.join("usb.dmg");
        let spec = InstallQemuArgv {
            machine: "virt",
            accel: "tcg",
            cpu: "max",
            memory_mb: 4096,
            cpus: 4,
            display: Display::Cocoa,
            code_tpl: &code,
            vars_fd: &vars,
            serial_log: "serial.log",
            mon_sock: "mon.sock",
            qmp_sock: "qmp.sock",
            usb_image: &usb,
            target: "disk.qcow2",
            arch: Architecture::Arm64,
        };
        let argv = install_qemu_argv(&spec);
        let at = |flag: &str| argv[argv.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(at("-display"), "cocoa");
    }

    #[test]
    fn ppm_p6_roundtrip() {
        // 2x1 red/green pixels with a comment in the header.
        let mut raw = b"P6\n# qemu screendump\n2 1\n255\n".to_vec();
        raw.extend_from_slice(&[255, 0, 0, 0, 255, 0]);
        let (w, h, rgb) = parse_ppm_p6(&raw).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgb, vec![255, 0, 0, 0, 255, 0]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.png");
        write_png(&p, w, h, &rgb).unwrap();
        let back = std::fs::read(&p).unwrap();
        assert_eq!(&back[1..4], b"PNG", "not a PNG");
        // 2 colors → indexed palette (IHDR color type 3), exact pixels.
        assert_eq!(
            back[25], 3,
            "expected palette PNG, got color type {}",
            back[25]
        );
        assert!(
            back.len() < 100,
            "2px palette PNG should be tiny, got {}",
            back.len()
        );
    }

    #[test]
    fn ppm_rejects_bad_magic_and_truncation() {
        assert!(parse_ppm_p6(b"P5\n2 1\n255\n\x00\x00").is_err());
        assert!(parse_ppm_p6(b"P6\n2 1\n255\n\xff").is_err());
    }

    #[test]
    fn blockstats_parse_totals() {
        let sample = "drive-tgt: rd_bytes=1048576 wr_bytes=524288 rd_operations=12\n\
                      drive-usb: rd_bytes=100 wr_bytes=0\n";
        assert_eq!(BlockIdle::parse_total(sample), Some(1048576 + 524288 + 100));
        assert_eq!(BlockIdle::parse_total(""), None);
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            expand_tilde("~/x.iso".into()),
            PathBuf::from(home.clone()).join("x.iso")
        );
        assert_eq!(expand_tilde("~".into()), PathBuf::from(home));
        // Untouched: relative, absolute, other users.
        assert_eq!(expand_tilde("rel/x.iso".into()), PathBuf::from("rel/x.iso"));
        assert_eq!(
            expand_tilde("/abs/x.iso".into()),
            PathBuf::from("/abs/x.iso")
        );
        assert_eq!(
            expand_tilde("~other/x.iso".into()),
            PathBuf::from("~other/x.iso")
        );
    }

    #[test]
    fn report_renders_table() {
        use crate::provenance::{DependencyPins, GuestParams, HostSpecs, IsoIdentity, Provenance};
        let record = crate::provenance::TestRecord {
            tested_at_epoch: 1_758_600_000,
            outcome: "ssh-banner".to_string(),
            ssh_banner: Some("SSH-2.0-test".to_string()),
            provenance: Provenance {
                builder_version: "0.1.0".to_string(),
                build_commit: "abcdef1234567".to_string(),
                build_dirty: true,
                built_at_epoch: 1_758_600_000,
                source_iso: IsoIdentity {
                    file_name: "Win11.iso".to_string(),
                    size_bytes: 1,
                    sha256: "x".to_string(),
                    pvd_creation: None,
                    wim_images: vec![],
                },
                host: HostSpecs {
                    os: "macos".to_string(),
                    os_version: "26.0".to_string(),
                    arch: "aarch64".to_string(),
                },
                deps: DependencyPins {
                    qemu_tag: "qemu-v11.1.1".to_string(),
                    wimlib_tag: "wimlib-v1.14.5-27".to_string(),
                    virtio_iso_name: "virtio-win.iso".to_string(),
                    virtio_sha256: None,
                    openssh_zip_name: "OpenSSH-ARM64.zip".to_string(),
                    openssh_release_tag: "t".to_string(),
                },
                guest: GuestParams {
                    arch: "arm64".to_string(),
                    username: "u".to_string(),
                    locale: "en-US".to_string(),
                    accel: "tcg".to_string(),
                },
            },
        };
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("tested-isos.jsonl");
        std::fs::write(&log, serde_json::to_string(&record).unwrap() + "\n").unwrap();
        let table = render_report(&log).unwrap();
        assert!(table.contains("| Date | ISO |"), "{table}");
        assert!(table.contains("Win11.iso"), "{table}");
        assert!(table.contains("abcdef1*"), "{table}");
        // No virtio hash in the fixture → falls back to the ISO name.
        assert!(table.contains("virtio-win.iso"), "{table}");
        assert_eq!(epoch_to_date(1_758_600_000), "2025-09-23");
    }
}
