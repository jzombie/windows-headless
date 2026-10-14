use quick_xml::escape::escape;
use std::env::consts;

/// Microsoft's public key token, identical on every inbox component.
const PUBLIC_KEY_TOKEN: &str = "31bf3856ad364e35";

/// Default guest username. Single source of truth: `VmConfig::default`,
/// the CLI `--username` defaults, and the sidecar fallback all reference
/// this const directly (clap accepts const paths in `default_value`).
pub const DEFAULT_USERNAME: &str = "AdminUser";

/// Default guest password. Its only job is passing Windows'
/// password-complexity filter unattended — Setup rejects weak passwords
/// and fails the install, so the default must contain upper, lower,
/// digit, and special by construction. It is neither secure nor memorable
/// on purpose: nobody types it (copy-paste once from the ready line),
/// the VM sits behind slirp NAT with no LAN exposure, and serious users
/// override it via `--password`. Same single-source rule as
/// [`DEFAULT_USERNAME`].
pub const DEFAULT_PASSWORD: &str = "P@ssw0rd123!";

/// CPU architecture targeted by the answer file and QEMU binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    Amd64,
    Arm64,
}

impl Architecture {
    pub fn as_xml_str(&self) -> &'static str {
        match self {
            Architecture::Amd64 => "amd64",
            Architecture::Arm64 => "arm64",
        }
    }

    pub fn qemu_binary(&self) -> &'static str {
        match self {
            Architecture::Amd64 => "qemu-system-x86_64",
            Architecture::Arm64 => "qemu-system-aarch64",
        }
    }

    /// UEFI firmware template names for this guest arch: read-only CODE
    /// candidates plus writable VARS candidates, resolved under
    /// `<qemu-dir>/share/qemu` (first hit wins).
    pub fn firmware_names(&self) -> (&[&str], &[&str]) {
        match self {
            Architecture::Amd64 => (
                &[
                    "edk2-x86_64-secure-code.fd",
                    "edk2-x86_64-code.fd",
                    "OVMF_CODE.fd",
                ],
                &["edk2-i386-vars.fd", "OVMF_VARS.fd"],
            ),
            Architecture::Arm64 => (&["edk2-aarch64-code.fd"], &["edk2-arm-vars.fd"]),
        }
    }

    /// OxDock release tarball for this guest arch
    /// (`oxdock-x86_64-pc-windows-msvc.tar.gz` vs
    /// `oxdock-aarch64-pc-windows-msvc.tar.gz`; both unpack to a single
    /// `oxdock.exe`). Replaces the Win32-OpenSSH zip slot: the guest runs
    /// no OpenSSH at all.
    pub fn oxdock_asset_name(&self) -> &'static str {
        match self {
            Architecture::Amd64 => "oxdock-x86_64-pc-windows-msvc.tar.gz",
            Architecture::Arm64 => "oxdock-aarch64-pc-windows-msvc.tar.gz",
        }
    }

    /// Expected SHA-256 of [`Self::oxdock_asset_name`] at
    /// [`crate::pins::OXDOCK_RELEASE_TAG`]. Strict per-arch mapping — no
    /// fallback, no caller-side `if`: the wrong hash for the wrong arch is
    /// a guaranteed mismatch, which is exactly what we want.
    pub fn oxdock_sha256(&self) -> &'static str {
        match self {
            Architecture::Amd64 => crate::pins::OXDOCK_SHA256_WIN64,
            Architecture::Arm64 => crate::pins::OXDOCK_SHA256_ARM64,
        }
    }

    /// ISO-cache slot for the extracted guest `oxdock.exe`
    /// (`oxdock-amd64.exe` vs `oxdock-arm64.exe`): `setup`/`fetch`
    /// download the release tarball, verify it, and unpack the exe here;
    /// `build-media` stages it onto the installer USB.
    pub fn oxdock_exe_cache_name(&self) -> String {
        format!("oxdock-{}.exe", self.as_xml_str())
    }

    /// NetKVM driver dir inside virtio-win.iso (`NetKVM/w11/<this>`).
    /// Matched case-insensitively, so case is cosmetic; doubles as the
    /// staged output dir (`drivers/NetKVM/w11/<this>/`).
    pub fn netkvm_iso_dir(&self) -> &'static str {
        match self {
            Architecture::Amd64 => "amd64",
            Architecture::Arm64 => "arm64",
        }
    }
}

