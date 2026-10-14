//! Build provenance sidecar: `build-media` writes `<out>.json` next to the
//! installer image, capturing everything needed to reproduce or audit the
//! build — source ISO identity, host specs, portable dependency pins, and
//! answer-file parameters (never secrets: no passwords, keys, or tokens).
//!
//! The sibling `install` success path appends one line per proven
//! run to the committed `tested-isos.jsonl` log (see [`TestRecord`]).
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Everything `build-media` records about one installer image build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    /// This crate's version (`CARGO_PKG_VERSION` at compile time).
    pub builder_version: String,
    /// Git commit the binary was built from (`BUILD_GIT_SHA` via build.rs;
    /// `"unknown"` for tarball builds without `.git`). This is the field
    /// that distinguishes debug runs — the version only changes on bump.
    pub build_commit: String,
    /// Uncommitted changes present at compile time (`BUILD_GIT_DIRTY`).
    /// `true` means the commit hash alone does NOT reproduce the binary.
    pub build_dirty: bool,
    /// UTC build timestamp (RFC 3339-ish, no chrono dep: seconds epoch).
    pub built_at_epoch: u64,
    pub source_iso: IsoIdentity,
    pub host: HostSpecs,
    pub deps: DependencyPins,
    pub guest: GuestParams,
}

/// Identity of the user-supplied Eval ISO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsoIdentity {
    pub file_name: String,
    pub size_bytes: u64,
    /// SHA-256 hex of the full ISO (a ~30 s pass on 7 GB; the dedup key).
    pub sha256: String,
    /// ISO9660 PVD volume creation stamp, as printed in the descriptor.
    /// Best-effort: `None` on UDF-only images or parse failure.
    pub pvd_creation: Option<String>,
    /// `install.wim` image names from `wimlib-imagex info` (best-effort).
    pub wim_images: Vec<String>,
}

/// The machine that ran the build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSpecs {
    pub os: String,
    /// `sw_vers -productVersion` on macOS, `PRETTY_NAME` from
    /// `/etc/os-release` on Linux, else `"unknown"`.
    pub os_version: String,
    pub arch: String,
}

/// Every external input that isn't the ISO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyPins {
    /// Full portable release tag (`qemu-v11.1.1-57`), never a bare
    /// version — rebuild tags mean a version no longer names a unique
    /// build. Renamed from `qemu_version`; the alias keeps old log lines
    /// parsing.
    #[serde(alias = "qemu_version")]
    pub qemu_tag: String,
    /// Full portable release tag (`wimlib-v1.14.5-27`), never a bare
    /// version — rebuild tags mean a version no longer names a unique
    /// build. Renamed from `wimlib_version`; the alias keeps old log
    /// lines parsing.
    #[serde(alias = "wimlib_version")]
    pub wimlib_tag: String,
    pub virtio_iso_name: String,
    pub virtio_sha256: Option<String>,
    /// Guest SSH server release tarball (per guest arch), from
    /// <https://github.com/jzombie/rust-oxdock>. The guest runs no
    /// OpenSSH at all — this is the only SSH software in the pipeline.
    /// `#[serde(default)]` keeps pre-OxDock log lines parsing.
    #[serde(default)]
    pub oxdock_asset: String,
    #[serde(default)]
    pub oxdock_release_tag: String,
    /// SHA-256 of the exact `oxdock.exe` bytes staged onto the installer
    /// USB (extracted from the tarball above).
    #[serde(default)]
    pub oxdock_exe_sha256: String,
}

/// Answer-file parameters (usernames/locales only — never the password).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestParams {
    pub arch: String,
    pub username: String,
    pub locale: String,
    /// QEMU accelerator actually used (`hvf`/`kvm`/`tcg`): recorded so the
    /// tested table can say whether TCG was ever exercised. Resolved at
    /// gather time via [`crate::unattend::resolve_accel`], same probe as boot.
    /// Defaults to `"unknown"` for log lines written before this field existed.
    #[serde(default = "unknown_accel")]
    pub accel: String,
}

/// Default for log lines predating the `accel` field.
fn unknown_accel() -> String {
    "unknown".to_string()
}

/// One line in the committed `tested-isos.jsonl` log: ISO identity plus
/// the install outcome. Written by `install` on SSH-banner success.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRecord {
    pub tested_at_epoch: u64,
    pub outcome: String,
    pub ssh_banner: Option<String>,
    pub provenance: Provenance,
}

impl TestRecord {
    /// Committed log filename (repo root, current dir at runtime).
    pub const LOG: &str = "tested-isos.jsonl";

    /// Append one line to [`TestRecord::LOG`] in the current directory
    /// (repo root in practice). Creates the file on first use.
    pub fn append(provenance: Provenance, banner: &str) -> Result<PathBuf> {
        let record = Self {
            tested_at_epoch: now_epoch(),
            outcome: "ssh-banner".to_string(),
            ssh_banner: Some(banner.trim().to_string()),
            provenance,
        };
        let path = PathBuf::from(Self::LOG);
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?
            .write_all(line.as_bytes())?;
        eprintln!("proven: {}", path.display());
        Ok(path)
    }

