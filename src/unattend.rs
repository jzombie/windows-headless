use quick_xml::escape::escape;
use std::env::consts;

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
    /// Single `LogonCount 1` auto-logon; default false (SSH needs no desktop).
    pub auto_logon: bool,
}

impl Default for VmConfig {
    fn default() -> Self {
        let (arch, _) = auto_detect_platform();
        Self {
            username: "AdminUser".to_string(),
            password: "P@ssw0rd123!".to_string(),
            locale: "en-US".to_string(),
            timezone: "Pacific Standard Time".to_string(),
            arch,
            auto_logon: false,
        }
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
    let accel = match consts::OS {
        "linux" => "kvm",
        "macos" => "hvf",
        _ => "tcg",
    };
    (arch, accel)
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

        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE" processorArchitecture="{arch}" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <SetupUILanguage><UILanguage>{locale}</UILanguage></SetupUILanguage>
      <InputLocale>{locale}</InputLocale>
      <SystemLocale>{locale}</SystemLocale>
      <UILanguage>{locale}</UILanguage>
      <UserLocale>{locale}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Setup" processorArchitecture="{arch}" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <DriverPaths>
        <PathAndCredentials wcm:action="add" wcm:keyValue="1"><Path>C:\drivers\viostor\w11\{arch}</Path></PathAndCredentials>
        <PathAndCredentials wcm:action="add" wcm:keyValue="2"><Path>C:\drivers\NetKVM\w11\{arch}</Path></PathAndCredentials>
      </DriverPaths>
      <RunSynchronous>
        <!-- Driver load and PnP enumeration race Setup, so BOTH steps retry
             in a loop (idempotent: reinstall is a no-op, re-wipe/recreate
             converges once the disk appears). Single-USB design keeps paths
             to C:/D:; every step guarded so a total miss degrades to the
             interactive prompt, never a hard fail. -->
        <RunSynchronousCommand wcm:action="add"><Order>1</Order><Path>cmd /c (for /L %i in (1,1,12) do @(if exist C:\drivers\viostor pnputil /add-driver C:\drivers\viostor\w11\{arch}\viostor.inf /install &amp; if exist D:\drivers\viostor pnputil /add-driver D:\drivers\viostor\w11\{arch}\viostor.inf /install &amp; timeout /t 10) &amp; exit 0)</Path><Description>Install viostor storage driver (retry)</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>2</Order><Path>cmd /c (for /L %i in (1,1,12) do @(diskpart /s C:\setup\part.txt &amp; diskpart /s D:\setup\part.txt &amp; timeout /t 10) &amp; exit 0)</Path><Description>Partition target disk (retry)</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>3</Order><Path>cmd /c reg add "HKLM\SYSTEM\Setup\LabConfig" /v BypassTPMCheck /t REG_DWORD /d 1 /f</Path><Description>Bypass TPM 2.0 Check</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>4</Order><Path>cmd /c reg add "HKLM\SYSTEM\Setup\LabConfig" /v BypassSecureBootCheck /t REG_DWORD /d 1 /f</Path><Description>Bypass Secure Boot Check</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>5</Order><Path>cmd /c reg add "HKLM\SYSTEM\Setup\LabConfig" /v BypassRAMCheck /t REG_DWORD /d 1 /f</Path><Description>Bypass RAM Size Check</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>6</Order><Path>cmd /c reg add "HKLM\SYSTEM\Setup\LabConfig" /v BypassCPUCheck /t REG_DWORD /d 1 /f</Path><Description>Bypass CPU Architecture Check</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>7</Order><Path>cmd /c reg add "HKLM\SYSTEM\Setup\LabConfig" /v BypassStorageCheck /t REG_DWORD /d 1 /f</Path><Description>Bypass Minimum Disk Size Check</Description></RunSynchronousCommand>
      </RunSynchronous>
      <ImageInstall><OSImage><InstallFrom><MetaData wcm:action="add"><Key>/IMAGE/NAME</Key><Value>Windows 11 Pro</Value></MetaData></InstallFrom><InstallTo><DiskID>1</DiskID><PartitionID>3</PartitionID></InstallTo><WillShowUI>OnError</WillShowUI></OSImage></ImageInstall>
      <UserData><ProductKey><Key></Key></ProductKey><AcceptEula>true</AcceptEula></UserData>
    </component>
  </settings>
  <settings pass="specialize">
    <component name="Microsoft-Windows-Deployment" processorArchitecture="{arch}" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <RunSynchronous>
        <RunSynchronousCommand wcm:action="add"><Order>1</Order><Path>powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0"</Path><Description>Install OpenSSH Server</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>2</Order><Path>powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "Set-Service -Name sshd -StartupType Automatic; Start-Service -Name sshd"</Path><Description>Configure SSHD Automatic Startup</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>3</Order><Path>powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (Inbound)' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22"</Path><Description>Open Firewall Port 22</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>4</Order><Path>cmd /c reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE" /v BypassNRO /t REG_DWORD /d 1 /f</Path><Description>Bypass OOBE Network Requirement</Description></RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add"><Order>5</Order><Path>cmd /c reg add "HKLM\SOFTWARE\Policies\Microsoft\Windows\OOBE" /v DisablePrivacyExperience /t REG_DWORD /d 1 /f</Path><Description>Disable OOBE Privacy Experience Page</Description></RunSynchronousCommand>
      </RunSynchronous>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="{arch}" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <UserAccounts><LocalAccounts><LocalAccount wcm:action="add"><Name>{user}</Name><Group>Administrators</Group><DisplayName>Headless Admin</DisplayName><Password><Value>{pass}</Value><PlainText>true</PlainText></Password></LocalAccount></LocalAccounts></UserAccounts>
      <OOBE><HideEULAPage>true</HideEULAPage><HideLocalAccountScreen>true</HideLocalAccountScreen><HideOEMRegistrationScreen>true</HideOEMRegistrationScreen><HideOnlineAccountScreens>true</HideOnlineAccountScreens><HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE><NetworkLocation>Work</NetworkLocation><ProtectYourPC>3</ProtectYourPC></OOBE>{auto_logon_xml}
      <TimeZone>{tz}</TimeZone>
    </component>
  </settings>
</unattend>"#
        )
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
            "<DiskID>1</DiskID>",
            "<MetaData",
            "Windows 11 Pro",
            "<ProductKey><Key></Key></ProductKey>",
            "DriverPaths",
            "NetKVM",
            "viostor",
            "pnputil",
            "diskpart /s",
            "BypassTPMCheck",
            "BypassSecureBootCheck",
            "BypassRAMCheck",
            "BypassCPUCheck",
            "BypassStorageCheck",
            "DisablePrivacyExperience",
            "BypassNRO",
            "HideOEMRegistrationScreen",
            "NetworkLocation",
            "New-NetFirewallRule",
            "TimeZone",
        ] {
            assert!(xml.contains(marker), "missing {marker}");
        }
        assert!(xml.contains(r#"processorArchitecture="amd64""#));
        assert!(!xml.contains("<DiskConfiguration>"));
        // No powershell in windowsPE (absent from WinPE); specialize may use it.
        assert!(!xml.contains("powershell.exe -NoProfile -ExecutionPolicy Bypass -Command \"for "));
        assert!(
            !xml.contains("<AutoLogon>"),
            "autologon must be off by default"
        );
    }

    #[test]
    fn arm64_and_drive_and_autologon() {
        // Single-USB design: the install stick is always C:, so the drive
        // letter is no longer configurable.
        let cfg = VmConfig {
            arch: Architecture::Arm64,
            auto_logon: true,
            ..VmConfig::default()
        };
        let xml = cfg.generate_unattend_xml();
        assert!(xml.contains(r#"processorArchitecture="arm64""#));
        assert!(xml.contains(r"C:\drivers\NetKVM\w11\arm64"));
        assert!(xml.contains(r"C:\drivers\viostor\w11\arm64"));
        assert!(xml.contains("<AutoLogon>"));
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