impl std::str::FromStr for Architecture {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "amd64" | "x86_64" => Ok(Architecture::Amd64),
            "arm64" | "aarch64" => Ok(Architecture::Arm64),
            other => Err(anyhow::anyhow!(
                "unknown arch '{other}' (want amd64|x86_64|arm64|aarch64)"
            )),
        }
    }
}

/// User intent for the answer file; rendering is pure `format!` + XML escaping.
/// Single-USB design: the install stick is always drive C: in WinPE, so no
/// drive letter is configurable.
#[derive(Debug, Clone)]
pub struct VmConfig {
    pub username: String,
    pub password: String,
    pub locale: String,
    pub timezone: String,
    pub arch: Architecture,
    /// Single `LogonCount 1` auto-logon; default false. No desktop session
    /// is needed (sshd runs headless); callers opt in explicitly.
    pub auto_logon: bool,
}

impl Default for VmConfig {
    fn default() -> Self {
        let (arch, _) = auto_detect_platform();
        Self {
            username: DEFAULT_USERNAME.to_string(),
            password: DEFAULT_PASSWORD.to_string(),
            locale: "en-US".to_string(),
            timezone: "Pacific Standard Time".to_string(),
            arch,
            auto_logon: false,
        }
    }
}

/// `/dev/kvm` present *and* openable for read/write. OrbStack (Apple
/// Virtualization Framework) and plain containers expose no KVM node at
/// all; rootless boxes may expose one owned by `kvm:root` that this user
/// cannot open. Either case must fall back to TCG instead of passing
/// `-accel kvm` and dying with "Could not access KVM kernel module".
pub fn kvm_available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

/// Resolve the accelerator: `$WINDOWS_HEADLESS_ACCEL` wins when set to
/// `kvm|hvf|tcg`, else per-OS default with a Linux KVM probe. On Linux
/// without usable KVM (OrbStack, docker, WSL1, unprivileged chroot) this
/// returns `tcg` — slow (expect 5-15x on install) but bootable. Run on the
/// macOS host (hvf) or bare-metal Linux (kvm) for full speed.
pub fn resolve_accel() -> &'static str {
    if let Ok(v) = std::env::var("WINDOWS_HEADLESS_ACCEL") {
        match v.to_ascii_lowercase().as_str() {
            "kvm" => return "kvm",
            "hvf" => return "hvf",
            "tcg" => return "tcg",
            other => {
                tracing::warn!(
                    "ignoring unknown $WINDOWS_HEADLESS_ACCEL='{other}' (want kvm|hvf|tcg)"
                );
            }
        }
    }
    match consts::OS {
        "macos" => "hvf",
        "linux" => {
            if kvm_available() {
                "kvm"
            } else {
                tracing::warn!(
                    "no usable /dev/kvm — falling back to tcg (very slow; run on the macOS host with hvf or bare-metal Linux with KVM for full speed; override with $WINDOWS_HEADLESS_ACCEL)"
                );
                "tcg"
            }
        }
        _ => "tcg",
    }
}

/// Detect host arch/accel. Unknown arch falls back to Amd64 with a warning
/// (caller may override); WHPX is intentionally not a v1 target.
pub fn auto_detect_platform() -> (Architecture, &'static str) {
    let arch = match consts::ARCH {
        "x86_64" => Architecture::Amd64,
        "aarch64" => Architecture::Arm64,
        other => {
            tracing::warn!(
                "unknown host arch '{other}', defaulting to amd64 (override via VmConfig)"
            );
            Architecture::Amd64
        }
    };
    (arch, resolve_accel())
}

impl VmConfig {
    pub fn generate_unattend_xml(&self) -> String {
        let arch = self.arch.as_xml_str();
        let user = escape(&self.username);
        let pass = escape(&self.password);
        let locale = escape(&self.locale);
        let tz = escape(&self.timezone);

        let auto_logon_xml = if self.auto_logon {
            format!(
                r#"
      <AutoLogon>
        <Password><Value>{pass}</Value><PlainText>true</PlainText></Password>
        <Username>{user}</Username>
        <Enabled>true</Enabled>
        <LogonCount>1</LogonCount>
      </AutoLogon>"#
            )
        } else {
            String::new()
        };

        let tpl = include_str!("../templates/autounattend.xml");
        tpl.replace("{arch}", arch)
            .replace("{pkt}", PUBLIC_KEY_TOKEN)
            .replace("{locale}", &locale)
            .replace("{user}", &user)
            .replace("{pass}", &pass)
            .replace("{tz}", &tz)
            .replace("{auto_logon_xml}", &auto_logon_xml)
    }

