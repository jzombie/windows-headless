use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::{Child, Command};

use crate::qmp::QmpClient;
use crate::ssh;
use crate::storage::DiskManager;
use crate::timeouts;
use crate::unattend::{Architecture, VmConfig, auto_detect_platform};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsVersion {
    Win11,
}

/// QEMU `-display` backend: the closed set this tool supports. Anything
/// else QEMU accepts is rejected at the CLI so a typo fails fast instead
/// of dying deep in QEMU startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Display {
    None,
    Cocoa,
    Gtk,
    Vnc(String),
}

impl Display {
    /// Parse a `--display` value: `none`, `cocoa`, `gtk`, or `vnc=<spec>`.
    /// Clap `value_parser` shape: `Err(String)` renders as a clean CLI error.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "none" => Ok(Display::None),
            "cocoa" => Ok(Display::Cocoa),
            "gtk" => Ok(Display::Gtk),
            s if s.starts_with("vnc=") => Ok(Display::Vnc(s.to_string())),
            _ => Err(format!(
                "unsupported --display '{s}' (want none|cocoa|gtk|vnc=<spec>)"
            )),
        }
    }

    /// The `-display` argv value.
    pub fn as_arg(&self) -> String {
        match self {
            Display::None => "none".to_string(),
            Display::Cocoa => "cocoa".to_string(),
            Display::Gtk => "gtk".to_string(),
            Display::Vnc(spec) => spec.clone(),
        }
    }

    pub fn is_vnc(&self) -> bool {
        matches!(self, Display::Vnc(_))
    }
}

impl std::fmt::Display for Display {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_arg())
    }
}

/// Proven topology: the exact QEMU configuration `install`
/// demonstrated live (SSH banner, zero clicks). `run` boots this same
/// configuration minus the USB installer stick. Every default below and
/// every CLI default mirrors these — no parallel hardcodings.
pub const PROVEN_SSH_PORT: u16 = 2232;
/// Loopback bind for SSH forwarding and VNC. Single source of truth:
/// every connect, message, and fallback below references this — never a
/// literal (the VNC `-display` spec string is the one exception: QEMU
/// syntax, kept in sync by the `vnc_display_matches_host` test).
pub const PROVEN_SSH_HOST: &str = "127.0.0.1";
/// Endpoint locations moved to `ipc` (per-VM-dir, never `/tmp`): file
/// names are [`crate::ipc::MON_SOCK_NAME`] etc., resolved with
/// [`crate::ipc::vm_endpoint`].
// QEMU VNC `host:DISPLAY` takes a display NUMBER, not a TCP port:
// TCP port = 5900 + display, so :10 listens on 5910 (matches vnc_url()
// below and every netstat we ever captured).
pub const PROVEN_VNC_DISPLAY: &str = "vnc=127.0.0.1:10,password=on";
/// Classic VNC caps passwords at 8 chars.
pub const PROVEN_VNC_PASSWORD: &str = "demo2026";

/// Guest machine type per arch — one source of truth for install and run.
///
/// The machine type is part of the proven topology: OVMF's NVRAM (VARS.fd)
/// records Boot#### device paths against the PCI layout of the machine it
/// was written on, and those entries are *not* portable across machine
/// types. Observed live: a q35-written VARS booted on the default `pc`
/// (i440fx) hung OVMF before console init — "Guest has not initialized the
/// display (yet)" forever, zero disk writes, SSH deadline — while the same
/// disk+VARS booted fine on q35. Install writes VARS on q35, so run must
/// boot q35. Never let the two argv builders drift apart again.
pub fn machine_for(arch: Architecture) -> &'static str {
    match arch {
        Architecture::Amd64 => "q35",
        Architecture::Arm64 => "virt",
    }
}

/// `vnc=127.0.0.1:10,...` → `vnc://127.0.0.1:5910` (port 5900+N).
/// Returns None for non-VNC displays. The printed URL always matches
/// the actual display spec — never a hardcoded port.
pub fn vnc_url(display: &str) -> Option<String> {
    let spec = display.strip_prefix("vnc=")?;
    let addr = spec.split(',').next()?;
    let (host, disp) = addr.rsplit_once(':')?;
    let n: u16 = disp.parse().ok()?;
    let host = if host.is_empty() {
        PROVEN_SSH_HOST
    } else {
        host
    };
    Some(format!("vnc://{host}:{}", 5900 + n))
}

#[derive(Debug, Clone)]
pub struct HeadlessVmBuilder {
    pub version: WindowsVersion,
    pub config: VmConfig,
    pub disk_size_gb: u32,
    pub memory_mb: u32,
    pub smp_cores: u32,
    pub ssh_host: String,
    pub ssh_port: u16,
    /// `-display` backend (`none`, `cocoa`, `gtk`, or `vnc=<spec>`).
    pub display: Display,
    pub monitor_socket: String,
    pub qmp_socket: String,
    /// Guest serial log path (per-VM, next to the disk — never `/tmp`).
    /// Re-resolved by [`HeadlessVmBuilder::workdir`]; set explicitly only
    /// after `workdir()`.
    pub serial_log: String,
    pub unattend_dir: PathBuf,
    pub workdir: PathBuf,
    /// Portable tree root (the extracted `qemu-portable` dir holding
    /// `bin/`). `None` = resolve `qemu-system-*` / `qemu-img` via PATH.
    pub qemu_dir: Option<PathBuf>,
    /// Resume migrated RAM + device state at boot (`-incoming <uri>`):
    /// the live-snapshot fast path. `None` = normal boot. Set by the
    /// caller after verifying the state file — the builder never guesses.
    pub incoming: Option<String>,
}

