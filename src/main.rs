//! Thin CLI: clap arg shapes + dispatch to [`windows_headless::commands`].
//! All business logic (cache dirs, QEMU argv, media build) lives in the
//! library so it is testable and reusable from other frontends.
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
#[cfg(feature = "downloader")]
use windows_headless::commands::{
    BuildArgs, FetchArgs, cmd_build, cmd_fetch, cmd_fetch_qemu, cmd_fetch_wimlib, cmd_setup,
    cmd_verify_pins,
};
use windows_headless::{
    Architecture, PROVEN_SSH_PORT, PROVEN_VNC_DISPLAY,
    commands::{
        BuildMediaArgs, GenArgs, GuestArgs, GuestOp, ReportArgs, RunArgs, cmd_build_media, cmd_gen,
        cmd_guest, cmd_install, cmd_overlay, cmd_report, cmd_run, cmd_stop,
    },
};

#[derive(Parser)]
#[command(name = "windows-headless", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render autounattend.xml to stdout or a vvfat dir.
    #[command(hide = true)]
    Gen {
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_USERNAME)]
        username: String,
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_PASSWORD)]
        password: String,
        #[arg(long, default_value = "en-US")]
        locale: String,
        #[arg(long)]
        autologon: bool,
        #[arg(long)]
        unattend_dir: Option<PathBuf>,
        /// Guest arch (default: host arch). Selects the OpenSSH zip names.
        #[arg(long)]
        arch: Option<Architecture>,
    },
    /// Run: boot the disk, attach if live, install first if missing.
    /// Boots `<vm-dir>/disk.qcow2` (or --disk); attaches to the shell when
    /// already running; installs when the disk is absent (needs cached
    /// media or --win-iso). Never rebuilds: to reinstall a live disk,
    /// delete it (or use hidden `install`), then `run`.
    Run {
        /// Disk image to boot (default: `<vm-dir>/disk.qcow2`).
        #[arg(long)]
        disk: Option<String>,
        #[arg(long, default_value_t = PROVEN_SSH_PORT)]
        ssh_port: u16,
        /// `-display` spec: `none`, `cocoa`, `gtk`, `vnc=127.0.0.1:0`.
        #[arg(long, default_value = PROVEN_VNC_DISPLAY)]
        display: String,
        /// Guest RAM: Docker syntax (`512m`, `4g`; bare digits are MB).
        /// Minimum 2g; Windows 11 setup needs 4g.
        #[arg(long, default_value = "4g", value_parser = windows_headless::sizes::parse_memory_mb)]
        memory: u32,
        /// Guest vCPUs, Docker-style (`--cpus`; `--smp` still works).
        #[arg(long, default_value_t = 4, visible_alias = "smp")]
        cpus: u32,
        /// New-disk size on install: Docker syntax (`64g`; bare digits are
        /// GB). Minimum 32g; ignored when booting an existing disk.
        #[arg(long, default_value = "64g", value_parser = windows_headless::sizes::parse_disk_gb)]
        disk_size: u32,
        /// VM dir for the pidfile (default: project cache `vms/win11`).
        #[arg(long)]
        vm_dir: Option<PathBuf>,
        /// Portable tree override (default: newest cached tree from `fetch-qemu`).
        #[arg(long)]
        qemu_dir: Option<PathBuf>,
        /// Guest username (for `--shell` login).
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_USERNAME)]
        username: String,
        /// Guest password (for `--shell` login).
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_PASSWORD)]
        password: String,
        /// Drop into an interactive guest shell after ready (built-in SSH
        /// client, no host `ssh` needed). Ctrl-D exits back to the CLI.
        /// Default: on when stdin is a terminal, off when piped (CI-safe).
        /// Pass `--shell` / `--shell=false` to force either way.
        #[arg(long)]
        shell: Option<bool>,
        /// Windows ISO: when the disk is missing, install it first
        /// (setup + build-media + install) instead of bailing.
        #[arg(long)]
        win_iso: Option<PathBuf>,
        /// Budget for the unattended install's SSH wait, in seconds
        /// (reached only when `--win-iso` triggers an install).
        #[arg(long, default_value_t = windows_headless::timeouts::INSTALL_SSH_DEADLINE_SECS)]
        install_timeout_secs: u64,
    },
    /// Proven install: USB-HDD media + NVMe target, unattended answer
    /// file, ends at SSH with the guest reporting Setup complete.
    /// USB media must already exist (`build-media` builds it).
    #[command(hide = true)]
    Install {
        /// USB installer image (default: project cache `media/winusb.dmg`).
        #[arg(long)]
        usb_image: Option<PathBuf>,
        /// Windows ISO: when the USB image is missing, run the full chain
        /// (`setup` + `build-media --win-iso`) instead of bailing.
        /// Absent + missing image = error naming both options.
        #[arg(long)]
        win_iso: Option<PathBuf>,
        /// Screendump cadence in seconds (0 = disabled). Screenshots land in
        /// `<vm-dir>/screenshots/` — a time-lapse of the install.
        #[arg(long, default_value_t = windows_headless::timeouts::SCREENSHOTS_EVERY_SECS)]
        screenshots_every_secs: u64,
        /// Guest username (must match the media's answer file; used for
        /// the readiness probe login and the ready line).
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_USERNAME)]
        username: String,
        /// End-state guest password. The media is always built with the
        /// default (Setup-safe); a different value here is rotated in via
        /// `net user` after the guest reports ready, then verified.
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_PASSWORD)]
        password: String,
        /// Drop into an interactive guest shell after ready (built-in SSH
        /// client, no host `ssh` needed). Ctrl-D exits back to the CLI.
        /// Default: on when stdin is a terminal, off when piped (CI-safe).
        /// Pass `--shell` / `--shell=false` to force either way.
        #[arg(long)]
        shell: Option<bool>,
        /// Keep Windows Automatic Repair ("Diagnosing your PC") enabled.
        /// Default off: faster boots. With recovery off, a truly damaged
        /// virtual disk fails fast instead of repairing. Snapshots are
        /// the safety net, not WinRE.
        #[arg(long, default_value_t = false)]
        recovery: bool,
        /// Leave Windows Update automatic (download + install + reboot on
        /// Microsoft's schedule, no logon needed). Default off: wuauserv
        /// is disabled at install — no surprise reboots, docker-like.
        /// Manual updates still work anytime over SSH.
        #[arg(long, default_value_t = false)]
        auto_updates: bool,
        /// Budget for the unattended install's SSH wait, in seconds.
        /// Generous on purpose: a slow host applies the image for hours,
        /// and a *parked* guest is caught separately by the no-I/O stall
        /// watchdog — this only bounds an install that is still moving.
        #[arg(long, default_value_t = windows_headless::timeouts::INSTALL_SSH_DEADLINE_SECS)]
        install_timeout_secs: u64,
        /// Guest RAM: Docker syntax (`512m`, `4g`; bare digits are MB).
        /// Minimum 2g; Windows 11 setup needs 4g.
        #[arg(long, default_value = "4g", value_parser = windows_headless::sizes::parse_memory_mb)]
        memory: u32,
        /// Guest vCPUs, Docker-style (`--cpus`; `--smp` still works).
        #[arg(long, default_value_t = 4, visible_alias = "smp")]
        cpus: u32,
        /// New-disk size: Docker syntax (`64g`; bare digits are GB).
        /// Minimum 32g.
        #[arg(long, default_value = "64g", value_parser = windows_headless::sizes::parse_disk_gb)]
        disk_size: u32,
    },
    /// Build the USB installer image from the Windows ISO (pure Rust +
    /// vendored `wimlib-imagex`; no host tools). Output defaults to the
    /// media cache (`install` consumes it).
    #[command(hide = true)]
    BuildMedia {
        /// Windows ISO path.
        #[arg(long)]
        win_iso: PathBuf,
        /// Guest arch (default: host arch — HVF/KVM require host == guest).
        /// Selects the OpenSSH zip, NetKVM driver dir, and answer-file arch.
        #[arg(long)]
        arch: Option<Architecture>,
        /// virtio-win ISO (NetKVM drivers extracted in-crate, pure Rust).
        /// Default: `<iso-cache>/virtio-win.iso` (`setup` fetches it).
        #[arg(long)]
        virtio_iso: Option<PathBuf>,
        /// Pre-extracted driver tree override (dev-time; relative paths
        /// preserved under `drivers/`). Wins over `--virtio-iso`.
        #[arg(long)]
        drivers_dir: Option<PathBuf>,
        /// OpenSSH zip path (offline SSH install; Win64 vs ARM64 by --arch).
        /// Default: `<iso-cache>/<name>` (`setup` fetches it).
        #[arg(long)]
        openssh_zip: Option<PathBuf>,
        /// Output image (default: project cache `media/winusb.dmg`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Rebuild even when the output image + sidecar already exist.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// `wimlib-imagex` binary (default: `$PATH` lookup; the vendored
        /// release asset once published).
        #[arg(long)]
        wimlib_bin: Option<PathBuf>,
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_USERNAME)]
        username: String,
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_PASSWORD)]
        password: String,
        #[arg(long, default_value = "en-US")]
        locale: String,
    },
    /// Offline overlay: qemu-img create -b base overlay.
    #[command(hide = true)]
    Overlay {
        overlay: PathBuf,
        /// Base image (default: `<vm-dir>/disk.qcow2` — the disk `run` boots).
        base: Option<PathBuf>,
        /// Portable tree root (extracted `qemu-portable` dir); else PATH.
        #[arg(long)]
        qemu_dir: Option<PathBuf>,
    },
    /// Graceful shutdown via QMP + pidfile cleanup (no orphans).
    #[command(hide = true)]
    Stop {
        /// VM dir holding `qemu.pid` (default: project cache `vms/win11`).
        #[arg(long)]
        vm_dir: Option<PathBuf>,
        /// QMP endpoint of the running VM (default: next to the disk in
        /// the VM dir — never /tmp).
        #[arg(long)]
        qmp_sock: Option<String>,
    },
    /// Run guest verbs against an already-booted machine (no install,
    /// no boot — just SSH). Connection flags mirror `run`/`install`.
    #[command(hide = true)]
    Guest {
        #[command(subcommand)]
        op: GuestOp,
        /// SSH port (default: proven 2232).
        #[arg(long, default_value_t = PROVEN_SSH_PORT)]
        ssh_port: u16,
        /// Guest username.
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_USERNAME)]
        username: String,
        /// Guest password.
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_PASSWORD)]
        password: String,
    },
    /// Render `tested-isos.jsonl` as a markdown table (stdout, or spliced
    /// into a README between `<!-- TESTED-BEGIN/END -->` markers).
    #[command(hide = true)]
    Report {
        /// Log path (default: `./tested-isos.jsonl`).
        #[arg(long, default_value = windows_headless::provenance::TestRecord::LOG)]
        log: PathBuf,
        /// README to update in place (absent: print table to stdout).
        #[arg(long)]
        readme: Option<PathBuf>,
    },
    /// One-shot host setup: portable QEMU + portable wimlib + virtio-win
    /// ISO + OpenSSH zip, all into the project cache. After this,
    /// `build-media` needs only `--win-iso` (the user-supplied Windows ISO).
    /// Re-runs are cheap (everything already-cached is skipped).
    #[cfg(feature = "downloader")]
    #[command(hide = true)]
    Setup {
        /// virtio-win.iso URL override (default: vendored release asset).
        #[arg(long)]
        virtio_iso_url: Option<String>,
        /// Guest arch (default: host arch — HVF/KVM require host == guest).
        /// Selects the OpenSSH zip (Win64 vs ARM64) and NetKVM driver dir.
        #[arg(long)]
        arch: Option<Architecture>,
    },
    /// Build the USB installer image: `setup` (fetch deps) + `build-media`.
    /// One command, one artifact. Only `--win-iso` is required (your Windows
    /// ISO); drivers, OpenSSH, wimlib, and output all default to the cache.
    #[cfg(feature = "downloader")]
    Build {
        /// Windows ISO path.
        #[arg(long)]
        win_iso: PathBuf,
        /// Guest arch (default: host arch — HVF/KVM require host == guest).
        #[arg(long)]
        arch: Option<Architecture>,
        /// Output image (default: project cache `media/winusb.dmg`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Rebuild even when the output image + sidecar already exist.
        #[arg(long, default_value_t = false)]
        force: bool,
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_USERNAME)]
        username: String,
        #[arg(long, default_value = windows_headless::unattend::DEFAULT_PASSWORD)]
        password: String,
        #[arg(long, default_value = "en-US")]
        locale: String,
    },
    /// Download a portable QEMU release into the project cache.
    #[cfg(feature = "downloader")]
    #[command(hide = true)]
    FetchQemu {
        /// Release tag (default: pinned, see pins::QEMU_PORTABLE_TAG).
        /// Pass an explicit tag only to test a new build.
        #[arg(long)]
        qemu_tag: Option<String>,
        /// Cache base dir (default: `<project>/.cache/qemu-portable`).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Proceed even when the release ships no SHA256SUMS.txt (or no
        /// entry for our asset). Fails closed by default; also honored via
        /// $WINDOWS_HEADLESS_ALLOW_UNVERIFIED. Testing new builds only.
        #[arg(long, default_value_t = false)]
        allow_unverified: bool,
    },
    /// Download ISOs into the cache dir, skipping what is already present.
    /// virtio-win.iso comes from our own releases (no Fedora bot-wall);
    /// OpenSSH-ARM64.zip from Microsoft. If Fedora's bot-wall blocks
    /// scripted downloads, fetch the file in a real browser and save it
    /// as `<dir>/virtio-win.iso` (or pass any mirror via --virtio-iso-url)
    /// — `fetch` validates and keeps it.
    #[cfg(feature = "downloader")]
    #[command(hide = true)]
    Fetch {
        /// Windows ISO URL (fresh, time-limited link from a browser).
        #[arg(long)]
        win_iso_url: Option<String>,
        /// Expected SHA-256 for the Windows ISO (default: none — URLs are
        /// time-limited/rotating, so TLS + ISO magic only; also read from
        /// $WIN11_EVAL_ISO_SHA256 when the flag is absent).
        #[arg(long)]
        win_iso_sha256: Option<String>,
        /// VirtIO ISO URL override (default: vendored release asset).
        #[arg(long)]
        virtio_iso_url: Option<String>,
        /// Cache dir (default: <project>/.cache/iso or $WINDOWS_HEADLESS_ISO_DIR).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Expected SHA-256 for virtio-win.iso; re-downloads on mismatch
        /// (default: the release `.sha256` sidecar).
        #[arg(long)]
        virtio_sha256: Option<String>,
        /// Also fetch the OpenSSH zip (Microsoft release asset, per --arch).
        #[arg(long, default_value_t = true)]
        openssh: bool,
        /// Guest arch (default: host arch). Selects the OpenSSH zip.
        #[arg(long)]
        arch: Option<Architecture>,
        /// Re-download even valid cache entries (needed when a time-limited
        /// Windows ISO URL now points at a newer build than the cached file).
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Verify every pinned external artifact (availability + hash) WITHOUT
    /// downloading archives or installing anything. CI-friendly: KBs of
    /// release metadata, plus two ~5 MB OpenSSH zips. Covers ALL platforms
    /// (macos-arm64, linux-x86_64, linux-aarch64) from any single runner —
    /// no per-OS matrix needed. Fails non-zero listing every mismatch so one
    /// run reports everything.
    #[cfg(feature = "downloader")]
    #[command(hide = true)]
    VerifyPins {},
    /// Download a portable wimlib release into the project cache.
    #[cfg(feature = "downloader")]
    #[command(hide = true)]
    FetchWimlib {
        /// Release tag (default: pinned, see pins::WIMLIB_PORTABLE_TAG).
        /// Pass an explicit tag only to test a new build.
        #[arg(long)]
        wimlib_tag: Option<String>,
        /// Cache base dir (default: `<project>/.cache/wimlib-portable`).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Proceed even when the release ships no SHA256SUMS.txt (or no
        /// entry for our asset). Fails closed by default; also honored via
        /// $WINDOWS_HEADLESS_ALLOW_UNVERIFIED. Testing new builds only.
        #[arg(long, default_value_t = false)]
        allow_unverified: bool,
    },
}