    /// Load the sidecar written by `build-media` for this image.
    pub fn load_sidecar(image: &Path) -> Result<Provenance> {
        let path = Provenance::sidecar_path(image);
        let json = std::fs::read_to_string(&path)
            .with_context(|| format!("read sidecar {}", path.display()))?;
        serde_json::from_str(&json).with_context(|| format!("parse sidecar {}", path.display()))
    }
}

pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// SHA-256 hex of a file, streamed (never whole in RAM).
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("open {} for hashing", path.display()))?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// ISO9660 PVD volume-creation stamp: sector 16, offset 40, 17 ASCII bytes
/// (`YYYYMMDDHHMMSScc`) + 1 offset byte. Returns the raw 17-char stamp.
/// `None` when the sector isn't a PVD (UDF-only image) — not an error.
pub fn pvd_creation_stamp(iso: &Path) -> Result<Option<String>> {
    let mut f = std::fs::File::open(iso)?;
    f.seek(SeekFrom::Start(16 * 2048))?;
    let mut sector = vec![0u8; 2048];
    f.read_exact(&mut sector)?;
    if sector[0] != 1 || &sector[1..6] != b"CD001" {
        return Ok(None);
    }
    let raw = &sector[40..57];
    if !raw.iter().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(raw).into_owned()))
}

/// `install.wim` image names via `wimlib-imagex info` (best-effort:
/// empty vec on any failure — provenance must never fail the build).
pub fn wim_image_names(wimlib_bin: &Path, wim_path: &Path) -> Vec<String> {
    let out = std::process::Command::new(wimlib_bin)
        .args(["info", &wim_path.to_string_lossy()])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // `wimlib-imagex info` prints `Index: N` + `Name: "..."` per image.
    text.lines()
        .filter_map(|l| {
            let (_, name) = l.trim_start().split_once("Name:")?;
            Some(name.trim().trim_matches('"').to_string())
        })
        .filter(|n| !n.is_empty())
        .collect()
}

fn host_specs() -> HostSpecs {
    let os_version = match std::env::consts::OS {
        "macos" => std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
        "linux" => linux_pretty_name(),
        _ => "unknown".to_string(),
    };
    HostSpecs {
        os: std::env::consts::OS.to_string(),
        os_version,
        arch: std::env::consts::ARCH.to_string(),
    }
}

/// Distribution name from `/etc/os-release`'s `PRETTY_NAME`
/// (e.g. `Ubuntu 24.04.1 LTS`); `"unknown"` when the file or the key
/// is missing. Parsing lives in [`os_release_pretty_name`] so it can
/// be tested without the file.
fn linux_pretty_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|c| os_release_pretty_name(&c))
        .unwrap_or_else(|| "unknown".to_string())
}

fn os_release_pretty_name(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let value = line.strip_prefix("PRETTY_NAME=")?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.to_string())
    })
}