    // TODO: Use OxDock templating
    /// Render the guest SSH-proxy script (`templates/oxdock-ssh-proxy.oxdock`)
    /// with this config's credentials. The `{user}`/`{pass}` slots sit
    /// inside DSL double-quoted strings, so values are escaped for that
    /// syntax: backslash first (a lone `\x` escape drops the backslash),
    /// then `"`. The default password needs no escaping; custom values
    /// with quotes/backslashes stay intact through this.
    pub fn render_oxdock_proxy_script(&self) -> String {
        let tpl = include_str!("../templates/oxdock-ssh-proxy.oxdock");
        tpl.replace("{user}", &escape_oxdock_string(&self.username))
            .replace("{pass}", &escape_oxdock_string(&self.password))
    }
}

// TODO: Import from OxDock; no mirroring
/// Escape a value for a double-quoted OxDock DSL string: `\` doubles
/// (else the next char's escape would eat it), `"` gains a backslash.
/// Mirrors the DSL's `\"` / `\'` rule (any other `\x` keeps `x`, drops `\`).
pub fn escape_oxdock_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardened_markers_present_amd64() {
        let xml = VmConfig {
            arch: Architecture::Amd64,
            ..VmConfig::default()
        }
        .generate_unattend_xml();
        for marker in [
            "<InstallTo>",
            "<DiskID>0</DiskID>",
            "<MetaData",
            "Windows 11 Pro",
            // Single-partition media: Setup finds install.swm via
            // MetaData media search (proven). No InstallFrom Path —
            // USB letters are topology-dependent (a stale D: Path
            // failed live with 0x80070490 ELEMENT_NOT_FOUND).
            "<ProductKey>",
            "<Key></Key>",
            "diskpart /s",
            "BypassTPMCheck",
            "BypassSecureBootCheck",
            "BypassRAMCheck",
            "BypassCPUCheck",
            "BypassStorageCheck",
            "DisablePrivacyExperience",
            "BypassNRO",
            "HideOEMRegistrationScreen",
            // OxDock is the only SSH server: no OpenSSH zip, no
            // install-sshd.ps1, no sshd service anywhere in the file.
            "OxDock",
            "oxdock.exe",
            "ssh-proxy.oxdock",
            "schtasks",
            "0.0.0.0:22:ssh-server",
            "OxDock-SSH-In-TCP",
            "New-NetFirewallRule",
            // SSH must be reachable regardless of NLA's profile verdict
            // (the profile wait is EventLog's gpsvc stall).
            "-Profile Any",
            // gpsvc repair must ship: missing GPSvcGroup blocks EventLog
            // 300 s on the IUserProfile2 endpoint resolution (5 min boots).
            "GPSvcGroup",
            "AuthenticationCapabilities",
            // Recovery off from the first specialize boot: a hard stop
            // (Ctrl-C) must never surface "Diagnosing your PC" — the
            // post-install SSH step only applies once Setup hits Ready.
            "bootstatuspolicy IgnoreAllFailures",
            "recoveryenabled No",
            // Headless diet lives post-READY (one SSH call), not here:
            // firewall off only (slirp NAT gates inbound; the firewall sat
            // on port 22 ~27s after the SSH server bound — measured).
            "netsh advfirewall set allprofiles state off",
            "NetworkLocation",
            "TimeZone",
        ] {
            assert!(xml.contains(marker), "missing {marker}");
        }
        for absent in [
            "OpenSSH",
            "install-sshd.ps1",
            "sshd",
            "{sshzip}",
            "{sshdir}",
        ] {
            assert!(!xml.contains(absent), "OpenSSH leftover: {absent}");
        }
        assert!(xml.contains(r#"processorArchitecture="amd64""#));
        assert!(!xml.contains("<DiskConfiguration>"));
        // No powershell in windowsPE (absent from WinPE); specialize may use it.
        let pe = xml
            .split("<settings pass=\"windowsPE\">")
            .nth(1)
            .unwrap()
            .split("</settings>")
            .next()
            .unwrap();
        assert!(!pe.contains("powershell.exe"));
        // Headless default: no auto-logon; callers opt in explicitly.
        assert!(!xml.contains("<AutoLogon>"));
    }

    #[test]
    fn run_synchronous_orders_are_consecutive_from_one() {
        // A gap (orders starting at 2) fails Setup with 0x800700D
        // ERROR_INVALID_DATA (observed live). NB: DiskConfiguration
        // partitions also use <Order>; only RunSynchronous counts here.
        // Both command passes are checked: specialize carries the gpsvc
        // repair, the OxDock firewall commands, the recovery policy, and
        // firewall-off (orders 1..=13; pre-SSH trim is 14..=16, the rest
        // of the service diet moved post-READY).
        for arch in [Architecture::Amd64, Architecture::Arm64] {
            let xml = VmConfig {
                arch,
                ..VmConfig::default()
            }
            .generate_unattend_xml();
            for pass in ["windowsPE", "specialize"] {
                let section = format!("<settings pass=\"{pass}\">");
                let rs = xml
                    .split(&section)
                    .nth(1)
                    .unwrap()
                    .split("</settings>")
                    .next()
                    .unwrap()
                    .split("<RunSynchronous>")
                    .nth(1)
                    .unwrap()
                    .split("</RunSynchronous>")
                    .next()
                    .unwrap();
                let orders: Vec<u32> = rs
                    .split("<Order>")
                    .skip(1)
                    .map(|s| s.split("</Order>").next().unwrap().parse().unwrap())
                    .collect();
                assert_eq!(
                    orders,
                    (1..=orders.len() as u32).collect::<Vec<_>>(),
                    "gap in {arch:?} pass {pass}: {orders:?}"
                );
            }
        }
    }

    #[test]
    fn run_synchronous_paths_stay_short() {
        // Setup validates every <Path> with WCM: a long value comes back
        // as WCM_E_INVALIDVALUE 0x80220005 "Value is invalid", which fails
        // the WHOLE answer file (windeploy 0x1f -> the "computer restarted
        // unexpectedly" loop). Live 2026-10-04: an 854-char PowerShell Path
        // was rejected while every command up to 243 chars passed, so keep
        // the proven bound with margin: nothing over 256.
        for arch in [Architecture::Amd64, Architecture::Arm64] {
            let xml = VmConfig {
                arch,
                ..VmConfig::default()
            }
            .generate_unattend_xml();
            for pass in ["windowsPE", "specialize"] {
                let section = format!("<settings pass=\"{pass}\">");
                let rs = xml
                    .split(&section)
                    .nth(1)
                    .unwrap()
                    .split("</settings>")
                    .next()
                    .unwrap()
                    .split("<RunSynchronous>")
                    .nth(1)
                    .unwrap()
                    .split("</RunSynchronous>")
                    .next()
                    .unwrap();
                let mut paths = rs
                    .split("<Path>")
                    .skip(1)
                    .map(|s| s.split("</Path>").next().unwrap());
                for path in &mut paths {
                    assert!(
                        path.len() <= 256,
                        "{arch:?} pass {pass}: Path is {} chars (limit 256): {path}",
                        path.len()
                    );
                }
            }
        }
    }

    #[test]
    fn arm64_and_drive_and_autologon() {
        // NVMe topology: no third-party storage drivers in the file;
        // part.txt selects Disk 0, InstallTo targets Disk 0.
        let cfg = VmConfig {
            arch: Architecture::Arm64,
            auto_logon: true,
            ..VmConfig::default()
        };
        let xml = cfg.generate_unattend_xml();
        assert!(xml.contains(r#"processorArchitecture="arm64""#));
        assert!(xml.contains("<DiskID>0</DiskID>"));
        assert!(!xml.contains("viostor"));
        assert!(!xml.contains("offlineServicing"));
        assert!(xml.contains("<AutoLogon>"));
        // SSH slot is OxDock, never OpenSSH: no zip/dir placeholders leak.
        assert!(xml.contains("oxdock.exe"));
        assert!(xml.contains("ssh-proxy.oxdock"));
        assert!(!xml.contains("OpenSSH"));
        assert!(!xml.contains("sshd"));
        assert!(!xml.contains("{sshzip}"));
        assert!(!xml.contains("{sshdir}"));
        assert!(!xml.contains("Win64"));
    }

    #[test]
    fn oxdock_asset_names_follow_arch() {
        for (arch, asset) in [
            (Architecture::Amd64, "oxdock-x86_64-pc-windows-msvc.tar.gz"),
            (Architecture::Arm64, "oxdock-aarch64-pc-windows-msvc.tar.gz"),
        ] {
            assert_eq!(arch.oxdock_asset_name(), asset);
        }
    }

    #[test]
    fn oxdock_hashes_are_arch_strict() {
        // Strict per-arch pins: distinct, 64 lowercase hex each, matching
        // the constants in pins.rs (bump rule: tag change updates both).
        let x64 = Architecture::Amd64.oxdock_sha256();
        let arm64 = Architecture::Arm64.oxdock_sha256();
        assert_eq!(x64, crate::pins::OXDOCK_SHA256_WIN64);
        assert_eq!(arm64, crate::pins::OXDOCK_SHA256_ARM64);
        assert_ne!(x64, arm64);
        for h in [x64, arm64] {
            assert_eq!(h.len(), 64, "not 64 hex chars: {h}");
            assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "not hex: {h}");
        }
    }

    #[test]
    fn oxdock_proxy_script_renders_credentials() {
        let cfg = VmConfig::default();
        let script = cfg.render_oxdock_proxy_script();
        assert!(script.contains(&format!("username: \"{}\"", cfg.username)));
        assert!(script.contains(&format!("password: \"{}\"", cfg.password)));
        assert!(!script.contains("{user}"));
        assert!(!script.contains("{pass}"));
        // Proto structure survived the adaptation: admission control,
        // exec/shell branching, ConPTY runners, audit lines.
        for marker in [
            "SSH_SERVE(",
            "SEMAPHORE_NEW",
            "SSH_DEQUEUE",
            "SSH_PTY_RUN",
            "SSH_PUMP_CHANNEL",
            "cmd.exe",
            "powershell.exe",
            "[AUDIT]",
        ] {
            assert!(script.contains(marker), "missing {marker}");
        }
        assert!(!script.contains("orb"));
    }

    #[test]
    fn oxdock_string_escaping_round_trips() {
        assert_eq!(escape_oxdock_string("plain"), "plain");
        assert_eq!(escape_oxdock_string("a\"b"), "a\\\"b");
        assert_eq!(escape_oxdock_string("a\\b"), "a\\\\b");
        // Rendered credentials with specials stay inside one DSL string.
        let cfg = VmConfig {
            username: "AdminUser".to_string(),
            password: "P@ss\"\\word".to_string(),
            ..VmConfig::default()
        };
        let script = cfg.render_oxdock_proxy_script();
        assert!(script.contains("password: \"P@ss\\\"\\\\word\""));
    }

    #[test]
    fn no_double_hyphen_in_comments() {
        // `--` inside <!-- --> is illegal XML: expat/MSXML reject the whole
        // file while quick-xml tolerates it (this exact defect shipped in an
        // answer file that Setup reset on). Check comment bodies strictly.
        let xml = VmConfig::default().generate_unattend_xml();
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Comment(e)) => {
                    let s: &str = e.as_ref();
                    assert!(!s.contains("--"), "illegal -- in comment: {s}");
                }
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("malformed XML: {e}"),
            }
            buf.clear();
        }
    }

    #[test]
    fn escaping_round_trips() {
        let cfg = VmConfig {
            username: "a<b".to_string(),
            password: "P@ss<>&\"".to_string(),
            ..VmConfig::default()
        };
        let xml = cfg.generate_unattend_xml();
        assert!(xml.contains("a&lt;b"));
        assert!(xml.contains("P@ss&lt;&gt;&amp;&quot;"));
        // Well-formedness spot check via quick-xml event reader.
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut buf = Vec::new();
        let mut count = 0;
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {
                    count += 1;
                    buf.clear();
                }
                Err(e) => panic!("malformed XML: {e}"),
            }
        }
        assert!(count > 50);
    }
}