/// Explicit runtime (not `#[tokio::main]`): `shutdown_timeout` with
/// [`windows_headless::timeouts::RUNTIME_EXIT_GRACE`] abandons leaked
/// blocking threads at exit instead of hanging on them. The leak:
/// every shell-loop iteration drops a pending `stdin.read()` when guest
/// output wins the select race, parking a blocking-pool thread in `read()`
/// forever (observed live: "stopped" prints, process never exits, Ctrl-C
/// mashing goes nowhere). One second of grace, then out.
fn main() -> Result<()> {
    // Tracing → stderr (never stdout: stdout carries no progress protocol,
    // while stderr hosts carriage-return progress bars that stdout logs
    // would tear). INFO by default so fail-closed gates announce themselves;
    // an explicitly-set RUST_LOG wins outright (a blanket INFO directive
    // would otherwise shadow it — directives don't override each other).
    let filter = match std::env::var("RUST_LOG").ok().filter(|v| !v.is_empty()) {
        Some(_) => tracing_subscriber::EnvFilter::from_default_env(),
        None => tracing_subscriber::EnvFilter::from_default_env()
            .add_directive(tracing::Level::INFO.into()),
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let out = rt.block_on(async_main());
    rt.shutdown_timeout(windows_headless::timeouts::RUNTIME_EXIT_GRACE);
    out
}

async fn async_main() -> Result<()> {
    use std::io::IsTerminal;
    /// Shell default: interactive terminal → shell, piped/CI → supervise.
    /// Explicit `--shell` / `--shell=false` always wins.
    fn want_shell(flag: Option<bool>) -> bool {
        flag.unwrap_or_else(|| std::io::stdin().is_terminal())
    }
    let cli = Cli::parse();
    match cli.cmd {
        #[cfg(feature = "downloader")]
        Cmd::Build {
            win_iso,
            arch,
            out,
            force,
            username,
            password,
            locale,
        } => {
            cmd_build(BuildArgs {
                win_iso,
                arch,
                out,
                force,
                username,
                password,
                locale,
            })
            .await
        }
        Cmd::Gen {
            username,
            password,
            locale,
            autologon,
            unattend_dir,
            arch,
        } => cmd_gen(GenArgs {
            username,
            password,
            locale,
            autologon,
            unattend_dir,
            arch,
        }),
        Cmd::BuildMedia {
            win_iso,
            arch,
            virtio_iso,
            drivers_dir,
            openssh_zip,
            out,
            force,
            wimlib_bin,
            username,
            password,
            locale,
        } => {
            cmd_build_media(BuildMediaArgs {
                win_iso,
                arch,
                virtio_iso,
                drivers_dir,
                openssh_zip,
                out,
                wimlib_bin,
                username,
                password,
                locale,
                force,
            })
            .await
        }
        Cmd::Run {
            disk,
            ssh_port,
            display,
            memory,
            cpus,
            disk_size,
            vm_dir,
            qemu_dir,
            username,
            password,
            shell,
            win_iso,
            install_timeout_secs,
        } => {
            cmd_run(RunArgs {
                disk,
                ssh_port,
                display,
                memory,
                cpus,
                disk_size,
                vm_dir,
                qemu_dir,
                username,
                password,
                shell: want_shell(shell),
                win_iso,
                install_timeout_secs,
            })
            .await
        }
        Cmd::Install {
            usb_image,
            win_iso,
            screenshots_every_secs,
            username,
            password,
            shell,
            recovery,
            auto_updates,
            install_timeout_secs,
            memory,
            cpus,
            disk_size,
        } => {
            cmd_install(
                usb_image,
                win_iso,
                screenshots_every_secs,
                &username,
                &password,
                want_shell(shell),
                recovery,
                auto_updates,
                install_timeout_secs,
                memory,
                cpus,
                disk_size,
            )
            .await
        }
        Cmd::Overlay {
            base,
            overlay,
            qemu_dir,
        } => cmd_overlay(base, overlay, qemu_dir).await,
        Cmd::Stop { vm_dir, qmp_sock } => cmd_stop(vm_dir, qmp_sock).await,
        Cmd::Guest {
            op,
            ssh_port,
            username,
            password,
        } => {
            cmd_guest(GuestArgs {
                op,
                ssh_port,
                username,
                password,
            })
            .await
        }
        Cmd::Report { log, readme } => cmd_report(ReportArgs { log, readme }),
        #[cfg(feature = "downloader")]
        Cmd::Setup {
            virtio_iso_url,
            arch,
        } => cmd_setup(virtio_iso_url, arch).await,
        #[cfg(feature = "downloader")]
        Cmd::FetchQemu {
            qemu_tag,
            dir,
            allow_unverified,
        } => cmd_fetch_qemu(qemu_tag, dir, allow_unverified).await,
        #[cfg(feature = "downloader")]
        Cmd::Fetch {
            win_iso_url,
            win_iso_sha256,
            virtio_iso_url,
            dir,
            virtio_sha256,
            openssh,
            arch,
            force,
        } => {
            cmd_fetch(FetchArgs {
                win_iso_url,
                win_iso_sha256,
                virtio_iso_url,
                dir,
                virtio_sha256,
                openssh,
                arch,
                force,
            })
            .await
        }
        #[cfg(feature = "downloader")]
        Cmd::VerifyPins {} => cmd_verify_pins().await,
        #[cfg(feature = "downloader")]
        Cmd::FetchWimlib {
            wimlib_tag,
            dir,
            allow_unverified,
        } => cmd_fetch_wimlib(wimlib_tag, dir, allow_unverified).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn run_accepts_docker_sizing() {
        // --memory in Docker syntax, --cpus primary, --disk-size in GB syntax.
        let cli = Cli::try_parse_from([
            "wh",
            "run",
            "--memory",
            "8g",
            "--cpus",
            "8",
            "--disk-size",
            "128g",
        ])
        .unwrap();
        let Cmd::Run {
            memory,
            cpus,
            disk_size,
            ..
        } = cli.cmd
        else {
            panic!("expected run");
        };
        assert_eq!(memory, 8192);
        assert_eq!(cpus, 8);
        assert_eq!(disk_size, 128);
    }

    #[test]
    fn run_smp_alias_and_defaults() {
        // Legacy --smp still maps to cpus; bare --memory digits are rejected.
        let cli = Cli::try_parse_from(["wh", "run", "--smp", "2"]).unwrap();
        let Cmd::Run {
            memory,
            cpus,
            disk_size,
            ..
        } = cli.cmd
        else {
            panic!("expected run");
        };
        assert_eq!((memory, cpus, disk_size), (4096, 2, 64));
        // Bare digits keep the old integer meaning (MB for memory, GB disk);
        // toy sizes hit the floors.
        let cli = Cli::try_parse_from(["wh", "run", "--memory", "4096"]).unwrap();
        let Cmd::Run { memory, .. } = cli.cmd else {
            panic!("expected run");
        };
        assert_eq!(memory, 4096);
        assert!(Cli::try_parse_from(["wh", "run", "--memory", "512m"]).is_err());
        assert!(Cli::try_parse_from(["wh", "run", "--disk-size", "8g"]).is_err());
        Cli::command().debug_assert();
    }

    #[test]
    fn overlay_takes_bare_overlay_first() {
        // `overlay` (required) precedes `base` (optional): a single
        // positional lands on overlay instead of erroring on a missing one.
        let cli = Cli::try_parse_from(["wh", "overlay", "delta.qcow2"]).unwrap();
        let Cmd::Overlay { base, overlay, .. } = cli.cmd else {
            panic!("expected overlay");
        };
        assert_eq!(overlay, PathBuf::from("delta.qcow2"));
        assert_eq!(base, None);
    }
}