impl HeadlessVmBuilder {
    pub fn new(version: WindowsVersion) -> Self {
        let (arch, _) = auto_detect_platform();
        Self {
            version,
            config: VmConfig {
                arch,
                ..VmConfig::default()
            },
            disk_size_gb: 64,
            memory_mb: 4096,
            smp_cores: 4,
            ssh_host: PROVEN_SSH_HOST.to_string(),
            ssh_port: PROVEN_SSH_PORT,
            display: Display::Vnc(PROVEN_VNC_DISPLAY.to_string()),
            monitor_socket: crate::ipc::vm_endpoint(
                std::path::Path::new("."),
                crate::ipc::MON_SOCK_NAME,
            ),
            qmp_socket: crate::ipc::vm_endpoint(
                std::path::Path::new("."),
                crate::ipc::QMP_SOCK_NAME,
            ),
            serial_log: crate::ipc::vm_endpoint(
                std::path::Path::new("."),
                crate::ipc::SERIAL_LOG_NAME,
            ),
            unattend_dir: PathBuf::from(crate::paths::FALLBACK_UNATTEND_DIR),
            workdir: PathBuf::from("."),
            qemu_dir: None,
            incoming: None,
        }
    }

    pub fn disk_size_gb(mut self, gb: u32) -> Self {
        self.disk_size_gb = gb;
        self
    }
    pub fn memory_mb(mut self, mb: u32) -> Self {
        self.memory_mb = mb;
        self
    }
    pub fn smp_cores(mut self, n: u32) -> Self {
        self.smp_cores = n;
        self
    }
    pub fn username(mut self, u: impl Into<String>) -> Self {
        self.config.username = u.into();
        self
    }
    pub fn password(mut self, p: impl Into<String>) -> Self {
        self.config.password = p.into();
        self
    }
    pub fn locale(mut self, l: impl Into<String>) -> Self {
        self.config.locale = l.into();
        self
    }
    pub fn arch(mut self, a: Architecture) -> Self {
        self.config.arch = a;
        self
    }
    pub fn autologon(mut self, on: bool) -> Self {
        self.config.auto_logon = on;
        self
    }
    pub fn ssh_port(mut self, p: u16) -> Self {
        self.ssh_port = p;
        self
    }
    pub fn vnc(mut self, spec: impl Into<String>) -> Self {
        self.display = Display::Vnc(format!("vnc={}", spec.into()));
        self
    }
    /// Display backend directly.
    pub fn display(mut self, d: Display) -> Self {
        self.display = d;
        self
    }
    pub fn monitor_socket(mut self, s: impl Into<String>) -> Self {
        self.monitor_socket = s.into();
        self
    }
    pub fn qmp_socket(mut self, s: impl Into<String>) -> Self {
        self.qmp_socket = s.into();
        self
    }
    pub fn unattend_dir(mut self, d: PathBuf) -> Self {
        self.unattend_dir = d;
        self
    }
    /// Use a portable QEMU tree (see `PortableQemu`); `dir` is the extracted
    /// `qemu-portable` root containing `bin/`.
    pub fn qemu_dir(mut self, d: PathBuf) -> Self {
        self.qemu_dir = Some(d);
        self
    }
    /// Resume migrated state at boot (`commands::migrate_read_uri` output).
    /// The caller verifies the state file first.
    pub fn incoming(mut self, uri: impl Into<String>) -> Self {
        self.incoming = Some(uri.into());
        self
    }
    /// Working dir anchor for per-VM state (defaults to `.`).
    pub fn workdir(mut self, d: PathBuf) -> Self {
        // Endpoints follow the VM dir (per-VM sockets, never /tmp):
        // explicit socket setters apply after workdir(), not before.
        self.monitor_socket = crate::ipc::vm_endpoint(&d, crate::ipc::MON_SOCK_NAME);
        self.qmp_socket = crate::ipc::vm_endpoint(&d, crate::ipc::QMP_SOCK_NAME);
        self.serial_log = crate::ipc::vm_endpoint(&d, crate::ipc::SERIAL_LOG_NAME);
        self.workdir = d;
        self
    }

