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

    /// Win32-OpenSSH release zip for this guest arch
    /// (`OpenSSH-Win64.zip` vs `OpenSSH-ARM64.zip`; both extract to a
    /// same-named subdir holding `install-sshd.ps1`).
    pub fn openssh_zip_name(&self) -> &'static str {
        match self {
            Architecture::Amd64 => "OpenSSH-Win64.zip",
            Architecture::Arm64 => "OpenSSH-ARM64.zip",
        }
    }

    /// Expected SHA-256 of [`Self::openssh_zip_name`] at
    /// [`crate::pins::OPENSSH_RELEASE_TAG`]. Strict per-arch mapping — no
    /// fallback, no caller-side `if`: the wrong hash for the wrong arch is
    /// a guaranteed mismatch, which is exactly what we want.
    pub fn openssh_sha256(&self) -> &'static str {
        match self {
            Architecture::Amd64 => crate::pins::OPENSSH_SHA256_WIN64,
            Architecture::Arm64 => crate::pins::OPENSSH_SHA256_ARM64,
        }
    }

    pub fn openssh_extract_dir(&self) -> &'static str {
        match self {
            Architecture::Amd64 => "OpenSSH-Win64",
            Architecture::Arm64 => "OpenSSH-ARM64",
        }
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
            .replace("{sshzip}", self.arch.openssh_zip_name())
            .replace("{sshdir}", self.arch.openssh_extract_dir())
            .replace("{pkt}", PUBLIC_KEY_TOKEN)
            .replace("{locale}", &locale)
            .replace("{user}", &user)
            .replace("{pass}", &pass)
            .replace("{tz}", &tz)
            .replace("{auto_logon_xml}", &auto_logon_xml)
    }
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
            "install-sshd.ps1",
            "OpenSSH-Win64",
            "OpenSSH-Win64.zip",
            "New-NetFirewallRule",
            "NetworkLocation",
            "TimeZone",
        ] {
            assert!(xml.contains(marker), "missing {marker}");
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
        for arch in [Architecture::Amd64, Architecture::Arm64] {
            let xml = VmConfig {
                arch,
                ..VmConfig::default()
            }
            .generate_unattend_xml();
            let pe = xml
                .split("<settings pass=\"windowsPE\">")
                .nth(1)
                .unwrap()
                .split("</settings>")
                .next()
                .unwrap();
            let rs = pe
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
                "gap in {arch:?}: {orders:?}"
            );
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
        // Arch-parameterized inputs: no placeholders leak, names match arch.
        assert!(xml.contains("OpenSSH-ARM64.zip"));
        assert!(xml.contains(r#"OpenSSH\OpenSSH-ARM64"#));
        assert!(!xml.contains("{sshzip}"));
        assert!(!xml.contains("{sshdir}"));
        assert!(!xml.contains("Win64"));
    }

    #[test]
    fn openssh_names_follow_arch() {
        for (arch, zip, dir) in [
            (Architecture::Amd64, "OpenSSH-Win64.zip", "OpenSSH-Win64"),
            (Architecture::Arm64, "OpenSSH-ARM64.zip", "OpenSSH-ARM64"),
        ] {
            let xml = VmConfig {
                arch,
                ..VmConfig::default()
            }
            .generate_unattend_xml();
            assert!(xml.contains(zip), "missing {zip} for {arch:?}");
            assert!(xml.contains(dir), "missing {dir} for {arch:?}");
        }
    }

    #[test]
    fn openssh_hashes_are_arch_strict() {
        // Strict per-arch pins: distinct, 64 lowercase hex each, matching
        // the constants in pins.rs (bump rule: tag change updates both).
        let win64 = Architecture::Amd64.openssh_sha256();
        let arm64 = Architecture::Arm64.openssh_sha256();
        assert_eq!(win64, crate::pins::OPENSSH_SHA256_WIN64);
        assert_eq!(arm64, crate::pins::OPENSSH_SHA256_ARM64);
        assert_ne!(win64, arm64);
        for h in [win64, arm64] {
            assert_eq!(h.len(), 64, "not 64 hex chars: {h}");
            assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "not hex: {h}");
        }
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