impl Provenance {
    /// Gather provenance for a `build-media` run. `wim_path` is the
    /// extracted `install.wim` (queried via `wimlib-imagex info`, then the
    /// caller deletes it). Slow steps (ISO hash) print progress.
    /// Best-effort sub-steps degrade to `None`/empty — only hard I/O
    /// errors propagate. Ten params is the honest shape (every input is
    /// recorded); grouping them would hide what the sidecar captures.
    #[allow(clippy::too_many_arguments)]
    pub fn gather(
        win_iso: &Path,
        wimlib_bin: &Path,
        wim_path: &Path,
        arch: crate::unattend::Architecture,
        qemu_tag: &str,
        wimlib_tag: &str,
        virtio_iso: Option<&Path>,
        username: &str,
        locale: &str,
        oxdock_exe_sha256: &str,
    ) -> Result<Self> {
        let meta = std::fs::metadata(win_iso)?;
        let file_name = win_iso
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| win_iso.display().to_string());
        eprintln!("media: hashing source ISO (one pass, ~30 s) ...");
        let sha256 = sha256_file(win_iso)?;
        let pvd_creation = pvd_creation_stamp(win_iso).unwrap_or(None);
        let wim_images = wim_image_names(wimlib_bin, wim_path);
        let (virtio_iso_name, virtio_sha256) = match virtio_iso {
            Some(p) => (
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                sha256_file(p).ok(),
            ),
            None => ("<drivers-dir>".to_string(), None),
        };
        Ok(Self {
            builder_version: env!("CARGO_PKG_VERSION").to_string(),
            build_commit: option_env!("BUILD_GIT_SHA")
                .unwrap_or("unknown")
                .to_string(),
            build_dirty: option_env!("BUILD_GIT_DIRTY").is_some_and(|v| v == "true"),
            built_at_epoch: now_epoch(),
            source_iso: IsoIdentity {
                file_name,
                size_bytes: meta.len(),
                sha256,
                pvd_creation,
                wim_images,
            },
            host: host_specs(),
            deps: DependencyPins {
                qemu_tag: qemu_tag.to_string(),
                wimlib_tag: wimlib_tag.to_string(),
                virtio_iso_name,
                virtio_sha256,
                oxdock_asset: arch.oxdock_asset_name().to_string(),
                oxdock_release_tag: crate::pins::OXDOCK_RELEASE_TAG.to_string(),
                oxdock_exe_sha256: oxdock_exe_sha256.to_string(),
            },
            guest: GuestParams {
                arch: arch.as_xml_str().to_string(),
                username: username.to_string(),
                locale: locale.to_string(),
                accel: crate::unattend::resolve_accel().to_string(),
            },
        })
    }

    /// `<image>.json` next to the installer image.
    pub fn sidecar_path(image: &Path) -> PathBuf {
        let mut s = image.as_os_str().to_owned();
        s.push(".json");
        PathBuf::from(s)
    }

    /// Invalidate a previous build: delete its sidecar so a crashed/failed
    /// rebuild can never report "fresh" over a partial image. Runs at build
    /// start (after the freshness skip); missing file is fine.
    pub fn invalidate(image: &Path) {
        let _ = std::fs::remove_file(Self::sidecar_path(image));
    }

    pub fn write_sidecar(&self, image: &Path) -> Result<PathBuf> {
        let path = Self::sidecar_path(image);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        eprintln!("media: provenance {}", path.display());
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_pvd(stamp: &[u8; 17]) -> Vec<u8> {
        // 16 empty sectors + PVD with "CD001" + stamp at offset 40.
        let mut img = vec![0u8; 17 * 2048];
        img[16 * 2048] = 1;
        img[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
        img[16 * 2048 + 40..16 * 2048 + 57].copy_from_slice(stamp);
        img
    }

    #[test]
    fn pvd_stamp_parses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fake.iso");
        std::fs::write(&p, synthetic_pvd(b"20260922093000000")).unwrap();
        assert_eq!(
            pvd_creation_stamp(&p).unwrap(),
            Some("20260922093000000".to_string())
        );
    }

    #[test]
    fn pvd_absent_on_non_iso9660() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fake.iso");
        std::fs::write(&p, vec![0u8; 17 * 2048]).unwrap();
        assert_eq!(pvd_creation_stamp(&p).unwrap(), None);
    }

    #[test]
    fn os_release_pretty_name_parses() {
        let s = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 26.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(
            os_release_pretty_name(s),
            Some("Ubuntu 26.04.1 LTS".to_string())
        );
        // Unquoted values are legal per os-release(5).
        assert_eq!(
            os_release_pretty_name("PRETTY_NAME=Alpine Linux v3.20"),
            Some("Alpine Linux v3.20".to_string())
        );
    }

    #[test]
    fn os_release_pretty_name_missing_is_none() {
        assert_eq!(os_release_pretty_name("ID=ubuntu\n"), None);
        assert_eq!(os_release_pretty_name("PRETTY_NAME=\n"), None);
    }

    #[test]
    fn provenance_json_roundtrip() {
        let prov = Provenance {
            builder_version: "0.1.0-alpha".to_string(),
            build_commit: "abc123def456".to_string(),
            build_dirty: true,
            built_at_epoch: 1_758_000_000,
            source_iso: IsoIdentity {
                file_name: "Win11.iso".to_string(),
                size_bytes: 7_000_000_000,
                sha256: "abc".to_string(),
                pvd_creation: Some("20260922093000000".to_string()),
                wim_images: vec!["Windows 11 Pro".to_string()],
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
                virtio_sha256: Some("def".to_string()),
                oxdock_asset: "oxdock-aarch64-pc-windows-msvc.tar.gz".to_string(),
                oxdock_release_tag: crate::pins::OXDOCK_RELEASE_TAG.to_string(),
                oxdock_exe_sha256: "abc123".to_string(),
            },
            guest: GuestParams {
                arch: "arm64".to_string(),
                username: "AdminUser".to_string(),
                locale: "en-US".to_string(),
                accel: "hvf".to_string(),
            },
        };
        let json = serde_json::to_string(&prov).unwrap();
        assert!(!json.contains("P@ss"), "no secrets in provenance");
        let back: Provenance = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source_iso.file_name, "Win11.iso");
        assert_eq!(back.deps.qemu_tag, "qemu-v11.1.1");
        assert_eq!(back.deps.wimlib_tag, "wimlib-v1.14.5-27");
        // Old log lines used `qemu_version`/`wimlib_version` — the aliases
        // keep them parsing.
        let old = json
            .replace("qemu_tag", "qemu_version")
            .replace("wimlib_tag", "wimlib_version");
        let back_old: Provenance = serde_json::from_str(&old).unwrap();
        assert_eq!(back_old.deps.qemu_tag, "qemu-v11.1.1");
        assert_eq!(back_old.deps.wimlib_tag, "wimlib-v1.14.5-27");
    }
}