    fn accel(&self) -> &'static str {
        auto_detect_platform().1
    }

    /// CPU model matching the accelerator: `host` passthrough on hardware
    /// accel (required for Windows 11 setup CPU checks to see something
    /// modern), `max` under TCG (the `virt` default is too old for Windows
    /// 11 ARM64, and `-cpu host` needs KVM/HVF to mean anything).
    fn cpu_model(&self) -> &'static str {
        if self.accel() == "tcg" { "max" } else { "host" }
    }

    /// UEFI firmware for this guest arch: read-only CODE plus a writable
    /// VARS template, both resolved strictly inside the portable tree
    /// (`<qemu-dir>/share/qemu`). No host paths are probed. Consumed as
    /// two `pflash` drives (never `-bios`): the Windows bootloader writes
    /// NVRAM boot variables, and Setup needs them to persist across its
    /// mid-install reboots.
    pub fn firmware(&self) -> Option<(PathBuf, PathBuf)> {
        let (code_names, vars_names) = self.config.arch.firmware_names();
        let q = self.qemu_dir.as_ref()?;
        let dir = q.join("share/qemu");
        let code = code_names
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_file())?;
        let vars = vars_names
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_file())?;
        Some((code, vars))
    }

    /// `<workdir>/VARS.fd`: the guest's private writable NVRAM copy.
    /// Seeded once from the portable template; never shared between VMs
    /// (locking collisions) and never written back to the template.
    pub fn vars_file(&self) -> PathBuf {
        self.workdir.join(crate::paths::VARS_FILE)
    }

    /// Copy the firmware VARS template into the private NVRAM file. Must
    /// run before argv build (the pflash arg names the file).
    ///
    /// `refresh == true` (install / ISO-boot flows): always overwrite, so
    /// OVMF starts with clean NVRAM — a reused VARS carries phantom boot
    /// entries that shadow `-boot d` and prevent ISO boot
    /// (`BlInitializeLibrary 0xc00000bb`).
    ///
    /// `refresh == false` (steady-state boot): seed only when absent, so
    /// the firmware state the guest wrote last boot (BootOrder, BootNext,
    /// policy) survives — this is the proven macOS behavior.
    fn seed_vars(&self, refresh: bool) -> Result<PathBuf> {
        let (_, template) = self.firmware().context("missing UEFI firmware")?;
        let dest = self.vars_file();
        if let Some(parent) = dest.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        if !refresh && dest.exists() {
            return Ok(dest);
        }
        std::fs::copy(&template, &dest).with_context(|| {
            format!(
                "copy VARS template {} -> {}",
                template.display(),
                dest.display()
            )
        })?;
        Ok(dest)
    }

    /// Fresh NVRAM for an install or an ISO boot: overwrite every time.
    pub fn refresh_vars(&self) -> Result<PathBuf> {
        self.seed_vars(true)
    }

    /// Steady-state boot: overwrite only when no VARS exists yet, keeping
    /// the guest's own firmware state across boots.
    pub fn seed_vars_if_absent(&self) -> Result<PathBuf> {
        self.seed_vars(false)
    }

    /// `qemu-system-*` binary: portable tree's when `qemu_dir` is set,
    /// otherwise PATH lookup by guest arch.
    pub fn system_bin(&self) -> String {
        match &self.qemu_dir {
            Some(d) => d
                .join(crate::paths::PORTABLE_BIN_DIR)
                .join(self.config.arch.qemu_binary())
                .to_string_lossy()
                .into_owned(),
            None => self.config.arch.qemu_binary().to_string(),
        }
    }

    /// `qemu-img` binary: portable tree's when `qemu_dir` is set, else PATH.
    pub fn img_bin(&self) -> PathBuf {
        match &self.qemu_dir {
            Some(d) => d
                .join(crate::paths::PORTABLE_BIN_DIR)
                .join(crate::paths::QEMU_IMG_NAME),
            None => PathBuf::from(crate::paths::QEMU_IMG_NAME),
        }
    }

    /// User-mode NIC with host-forwarded SSH. `romfile=` (empty) drops
    /// the NIC's PXE option ROM: QEMU ships it as an X64-only iPXE build,
    /// which ARM64 EDK2 loudly refuses ("Image type X64 can't be loaded")
    /// on every boot. We never PXE-boot.
    fn net_args(&self) -> Vec<String> {
        vec![
            "-netdev".into(),
            format!("user,id=n1,hostfwd=tcp::{}-:22", self.ssh_port),
            "-device".into(),
            "virtio-net-pci,netdev=n1,romfile=".into(),
        ]
    }

    /// Answer-file drive args. WinPE reads `autounattend.xml` before the
    /// VirtIO drivers in `<DriverPaths>` load, so the bus must have inbox
    /// drivers — never `if=virtio`:
    /// - x86_64 (`pc`): `-drive ...if=ide` (inbox ATA).
    /// - ARM64 (`virt` has no IDE bus): USB mass storage on the shared
    ///   xHCI bus (inbox USBSTOR — the classic answer-file-on-a-stick flow).
    ///
    /// An explicit `--unattend-iso` replaces either with a second CD-ROM.
    /// (The xHCI bus itself is added by `install_argv`, which also owns the
    /// USB keyboard — kept together so neither dangles bus-less.)
    fn answer_drive_args(&self, unattend_iso: Option<&str>) -> Vec<String> {
        if let Some(iso) = unattend_iso {
            return vec![
                "-drive".into(),
                format!("file={iso},media=cdrom,readonly=on"),
            ];
        }
        match self.config.arch {
            Architecture::Amd64 => vec![
                "-drive".into(),
                format!(
                    "file=fat:ro:{},format=raw,if=ide",
                    self.unattend_dir.display()
                ),
            ],
            Architecture::Arm64 => vec![
                "-drive".into(),
                format!(
                    "file=fat:ro:{},format=raw,if=none,id=unattend-drive,readonly=on",
                    self.unattend_dir.display()
                ),
                "-device".into(),
                "usb-storage,drive=unattend-drive".into(),
            ],
        }
    }
    /// QEMU argv for the install phase (ISOs + answer drive attached).
    pub fn install_argv(
        &self,
        disk: &str,
        win_iso: &str,
        virtio_iso: &str,
        unattend_iso: Option<&str>,
    ) -> Vec<String> {
        let mut a = vec![
            self.system_bin(),
            "-accel".into(),
            self.accel().into(),
            "-m".into(),
            self.memory_mb.to_string(),
            "-cpu".into(),
            self.cpu_model().into(),
        ];
        // Explicit machine on every arch (machine_for): the topology must
        // be the one NVRAM was written on — see `machine_for`.
        a.push("-M".into());
        a.push(machine_for(self.config.arch).into());
        a.push("-smp".into());
        a.push(self.smp_cores.to_string());
        // UEFI firmware as two pflash drives: read-only CODE + the VM's
        // private writable VARS (seeded before argv build — `refresh_vars`
        // for install/ISO boots, `seed_vars_if_absent` for plain boots).
        // Never `-bios`: the Windows bootloader writes NVRAM boot
        // variables and Setup needs them to persist across its reboots.
        if let Some((fw_code, _)) = self.firmware() {
            a.push("-drive".into());
            a.push(format!(
                "if=pflash,format=raw,readonly=on,file={}",
                fw_code.display()
            ));
            a.push("-drive".into());
            a.push(format!(
                "if=pflash,format=raw,file={}",
                self.vars_file().display()
            ));
        }
        // Target disk on the SAME NVMe controller + serial boot_argv uses
        // (inbox stornvme; part.txt/InstallTo pick Disk 0 — see the
        // template header): identical bus topology across install and
        // first boot means no controller change, no NIC slot shift, no
        // minutes-long PnP re-enumeration. Never `if=virtio` here — the
        // answer file loads no storage driver.
        //
        // Deliberately NO bootindex on the install NVMe (unlike
        // boot_argv): any bootindex makes OVMF build its boot list
        // exclusively from the fw_cfg bootorder file, and unindexed
        // devices (the installer CD) get no boot option at all —
        // observed live: setup drops to the UEFI shell instead of
        // booting the ISO. Without one, OVMF enumerates every device
        // and falls through to the installer (today's proven path, with
        // -boot order=d below unchanged).
        a.push("-drive".into());
        a.push(format!("file={disk},format=qcow2,if=none,id=tgt"));
        a.push("-device".into());
        a.push("nvme,drive=tgt,serial=nvme1".into());
        a.push("-drive".into());
        a.push(format!("file={win_iso},media=cdrom,readonly=on"));
        a.push("-drive".into());
        a.push(format!("file={virtio_iso},media=cdrom,readonly=on"));
        // Boot the installer CD first (the qcow2 target is empty).
        // x86/SeaBIOS only: on ARM64 an explicit order makes EDK2 use ONLY
        // the fw_cfg entries with no removable-media fallback, which drops
        // to the UEFI shell; without it EDK2 boots the CD by default.
        if self.config.arch == Architecture::Amd64 {
            a.push("-boot".into());
            a.push("order=d".into());
        }
        // ARM64 `virt` ships with NO input/display devices of its own:
        // xHCI + USB keyboard (send-key needs it) + USB tablet (VNC
        // pointer needs it), and ramfb as the one framebuffer (single
        // console: VNC and screendump keep working after Windows takes
        // over, unlike virtio-gpu). x86/pc has PS/2 + VGA by default.
        if self.config.arch == Architecture::Arm64 {
            a.push("-device".into());
            a.push("qemu-xhci,id=xhci".into());
            a.push("-device".into());
            a.push("usb-kbd".into());
            a.push("-device".into());
            a.push("usb-tablet".into());
            a.push("-device".into());
            a.push("ramfb".into());
        }
        a.extend(self.answer_drive_args(unattend_iso));
        // Guest serial log (EDK2 shouts here on `virt`): the only record
        // when the framebuffer is dark. Same path as the proven install.
        a.push("-serial".into());
        a.push(format!("file:{}", self.serial_log));
        a.extend(self.net_args());
        a.push("-display".into());
        a.push(self.display.as_arg());
        a.push("-monitor".into());
        a.push(crate::ipc::qemu_arg(&self.monitor_socket));
        a.push("-qmp".into());
        a.push(crate::ipc::qemu_arg(&self.qmp_socket));
        a
    }

    /// QEMU argv for ephemeral boots (no install media, no vvfat).
    pub fn boot_argv(&self, disk: &str) -> Vec<String> {
        let mut a = vec![
            self.system_bin(),
            "-accel".into(),
            self.accel().into(),
            "-m".into(),
            self.memory_mb.to_string(),
            "-cpu".into(),
            self.cpu_model().into(),
        ];
        // Explicit machine on every arch (machine_for): run must boot the
        // exact topology install wrote NVRAM on — see `machine_for`.
        a.push("-M".into());
        a.push(machine_for(self.config.arch).into());
        a.push("-smp".into());
        a.push(self.smp_cores.to_string());
        // Live resume: migrated state streams in over `-incoming`, so the
        // guest continues where the save left it instead of booting. Disk
        // and VARS are restored to the save instant by the caller — argv
        // only carries the RAM side. Guest clock isolated from the host
        // (`clock=vm`): with `clock=host` the RTC can wait out its whole
        // next period after a restore and lose ticks forever (documented
        // migration pathology); vm-clock tracking restarts cleanly.
        if let Some(uri) = &self.incoming {
            a.push("-incoming".into());
            a.push(uri.clone());
            a.push("-rtc".into());
            a.push("base=utc,clock=vm".into());
        }
        if let Some((fw_code, _)) = self.firmware() {
            a.push("-drive".into());
            a.push(format!(
                "if=pflash,format=raw,readonly=on,file={}",
                fw_code.display()
            ));
            a.push("-drive".into());
            a.push(format!(
                "if=pflash,format=raw,file={}",
                self.vars_file().display()
            ));
        }
        // Ephemeral boots need input + display too (else blind post-install).
        // (xHCI first: the USB input devices have no bus without it.)
        if self.config.arch == Architecture::Arm64 {
            a.push("-device".into());
            a.push("qemu-xhci,id=xhci".into());
            a.push("-device".into());
            a.push("usb-kbd".into());
            a.push("-device".into());
            a.push("usb-tablet".into());
            a.push("-device".into());
            a.push("ramfb".into());
        }
        // The installed system lives on NVMe (the proven install topology):
        // boot it on the identical controller + serial so Windows sees the
        // same device it was installed on — not a controller change plus a
        // minutes-long PnP re-enumeration. stornvme is inbox, so this also
        // boots disks from the legacy virtio install flow.
        a.push("-drive".into());
        a.push(format!("file={disk},format=qcow2,if=none,id=tgt"));
        a.push("-device".into());
        a.push("nvme,drive=tgt,serial=nvme1,bootindex=0".into());
        a.extend(self.net_args());
        a.push("-display".into());
        a.push(self.display.as_arg());
        a.push("-serial".into());
        a.push(format!("file:{}", self.serial_log));
        a.push("-monitor".into());
        a.push(crate::ipc::qemu_arg(&self.monitor_socket));
        a.push("-qmp".into());
        a.push(crate::ipc::qemu_arg(&self.qmp_socket));
        a
    }

    /// Fail fast unless a portable QEMU tree with UEFI firmware is wired
    /// up (`--qemu-dir`, i.e. `fetch-qemu` output). Portable-only by
    /// design: no host firmware paths are probed. Both CODE and the VARS
    /// template must be present (pflash pair; `-bios` is not used).
    fn require_firmware(&self) -> Result<()> {
        if self.qemu_dir.is_none() {
            anyhow::bail!(
                "portable QEMU is required: pass --qemu-dir <fetch-qemu tree> (run `fetch-qemu` first)"
            );
        }
        if self.firmware().is_none() {
            anyhow::bail!(
                "portable tree lacks UEFI firmware (CODE + VARS under share/qemu); re-run `fetch-qemu`"
            );
        }
        Ok(())
    }

    async fn spawn(argv: &[String]) -> Result<Child> {
        let (bin, args) = argv.split_first().context("empty argv")?;
        // Never orphan: if the owner drops this handle (CLI exit, panic,
        // task cancel), the guest dies with it. There is no detach path.
        let mut cmd = Command::new(bin);
        cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
        // Own process group (Unix): a tty Ctrl-C broadcasts SIGINT to the
        // whole foreground group. Without this, QEMU dies by signal on
        // every Ctrl-C — bypassing graceful shutdown and risking dirty
        // NTFS (observed live: "terminating on signal 2", then QMP gone).
        // Only the CLI receives the keystroke; the guest exits via ACPI.
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        // Same isolation on Windows: console Ctrl-C reaches every process
        // attached to the console. CREATE_NEW_PROCESS_GROUP detaches QEMU
        // (Ctrl-Break still would, which nobody sends by accident).
        #[cfg(windows)]
        {
            /// Win32 `CREATE_NEW_PROCESS_GROUP` (no winapi dep for one flag;
            /// tokio's `Command` carries `creation_flags` itself).
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        cmd.spawn().with_context(|| format!("spawn {bin}"))
    }

    /// Give QEMU a few seconds to reject bad argv, then fail fast instead
    /// of polling SSH for 15 minutes against a dead process. A healthy
    /// boot is still starting at this point, so a live child means success.
    async fn spawn_guarded(argv: &[String]) -> Result<Child> {
        let mut child = Self::spawn(argv).await?;
        eprintln!(
            "QEMU spawned (pid {:?}); waiting {} s ...",
            child.id(),
            timeouts::SPAWN_GUARD.as_secs()
        );
        tokio::time::sleep(timeouts::SPAWN_GUARD).await;
        match child.try_wait().context("poll qemu process")? {
            Some(status) => anyhow::bail!(
                "QEMU (pid {:?}) exited immediately (status {status}); check its stderr above for the rejected flag",
                child.id(),
            ),
            None => Ok(child),
        }
    }

    /// `<workdir>/qemu.pid` — written on launch, removed by `stop`.
    /// Lets the CLI refuse double-starts and shut down cleanly instead of
    /// orphaning QEMU when the operator's shell exits.
    pub fn pidfile(&self) -> std::path::PathBuf {
        self.workdir.join(crate::paths::QEMU_PIDFILE)
    }

    fn write_pidfile(&self, child: &Child) -> Result<()> {
        if let Some(id) = child.id() {
            std::fs::write(self.pidfile(), id.to_string())
                .with_context(|| format!("write {}", self.pidfile().display()))?;
        }
        Ok(())
    }

    /// Remove stale monitor/QMP socket files left by a previous run.
    /// Only unlinks when nothing is listening (live VM → keep, caller
    /// preflights that case separately). Without this QEMU's bind fails
    /// on the leftover file even though no process holds the port.
    async fn clear_stale_sockets(&self) {
        for sock in [&self.monitor_socket, &self.qmp_socket] {
            // Never unlink a live endpoint (a running VM's socket): only
            // stale files go. Named pipes aren't fs entries (no-op there).
            if !crate::ipc::endpoint_alive(sock).await {
                crate::ipc::stale_remove(sock);
            }
        }
    }

    /// Full install flow: render XML → stage vvfat → refresh VARS → create
    /// base → launch. Uses the portable `qemu-img` when `qemu_dir` is set.
    pub async fn build_and_start(
        self,
        disk: &str,
        win_iso: &str,
        virtio_iso: &str,
    ) -> Result<HeadlessVm> {
        self.require_firmware()?;
        let xml = self.config.generate_unattend_xml();
        DiskManager::ensure_unattend_dir(&self.unattend_dir, &xml)?;
        // Clean NVRAM: stale boot entries would shadow `-boot d`.
        self.refresh_vars()?;
        DiskManager::create_base(
            &self.img_bin(),
            std::path::Path::new(disk),
            self.disk_size_gb,
        )
        .await?;
        self.clear_stale_sockets().await;
        let argv = self.install_argv(disk, win_iso, virtio_iso, None);
        let child = Self::spawn_guarded(&argv).await?;
        self.write_pidfile(&child)?;
        Ok(HeadlessVm {
            builder: self,
            child,
            disk: disk.to_string(),
        })
    }

    /// Boot an existing (overlay) disk without install media.
    pub async fn boot_existing(self, disk: &str) -> Result<HeadlessVm> {
        self.require_firmware()?;
        // Keep the guest's NVRAM across boots (seed only when absent):
        // nothing here shadows a boot device, and the guest's own
        // BootOrder/BootNext must survive (proven macOS behavior).
        self.seed_vars_if_absent()?;
        self.clear_stale_sockets().await;
        let argv = self.boot_argv(disk);
        let mut child = Self::spawn_guarded(&argv).await?;
        // Same slow-start race as the install path (hvf startup >3 s): poll
        // for the monitor socket FILE (bind = starting), bail only on death
        // or timeout — never on a live child, or kill_on_drop destroys the
        // evidence. Deliberately no connect-probe here: QEMU's listen
        // backlog is a single slot and a probe occupies it, refusing the
        // real VNC handshake that follows (observed ECONNREFUSED on live
        // QEMU). The handshake itself retries (see install path).
        // Total wait = the declared budget, split into polls of
        // MONITOR_SOCKET_POLL_SECS (so changing either const keeps the
        // message and the loop in agreement).
        let polls = timeouts::MONITOR_SOCKET_WAIT_SECS / timeouts::MONITOR_SOCKET_POLL_SECS.max(1);
        for _ in 0..polls {
            if std::path::Path::new(&self.monitor_socket).exists() {
                break;
            }
            if let Some(status) = child.try_wait().context("poll qemu process")? {
                anyhow::bail!(
                    "QEMU (pid {:?}) exited during startup (status {status}); check its stderr above for the rejected flag",
                    child.id(),
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(
                timeouts::MONITOR_SOCKET_POLL_SECS,
            ))
            .await;
        }
        if !std::path::Path::new(&self.monitor_socket).exists() {
            let _ = child.kill().await;
            anyhow::bail!(
                "QEMU (pid {:?}) alive but no monitor socket at {} after {} s; killed it",
                child.id(),
                self.monitor_socket,
                timeouts::MONITOR_SOCKET_WAIT_SECS
            );
        }
        self.write_pidfile(&child)?;
        Ok(HeadlessVm {
            builder: self,
            child,
            disk: disk.to_string(),
        })
    }
}

/// Re-exported transport check (lives in `ipc`; kept here so existing
/// `qemu::qmp_alive` import paths don't churn).
pub use crate::ipc::qmp_alive;

pub struct HeadlessVm {
    pub builder: HeadlessVmBuilder,
    pub child: Child,
    pub disk: String,
}

impl HeadlessVm {
    pub async fn wait_for_ssh(&self, timeout: std::time::Duration) -> Result<()> {
        ssh::wait_for_ssh(&self.builder.ssh_host, self.builder.ssh_port, timeout).await
    }

    pub async fn qmp(&self) -> Result<QmpClient> {
        QmpClient::connect(&self.builder.qmp_socket).await
    }

    pub async fn snapshot_live(&self, overlay: &str) -> Result<()> {
        let mut q = self.qmp().await?;
        q.live_overlay("drive0", overlay).await
    }

    /// Tap guest keys via QMP (e.g. `&["ret"]` for the ISO boot prompt).
    pub async fn send_keys(&self, keys: &[&str]) -> Result<()> {
        let mut q = self.qmp().await?;
        q.send_keys(keys).await
    }

    /// Graceful shutdown over SSH (built-in russh client, quote-free
    /// command — no host `ssh` needed, no quoting to mangle), required
    /// before offline `create_overlay` to avoid a dirty-NTFS `chkdsk` on
    /// next boot.
    pub async fn shutdown_cleanly(&self) -> Result<()> {
        crate::ssh::ssh_exec(
            &self.builder.ssh_host,
            self.builder.ssh_port,
            &self.builder.config.username,
            &self.builder.config.password,
            "powershell -NoProfile -Command Stop-Computer -Force",
        )
        .await?;
        Ok(())
    }

    /// Graceful teardown for the supervisor path: ACPI powerdown (clean
    /// Windows shutdown, no dirty NTFS bit), wait up to `timeout` for the
    /// process to exit, SIGKILL fallback with a warning. Dropping the VM
    /// without this still kills QEMU via `kill_on_drop` — just dirtier.
    pub async fn shutdown_graceful(&mut self, timeout: std::time::Duration) -> Result<()> {
        // Powerdown errors are LOUD, never swallowed: a guest that never
        // got the message makes the whole timeout expire feeling hung
        // (observed live). Failure here still falls through to the wait
        // (the guest may already be exiting) but the log says so.
        match self.qmp().await {
            Ok(mut q) => match q.powerdown().await {
                Ok(_) => eprintln!("ACPI powerdown sent; waiting for guest exit ..."),
                Err(e) => eprintln!("warning: powerdown send failed ({e:#}); waiting anyway ..."),
            },
            Err(e) => eprintln!("warning: no QMP for powerdown ({e:#}); waiting anyway ..."),
        }
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(_) => Ok(()),
            Err(_) => {
                tracing::warn!("guest did not exit in time; SIGKILL (next boot may chkdsk)");
                self.child.kill().await.context("kill qemu")?;
                let _ = self.child.wait().await;
                Ok(())
            }
        }
    }

    pub async fn kill(mut self) -> Result<()> {
        self.child.kill().await.context("kill qemu")?;
        let _ = self.child.wait().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_sizing_reaches_boot_argv() {
        // Same flags-through-argv proof for the boot path: what the
        // builder holds is what QEMU gets, no VM needed.
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11)
            .memory_mb(8192)
            .smp_cores(8);
        let argv = b.boot_argv("d.qcow2");
        let at = |flag: &str| argv[argv.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(at("-m"), "8192");
        assert_eq!(at("-smp"), "8");
    }

    #[test]
    fn builder_incoming_reaches_boot_argv() {
        // Live resume: -incoming streams the migrated state file, no boot.
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11).incoming("exec:cat '/tmp/state'");
        let argv = b.boot_argv("d.qcow2");
        let at = |flag: &str| argv[argv.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(at("-incoming"), "exec:cat '/tmp/state'");
        // Restore carries isolated guest time (documented RTC-stop
        // workaround); plain boots set no -rtc.
        assert_eq!(at("-rtc"), "base=utc,clock=vm");
        // Default boot carries no -incoming.
        let plain = HeadlessVmBuilder::new(WindowsVersion::Win11).boot_argv("d.qcow2");
        assert!(!plain.iter().any(|a| a == "-incoming"));
        assert!(!plain.iter().any(|a| a == "-rtc"));
    }

    #[test]
    fn display_parse_rejects_typos() {
        assert_eq!(Display::parse("cocoa").unwrap(), Display::Cocoa);
        assert_eq!(Display::parse("none").unwrap(), Display::None);
        assert_eq!(
            Display::parse("vnc=127.0.0.1:10,password=on")
                .unwrap()
                .as_arg(),
            "vnc=127.0.0.1:10,password=on"
        );
        assert!(Display::parse("x11").is_err());
        assert!(Display::parse("vnc").is_err());
        assert!(!Display::Cocoa.is_vnc());
        assert!(Display::parse("vnc=127.0.0.1:0").unwrap().is_vnc());
    }

    #[test]
    fn vnc_display_matches_host() {
        // PROVEN_VNC_DISPLAY is QEMU syntax (can't concat! a const into
        // it), so this test keeps its bind address glued to PROVEN_SSH_HOST.
        let spec = PROVEN_VNC_DISPLAY.strip_prefix("vnc=").unwrap();
        let addr = spec.split(',').next().unwrap();
        let (host, _) = addr.rsplit_once(':').unwrap();
        assert_eq!(host, PROVEN_SSH_HOST);
    }

    #[test]
    fn vvfat_never_virtio() {
        // x86_64/pc: inbox IDE.
        let x86 = HeadlessVmBuilder::new(WindowsVersion::Win11).arch(Architecture::Amd64);
        let argv = x86.install_argv("d.qcow2", "w.iso", "v.iso", None);
        let vvfat = argv.iter().find(|a| a.starts_with("file=fat:")).unwrap();
        assert!(
            vvfat.contains("if=ide"),
            "vvfat must be if=ide, got {vvfat}"
        );
        // ARM64/virt has no IDE bus: USB mass storage (inbox USBSTOR).
        let arm = HeadlessVmBuilder::new(WindowsVersion::Win11).arch(Architecture::Arm64);
        let argv = arm.install_argv("d.qcow2", "w.iso", "v.iso", None);
        let vvfat = argv.iter().find(|a| a.starts_with("file=fat:")).unwrap();
        assert!(vvfat.contains("if=none"), "got {vvfat}");
        assert!(argv.iter().any(|a| a == "qemu-xhci,id=xhci"));
        assert!(argv.iter().any(|a| a == "usb-storage,drive=unattend-drive"));
        // ARM64/virt has no keyboard either: send-key needs usb-kbd or the
        // taps are accepted by QMP but never reach the guest.
        assert!(argv.iter().any(|a| a == "usb-kbd"));
        assert!(argv.iter().any(|a| a == "usb-tablet"));
        // ...and the keyboard bus must exist even with --unattend-iso.
        let argv_iso = arm.install_argv("d.qcow2", "w.iso", "v.iso", Some("u.iso"));
        assert!(argv_iso.iter().any(|a| a == "qemu-xhci,id=xhci"));
        assert!(argv_iso.iter().any(|a| a == "usb-kbd"));
        // ARM needs a real framebuffer (ramfb: single console, observable
        // under Windows) and input devices for VNC/send-key.
        assert!(argv.iter().any(|a| a == "ramfb"));
        assert!(!argv.iter().any(|a| a == "virtio-gpu-pci"));
        assert!(arm.boot_argv("d.qcow2").iter().any(|a| a == "ramfb"));
        // Guest serial log for dark-framebuffer forensics, install + boot.
        assert!(argv.iter().any(|a| a.ends_with("serial.log")));
        assert!(
            arm.boot_argv("d.qcow2")
                .iter()
                .any(|a| a.ends_with("serial.log"))
        );
        for argv in [x86.install_argv("d.qcow2", "w.iso", "v.iso", None), argv] {
            assert!(
                !argv
                    .iter()
                    .any(|a| a.contains("if=virtio") && a.starts_with("file=fat:"))
            );
            // Install target: NVMe (same controller as boot_argv, never
            // virtio-blk) but WITHOUT bootindex — any bootindex starves
            // the unindexed installer CD of a boot option in OVMF
            // (observed: setup drops to the UEFI shell instead).
            let os = argv.iter().find(|a| a.contains(".qcow2")).unwrap();
            assert!(os.contains("if=none,id=tgt"), "got {os}");
            assert!(argv.iter().any(|a| a == "nvme,drive=tgt,serial=nvme1"));
            assert!(!argv.iter().any(|a| a.contains("bootindex")));
        }
    }

    #[test]
    fn install_and_boot_share_the_nvme_controller() {
        // One controller identity across install and first boot: same
        // device string (bus topology + serial), differing only by
        // boot_argv's bootindex (firmware boot priority only — invisible
        // to Windows PnP, so it never perturbs device enumeration).
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11);
        let install = b.install_argv("d.qcow2", "w.iso", "v.iso", None);
        let boot = b.boot_argv("d.qcow2");
        let dev = |argv: &[String]| {
            argv.iter()
                .find(|a| a.starts_with("nvme,drive=tgt"))
                .expect("NVMe target device present")
                .clone()
        };
        assert_eq!(dev(&install), "nvme,drive=tgt,serial=nvme1");
        assert_eq!(dev(&boot), "nvme,drive=tgt,serial=nvme1,bootindex=0");
    }

    #[test]
    fn unattend_iso_replaces_vvfat() {
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11);
        let argv = b.install_argv("d.qcow2", "w.iso", "v.iso", Some("u.iso"));
        assert!(
            argv.iter()
                .any(|a| a.contains("u.iso") && a.contains("media=cdrom"))
        );
        assert!(!argv.iter().any(|a| a.starts_with("file=fat:")));
    }

    #[test]
    fn ephemeral_boot_has_no_install_media() {
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11);
        let argv = b.boot_argv("overlay.qcow2");
        assert!(!argv.iter().any(|a| a.starts_with("file=fat:")));
        assert!(!argv.iter().any(|a| a.contains("media=cdrom")));
        // Installed on NVMe, booted on NVMe: same controller + serial as
        // the proven install, never a virtio-blk surprise.
        assert!(
            argv.iter()
                .any(|a| a == "nvme,drive=tgt,serial=nvme1,bootindex=0")
        );
        assert!(
            !argv
                .iter()
                .any(|a| a.contains(".qcow2") && a.contains("if=virtio"))
        );
    }

    #[test]
    fn portable_dir_overrides_path_lookup() {
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11);
        assert_eq!(b.system_bin(), b.config.arch.qemu_binary());
        assert_eq!(b.img_bin(), PathBuf::from("qemu-img"));

        let p = b.qemu_dir(PathBuf::from("/cache/qemu-portable/11.1.1/qemu-portable"));
        let sys = p.system_bin();
        assert!(
            sys.starts_with("/cache/qemu-portable/11.1.1/qemu-portable/bin/qemu-system-"),
            "got {sys}"
        );
        assert_eq!(
            p.img_bin(),
            PathBuf::from("/cache/qemu-portable/11.1.1/qemu-portable/bin/qemu-img")
        );
        // argv[0] is the resolved binary.
        assert_eq!(p.install_argv("d.qcow2", "w.iso", "v.iso", None)[0], sys);
    }

    #[test]
    fn machine_type_is_pinned_on_both_arches() {
        // Shared NIC shape (incl. romfile= PXE-ROM suppression) on both.
        for b in [
            HeadlessVmBuilder::new(WindowsVersion::Win11),
            HeadlessVmBuilder::new(WindowsVersion::Win11).arch(Architecture::Arm64),
        ] {
            let argv = b.install_argv("d.qcow2", "w.iso", "v.iso", None);
            assert!(
                argv.iter()
                    .any(|a| a == "virtio-net-pci,netdev=n1,romfile=")
            );
        } // qemu-system-aarch64 has no default machine: -M virt is required.
        let arm = HeadlessVmBuilder::new(WindowsVersion::Win11).arch(Architecture::Arm64);
        for argv in [
            arm.install_argv("d.qcow2", "w.iso", "v.iso", None),
            arm.boot_argv("d.qcow2"),
        ] {
            let pos = argv.iter().position(|a| a == "-M").expect("-M present");
            assert_eq!(argv[pos + 1], "virt");
        }
        // x86 pins q35 — the exact topology install wrote NVRAM on
        // (default `pc` + q35-written VARS hangs OVMF, machine_for).
        let x86 = HeadlessVmBuilder::new(WindowsVersion::Win11).arch(Architecture::Amd64);
        for argv in [
            x86.install_argv("d.qcow2", "w.iso", "v.iso", None),
            x86.boot_argv("d.qcow2"),
        ] {
            let pos = argv.iter().position(|a| a == "-M").expect("-M present");
            assert_eq!(argv[pos + 1], "q35");
        }
    }

    #[test]
    fn firmware_is_portable_tree_only() {
        // No --qemu-dir: no firmware, never host paths.
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11);
        assert_eq!(b.qemu_dir, None);
        assert!(b.firmware().is_none());
        assert!(b.require_firmware().is_err());

        // Portable tree with CODE + VARS: resolves both inside the tree.
        let tmp = tempfile::tempdir().unwrap();
        let fw_dir = tmp.path().join("qemu-portable/share/qemu");
        std::fs::create_dir_all(&fw_dir).unwrap();
        std::fs::write(fw_dir.join("edk2-aarch64-code.fd"), b"fake-code").unwrap();
        std::fs::write(fw_dir.join("edk2-arm-vars.fd"), b"fake-vars").unwrap();
        let p = HeadlessVmBuilder::new(WindowsVersion::Win11)
            .arch(Architecture::Arm64)
            .qemu_dir(tmp.path().join("qemu-portable"))
            .workdir(tmp.path().join("vm"));
        let (code, vars) = p.firmware().unwrap();
        assert_eq!(code, fw_dir.join("edk2-aarch64-code.fd"));
        assert_eq!(vars, fw_dir.join("edk2-arm-vars.fd"));
        assert!(p.require_firmware().is_ok());

        // VARS seeding: steady-state keeps guest NVRAM, install refreshes.
        let seeded = p.seed_vars_if_absent().unwrap();
        assert_eq!(seeded, tmp.path().join("vm/VARS.fd"));
        assert_eq!(std::fs::read(&seeded).unwrap(), b"fake-vars");
        std::fs::write(&seeded, b"guest-modified").unwrap();
        // Seed-once: a second call leaves the guest's firmware state alone.
        assert_eq!(p.seed_vars_if_absent().unwrap(), seeded);
        assert_eq!(std::fs::read(&seeded).unwrap(), b"guest-modified");
        // Install / ISO boot: fresh template every time (stale boot entries
        // would shadow `-boot d`).
        assert_eq!(p.refresh_vars().unwrap(), seeded);
        assert_eq!(std::fs::read(&seeded).unwrap(), b"fake-vars");

        // pflash pair in argv, no -bios anywhere.
        let argv = p.install_argv("d.qcow2", "w.iso", "v.iso", None);
        assert!(!argv.iter().any(|a| a == "-bios"));
        let pf: Vec<_> = argv.iter().filter(|a| a.starts_with("if=pflash")).collect();
        assert_eq!(pf.len(), 2);
        assert!(pf[0].contains("readonly=on"));
        assert!(pf[0].contains("edk2-aarch64-code.fd"));
        assert!(pf[1].contains("vm/VARS.fd"));
        assert!(!pf[1].contains("readonly"));
    }
}
