use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::{Child, Command};

use crate::qmp::QmpClient;
use crate::ssh;
use crate::storage::DiskManager;
use crate::unattend::{Architecture, VmConfig, auto_detect_platform};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsVersion {
    Win11EnterpriseEval,
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
    /// `-display` spec (`none`, `cocoa`, `gtk`, `vnc=127.0.0.1:0`).
    pub display: String,
    pub monitor_socket: String,
    pub qmp_socket: String,
    pub unattend_dir: PathBuf,
    pub workdir: PathBuf,
    /// Portable tree root (the extracted `qemu-portable` dir holding
    /// `bin/`). `None` = resolve `qemu-system-*` / `qemu-img` via PATH.
    pub qemu_dir: Option<PathBuf>,
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
            ssh_host: "127.0.0.1".to_string(),
            ssh_port: 2222,
            display: "none".to_string(),
            monitor_socket: "/tmp/qemu-monitor.sock".to_string(),
            qmp_socket: "/tmp/qemu-qmp.sock".to_string(),
            unattend_dir: PathBuf::from("./unattend_dir"),
            workdir: PathBuf::from("."),
            qemu_dir: None,
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
        self.display = format!("vnc={}", spec.into());
        self
    }
    /// Raw `-display` spec: `none`, `cocoa`, `gtk`, `vnc=127.0.0.1:0`.
    pub fn display(mut self, spec: impl Into<String>) -> Self {
        self.display = spec.into();
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
    /// Working dir anchor for per-VM state (defaults to `.`).
    pub fn workdir(mut self, d: PathBuf) -> Self {
        self.workdir = d;
        self
    }

    fn accel(&self) -> &'static str {
        auto_detect_platform().1
    }

    /// UEFI firmware for this guest arch: read-only CODE plus a writable
    /// VARS template, both resolved strictly inside the portable tree
    /// (`<qemu-dir>/share/qemu`). No host paths are probed. Consumed as
    /// two `pflash` drives (never `-bios`): the Windows bootloader writes
    /// NVRAM boot variables, and Setup needs them to persist across its
    /// mid-install reboots.
    pub fn firmware(&self) -> Option<(PathBuf, PathBuf)> {
        let (code_names, vars_names): (&[&str], &[&str]) = match self.config.arch {
            Architecture::Amd64 => (
                &["edk2-x86_64-code.fd", "OVMF_CODE.fd"],
                &["edk2-i386-vars.fd", "OVMF_VARS.fd"],
            ),
            Architecture::Arm64 => (&["edk2-aarch64-code.fd"], &["edk2-arm-vars.fd"]),
        };
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
        self.workdir.join("VARS.fd")
    }

    /// Seed the private VARS copy if absent. Must run before argv build.
    pub fn ensure_vars(&self) -> Result<PathBuf> {
        let (_, template) = self.firmware().context("missing UEFI firmware")?;
        let dest = self.vars_file();
        if !dest.exists() {
            if let Some(parent) = dest.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            std::fs::copy(&template, &dest).with_context(|| {
                format!(
                    "copy VARS template {} -> {}",
                    template.display(),
                    dest.display()
                )
            })?;
        }
        Ok(dest)
    }

    /// `qemu-system-*` binary: portable tree's when `qemu_dir` is set,
    /// otherwise PATH lookup by guest arch.
    pub fn system_bin(&self) -> String {
        match &self.qemu_dir {
            Some(d) => d
                .join("bin")
                .join(self.config.arch.qemu_binary())
                .to_string_lossy()
                .into_owned(),
            None => self.config.arch.qemu_binary().to_string(),
        }
    }

    /// `qemu-img` binary: portable tree's when `qemu_dir` is set, else PATH.
    pub fn img_bin(&self) -> PathBuf {
        match &self.qemu_dir {
            Some(d) => d.join("bin/qemu-img"),
            None => PathBuf::from("qemu-img"),
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
        ];
        // Host CPU passthrough on hardware accel (required for Windows 11
        // setup CPU checks to see something modern; TCG keeps the default).
        if self.accel() != "tcg" {
            a.push("-cpu".into());
            a.push("host".into());
        }
        // qemu-system-aarch64 has no default machine (x86 defaults to pc).
        if self.config.arch == Architecture::Arm64 {
            a.push("-M".into());
            a.push("virt".into());
        }
        a.push("-smp".into());
        a.push(self.smp_cores.to_string());
        // UEFI firmware as two pflash drives: read-only CODE + the VM's
        // private writable VARS (seeded by ensure_vars before argv build).
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
        a.push("-drive".into());
        a.push(format!("file={disk},format=qcow2,if=virtio"));
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
        // when the framebuffer is dark.
        a.push("-serial".into());
        a.push(format!(
            "file:{}",
            self.workdir.join("serial.log").display()
        ));
        a.extend(self.net_args());
        a.push("-display".into());
        a.push(self.display.clone());
        a.push("-monitor".into());
        a.push(format!("unix:{},server,nowait", self.monitor_socket));
        a.push("-qmp".into());
        a.push(format!("unix:{},server,nowait", self.qmp_socket));
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
        ];
        if self.accel() != "tcg" {
            a.push("-cpu".into());
            a.push("host".into());
        }
        if self.config.arch == Architecture::Arm64 {
            a.push("-M".into());
            a.push("virt".into());
        }
        a.push("-smp".into());
        a.push(self.smp_cores.to_string());
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
        a.push("-drive".into());
        a.push(format!("file={disk},format=qcow2,if=virtio"));
        a.extend(self.net_args());
        a.push("-display".into());
        a.push(self.display.clone());
        a.push("-serial".into());
        a.push(format!(
            "file:{}",
            self.workdir.join("serial.log").display()
        ));
        a.push("-monitor".into());
        a.push(format!("unix:{},server,nowait", self.monitor_socket));
        a.push("-qmp".into());
        a.push(format!("unix:{},server,nowait", self.qmp_socket));
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
        Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {bin}"))
    }

    /// Give QEMU a few seconds to reject bad argv, then fail fast instead
    /// of polling SSH for 15 minutes against a dead process. A healthy
    /// boot is still starting at this point, so a live child means success.
    async fn spawn_guarded(argv: &[String]) -> Result<Child> {
        let mut child = Self::spawn(argv).await?;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        match child.try_wait().context("poll qemu process")? {
            Some(status) => anyhow::bail!(
                "QEMU exited immediately (status {status}); check its stderr above for the rejected flag"
            ),
            None => Ok(child),
        }
    }

    /// `<workdir>/qemu.pid` — written on launch, removed by `stop`.
    /// Lets the CLI refuse double-starts and shut down cleanly instead of
    /// orphaning QEMU when the operator's shell exits.
    pub fn pidfile(&self) -> std::path::PathBuf {
        self.workdir.join("qemu.pid")
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
    fn clear_stale_sockets(&self) {
        for sock in [&self.monitor_socket, &self.qmp_socket] {
            if !qmp_alive(sock) {
                let _ = std::fs::remove_file(sock);
            }
        }
    }

    /// Full install flow: render XML → stage vvfat → seed VARS → create
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
        self.ensure_vars()?;
        DiskManager::create_base(
            &self.img_bin(),
            std::path::Path::new(disk),
            self.disk_size_gb,
        )
        .await?;
        self.clear_stale_sockets();
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
        self.ensure_vars()?;
        self.clear_stale_sockets();
        let argv = self.boot_argv(disk);
        let child = Self::spawn_guarded(&argv).await?;
        self.write_pidfile(&child)?;
        Ok(HeadlessVm {
            builder: self,
            child,
            disk: disk.to_string(),
        })
    }
}

/// True when something accepts on the QMP socket (i.e. our QEMU is alive).
/// A stale socket file with no listener refuses → not alive.
pub fn qmp_alive(socket: &str) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

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

    /// Graceful shutdown over SSH (`Stop-Computer -Force`), required before
    /// offline `create_overlay` to avoid a dirty-NTFS `chkdsk` on next boot.
    pub async fn shutdown_cleanly(&self) -> Result<()> {
        let status = Command::new("ssh")
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-p")
            .arg(self.builder.ssh_port.to_string())
            .arg(format!(
                "{}@{}",
                self.builder.config.username, self.builder.ssh_host
            ))
            .arg("powershell.exe -NoProfile -Command \"Stop-Computer -Force\"")
            .status()
            .await
            .context("ssh shutdown")?;
        if !status.success() {
            anyhow::bail!("remote shutdown command failed");
        }
        Ok(())
    }

    /// Graceful teardown for the supervisor path: ACPI powerdown (clean
    /// Windows shutdown, no dirty NTFS bit), wait up to `timeout` for the
    /// process to exit, SIGKILL fallback with a warning. Dropping the VM
    /// without this still kills QEMU via `kill_on_drop` — just dirtier.
    pub async fn shutdown_graceful(&mut self, timeout: std::time::Duration) -> Result<()> {
        if let Ok(mut q) = self.qmp().await {
            let _ = q.powerdown().await;
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
    fn vvfat_never_virtio() {
        // x86_64/pc: inbox IDE.
        let x86 =
            HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval).arch(Architecture::Amd64);
        let argv = x86.install_argv("d.qcow2", "w.iso", "v.iso", None);
        let vvfat = argv.iter().find(|a| a.starts_with("file=fat:")).unwrap();
        assert!(
            vvfat.contains("if=ide"),
            "vvfat must be if=ide, got {vvfat}"
        );
        // ARM64/virt has no IDE bus: USB mass storage (inbox USBSTOR).
        let arm =
            HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval).arch(Architecture::Arm64);
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
            let os = argv.iter().find(|a| a.contains(".qcow2")).unwrap();
            assert!(os.contains("if=virtio"));
        }
    }

    #[test]
    fn unattend_iso_replaces_vvfat() {
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval);
        let argv = b.install_argv("d.qcow2", "w.iso", "v.iso", Some("u.iso"));
        assert!(
            argv.iter()
                .any(|a| a.contains("u.iso") && a.contains("media=cdrom"))
        );
        assert!(!argv.iter().any(|a| a.starts_with("file=fat:")));
    }

    #[test]
    fn ephemeral_boot_has_no_install_media() {
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval);
        let argv = b.boot_argv("overlay.qcow2");
        assert!(!argv.iter().any(|a| a.starts_with("file=fat:")));
        assert!(!argv.iter().any(|a| a.contains("media=cdrom")));
    }

    #[test]
    fn portable_dir_overrides_path_lookup() {
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval);
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
    fn arm64_gets_virt_machine_x86_does_not() {
        // Shared NIC shape (incl. romfile= PXE-ROM suppression) on both.
        for b in [
            HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval),
            HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval).arch(Architecture::Arm64),
        ] {
            let argv = b.install_argv("d.qcow2", "w.iso", "v.iso", None);
            assert!(
                argv.iter()
                    .any(|a| a == "virtio-net-pci,netdev=n1,romfile=")
            );
        } // qemu-system-aarch64 has no default machine: -M virt is required.
        let arm =
            HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval).arch(Architecture::Arm64);
        for argv in [
            arm.install_argv("d.qcow2", "w.iso", "v.iso", None),
            arm.boot_argv("d.qcow2"),
        ] {
            let pos = argv.iter().position(|a| a == "-M").expect("-M present");
            assert_eq!(argv[pos + 1], "virt");
        }
        let x86 =
            HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval).arch(Architecture::Amd64);
        assert!(
            !x86.install_argv("d.qcow2", "w.iso", "v.iso", None)
                .iter()
                .any(|a| a == "-M")
        );
    }

    #[test]
    fn firmware_is_portable_tree_only() {
        // No --qemu-dir: no firmware, never host paths.
        let b = HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval);
        assert_eq!(b.qemu_dir, None);
        assert!(b.firmware().is_none());
        assert!(b.require_firmware().is_err());

        // Portable tree with CODE + VARS: resolves both inside the tree.
        let tmp = tempfile::tempdir().unwrap();
        let fw_dir = tmp.path().join("qemu-portable/share/qemu");
        std::fs::create_dir_all(&fw_dir).unwrap();
        std::fs::write(fw_dir.join("edk2-aarch64-code.fd"), b"fake-code").unwrap();
        std::fs::write(fw_dir.join("edk2-arm-vars.fd"), b"fake-vars").unwrap();
        let p = HeadlessVmBuilder::new(WindowsVersion::Win11EnterpriseEval)
            .arch(Architecture::Arm64)
            .qemu_dir(tmp.path().join("qemu-portable"))
            .workdir(tmp.path().join("vm"));
        let (code, vars) = p.firmware().unwrap();
        assert_eq!(code, fw_dir.join("edk2-aarch64-code.fd"));
        assert_eq!(vars, fw_dir.join("edk2-arm-vars.fd"));
        assert!(p.require_firmware().is_ok());

        // VARS seeding: copies template once, keeps the private copy after.
        let seeded = p.ensure_vars().unwrap();
        assert_eq!(seeded, tmp.path().join("vm/VARS.fd"));
        assert_eq!(std::fs::read(&seeded).unwrap(), b"fake-vars");
        std::fs::write(&seeded, b"guest-modified").unwrap();
        assert_eq!(p.ensure_vars().unwrap(), seeded);
        assert_eq!(std::fs::read(&seeded).unwrap(), b"guest-modified");

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
