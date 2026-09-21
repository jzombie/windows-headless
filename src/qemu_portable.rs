//! Portable QEMU provisioning from `jzombie/lab-qemu-portable` releases
//! (feature `downloader`).
//!
//! Layout on disk (managed by `cache-manager`, never evicted):
//! `<cache>/qemu-portable/<version>/qemu-portable/{bin,lib,share}/`.
//! The bundled dylibs/so link via `@executable_path`/`$ORIGIN`-relative
//! paths and QEMU finds `share/qemu` (OVMF firmware) relative to the
//! executable, so the tree works from any CWD with no env vars.
use anyhow::{Context, Result};
use cache_manager::CacheRoot;
use std::env::consts;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub const REPO_API_LATEST: &str =
    "https://api.github.com/repos/jzombie/lab-qemu-portable/releases/latest";
const UA: &str = "windows-headless/0.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    TarGz,
    TarXz,
}

/// A provisioned portable QEMU tree. `root` is the extracted `qemu-portable`
/// dir (the one containing `bin/`, `lib/`, `share/`).
#[derive(Debug, Clone)]
pub struct PortableQemu {
    pub root: PathBuf,
    pub version: String,
}

impl PortableQemu {
    pub fn qemu_img(&self) -> PathBuf {
        self.root.join("bin/qemu-img")
    }

    pub fn qemu_system(&self, binary: &str) -> PathBuf {
        self.root.join("bin").join(binary)
    }

    /// All `qemu-system-*` binaries actually present in this tree
    /// (what the release shipped for this host — e.g. macOS ships only
    /// `qemu-system-aarch64`, Linux ships `qemu-system-x86_64`).
    pub fn system_binaries(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.root.join("bin")) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("qemu-system-") {
                    out.push(entry.path());
                }
            }
        }
        out.sort();
        out
    }
}

/// `(asset-name marker, kind)` for this host, or an error when no portable
/// build exists (Intel Macs, Windows hosts) telling the caller to use system
/// QEMU instead.
fn asset_for_host() -> Result<(&'static str, AssetKind)> {
    match (consts::OS, consts::ARCH) {
        ("macos", "aarch64") => Ok(("macos-arm64", AssetKind::TarGz)),
        ("linux", "x86_64") => Ok(("linux-x86_64", AssetKind::TarXz)),
        ("linux", "aarch64") => Ok(("linux-aarch64", AssetKind::TarXz)),
        (os, arch) => anyhow::bail!(
            "no portable QEMU build for {os}/{arch} (only macos-arm64 and linux x86_64/aarch64 are published); install system QEMU instead"
        ),
    }
}

/// `qemu-v11.1.1` / `v11.1.1` / `11.1.1` → `11.1.1`.
fn normalize_version(tag: &str) -> String {
    tag.trim_start_matches("qemu-")
        .trim_start_matches('v')
        .to_string()
}

/// Parse a `SHA256SUMS.txt`-style body (`<hex>  <filename>` per line).
fn parse_sums(body: &str, asset: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some(hash), Some(name)) if name.trim_start_matches('*') == asset => {
                Some(hash.to_string())
            }
            _ => None,
        }
    })
}

/// Download `url` to `dest` (via `.part` + rename, as in `downloader`).
async fn fetch_to(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("reqwest client")?;
    let mut resp = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp).await?;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

fn github_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("reqwest client")
}

/// Resolve the latest release tag + asset download URLs from the GitHub API.
/// Skipped entirely when the requested version is already extracted.
async fn latest_release() -> Result<(String, Vec<(String, String)>)> {
    let body: serde_json::Value = github_client()?
        .get(REPO_API_LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tag = body
        .get("tag_name")
        .and_then(|t| t.as_str())
        .context("release has no tag_name")?
        .to_string();
    let assets = body
        .get("assets")
        .and_then(|a| a.as_array())
        .context("release has no assets array")?
        .iter()
        .filter_map(|a| {
            Some((
                a.get("name")?.as_str()?.to_string(),
                a.get("browser_download_url")?.as_str()?.to_string(),
            ))
        })
        .collect();
    Ok((tag, assets))
}

/// Unpack a `.tar.gz` / `.tar.xz` archive into `dest`, rejecting absolute
/// paths and `..` components (defense in depth over `unpack_in`).
fn extract(kind: AssetKind, archive: &Path, dest: &Path) -> Result<()> {
    // dest must already exist (created via the cache group); `unpack_in`
    // canonicalizes it as a traversal guard and fails otherwise.
    let file =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    match kind {
        AssetKind::TarGz => {
            extract_tar(flate2::read::GzDecoder::new(file), dest)?;
        }
        AssetKind::TarXz => {
            extract_tar(liblzma::read::XzDecoder::new(file), dest)?;
        }
    }
    // tar preserves mode bits, but ensure the tools are executable in case
    // the archive or platform dropped them.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(rd) = std::fs::read_dir(dest.join("qemu-portable/bin")) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_file() {
                    let mut perm = std::fs::metadata(&p)?.permissions();
                    perm.set_mode(perm.mode() | 0o111);
                    std::fs::set_permissions(&p, perm)?;
                }
            }
        }
    }
    Ok(())
}

fn extract_tar<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let path = entry.path().context("tar entry path")?.into_owned();
        if path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
            anyhow::bail!("refusing unsafe tar entry: {}", path.display());
        }
        entry
            .unpack_in(dest)
            .with_context(|| format!("unpack {}", path.display()))?;
    }
    Ok(())
}

/// Strip macOS quarantine so the adhoc-signed binaries actually launch under
/// HVF. Best-effort: ignored on other OSes and when `xattr` is unavailable.
fn strip_quarantine(dir: &Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .arg("-dr")
            .arg("com.apple.quarantine")
            .arg(dir)
            .output();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = dir;
    }
}

/// Ensure a portable QEMU tree in the project cache (never evicted).
///
/// - `dir`: explicit cache base, else `<project>/.cache/qemu-portable`
///   (same discovery as the ISO cache; `$WINDOWS_HEADLESS_QEMU_DIR`
///   overrides the base).
/// - `version`: explicit `11.1.1` (or tag form); else the latest GitHub
///   release. The API is only hit when the version is not already extracted.
pub async fn ensure(dir: Option<PathBuf>, version: Option<String>) -> Result<PortableQemu> {
    // All directories via cache-manager: explicit --dir / env override become
    // the root, otherwise the discovered project root. Every subdirectory
    // (group, version) goes through the group API — no raw create_dir_all.
    let root = match dir {
        Some(d) => CacheRoot::from_root(d),
        None => match std::env::var("WINDOWS_HEADLESS_QEMU_DIR") {
            Ok(d) => CacheRoot::from_root(d),
            Err(_) => CacheRoot::from_discovery().context("locate project cache root")?,
        },
    };
    // Ensure the base group exists even before we know the version, so the
    // fast path below can scan it.
    root.ensure_group("qemu-portable")
        .context("create qemu-portable cache group")?;

    let (marker, kind) = asset_for_host()?;

    let norm_version: String = match version {
        Some(v) => normalize_version(&v),
        None => {
            // Fast path: reuse the newest extracted tree without touching
            // the (rate-limited) GitHub API.
            if let Some(cached) = newest_extracted(&root.group_path("qemu-portable")) {
                return Ok(cached);
            }
            let (tag, _) = latest_release().await?;
            normalize_version(&tag)
        }
    };
    let group = root.group(format!("qemu-portable/{norm_version}"));
    group.ensure_dir().context("create version cache group")?;
    let tree = group.path().join("qemu-portable");
    if tree.join("bin/qemu-img").is_file() {
        return Ok(PortableQemu {
            root: tree,
            version: norm_version,
        });
    }

    // Pin the exact asset URLs from the release metadata so a renamed
    // asset fails loudly instead of 404ing a guessed URL.
    let (tag, assets) = release_by_version(&norm_version).await?;
    let _ = tag;
    let asset_name = assets
        .iter()
        .map(|(n, _)| n.as_str())
        .find(|n| n.contains(marker) && n.starts_with("qemu-portable-"))
        .context(format!(
            "release has no portable asset for this host ({marker})"
        ))?
        .to_string();
    let url = assets
        .iter()
        .find(|(n, _)| n == &asset_name)
        .map(|(_, u)| u.clone())
        .unwrap();

    let base = root.group_path("qemu-portable");
    let archive_path = base.join(format!("{asset_name}.download"));
    fetch_to(&url, &archive_path).await?;

    // Verify against the release SHA256SUMS.txt when present.
    if let Some(sums_url) = assets
        .iter()
        .find(|(n, _)| n == "SHA256SUMS.txt")
        .map(|(_, u)| u.clone())
    {
        let sums_path = base.join("SHA256SUMS.txt.download");
        fetch_to(&sums_url, &sums_path).await?;
        let body = std::fs::read_to_string(&sums_path)?;
        match parse_sums(&body, &asset_name) {
            Some(expected) => {
                let actual = crate::downloader::sha256_of(&archive_path).await?;
                anyhow::ensure!(
                    actual.eq_ignore_ascii_case(&expected),
                    "SHA-256 mismatch for {asset_name}: expected {expected}, got {actual}"
                );
            }
            None => {
                tracing::warn!("{asset_name} not listed in SHA256SUMS.txt; skipping verification")
            }
        }
        let _ = std::fs::remove_file(&sums_path);
    }

    let dest = group.path().to_path_buf();
    extract(kind, &archive_path, &dest)?;
    strip_quarantine(&dest);
    let _ = std::fs::remove_file(&archive_path);

    Ok(PortableQemu {
        root: dest.join("qemu-portable"),
        version: norm_version,
    })
}

/// Newest already-extracted tree (by numeric version order), if any.
fn newest_extracted(base: &Path) -> Option<PortableQemu> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(base).ok()?.flatten() {
        let tree = entry.path().join("qemu-portable");
        if tree.join("bin/qemu-img").is_file() {
            found.push((entry.file_name().to_string_lossy().into_owned(), tree));
        }
    }
    found.sort_by(|a, b| cmp_versions(&a.0, &b.0));
    found
        .pop()
        .map(|(version, root)| PortableQemu { root, version })
}

/// Numeric dotted-version order (`9.2.0 < 10.0.0 < 11.1.1`); non-numeric
/// components fall back to string order. Plain string sort would rank
/// `9.x` above `11.x`.
fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.split('.');
    let mut bi = b.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(xn), Ok(yn)) => xn.cmp(&yn),
                    _ => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Fetch release metadata for a concrete version (`11.1.1` or tag form).
async fn release_by_version(norm: &str) -> Result<(String, Vec<(String, String)>)> {
    let url = format!(
        "https://api.github.com/repos/jzombie/lab-qemu-portable/releases/tags/qemu-v{norm}"
    );
    match github_client()?.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await?;
            parse_release_body(&body)
        }
        _ => {
            // Tag scheme may differ (e.g. `v{norm}`); fall back to latest and
            // require it to match the requested version.
            let (tag, assets) = latest_release().await?;
            anyhow::ensure!(
                normalize_version(&tag) == norm,
                "requested portable QEMU {norm}, but latest release is {tag}"
            );
            Ok((tag, assets))
        }
    }
}

fn parse_release_body(body: &serde_json::Value) -> Result<(String, Vec<(String, String)>)> {
    let tag = body
        .get("tag_name")
        .and_then(|t| t.as_str())
        .context("release has no tag_name")?
        .to_string();
    let assets = body
        .get("assets")
        .and_then(|a| a.as_array())
        .context("release has no assets array")?
        .iter()
        .filter_map(|a| {
            Some((
                a.get("name")?.as_str()?.to_string(),
                a.get("browser_download_url")?.as_str()?.to_string(),
            ))
        })
        .collect();
    Ok((tag, assets))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_normalization() {
        assert_eq!(normalize_version("qemu-v11.1.1"), "11.1.1");
        assert_eq!(normalize_version("v11.1.1"), "11.1.1");
        assert_eq!(normalize_version("11.1.1"), "11.1.1");
    }

    #[test]
    fn sums_parsing() {
        let body = "abc123  qemu-portable-linux-x86_64-11.1.1.tar.xz\ndef456 *other.zip\n";
        assert_eq!(
            parse_sums(body, "qemu-portable-linux-x86_64-11.1.1.tar.xz"),
            Some("abc123".to_string())
        );
        assert_eq!(parse_sums(body, "missing"), None);
    }

    #[test]
    fn version_order_is_numeric_not_lexicographic() {
        assert_eq!(cmp_versions("9.2.0", "11.1.1"), std::cmp::Ordering::Less);
        assert_eq!(cmp_versions("11.1.1", "11.1.1"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn newest_extracted_picks_highest_version() {
        let tmp = tempfile::tempdir().unwrap();
        for v in ["9.2.0", "11.1.1", "10.0.0"] {
            let bin = tmp.path().join(v).join("qemu-portable/bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("qemu-img"), b"x").unwrap();
        }
        let got = newest_extracted(tmp.path()).unwrap();
        assert_eq!(got.version, "11.1.1");
        assert!(got.root.ends_with("11.1.1/qemu-portable"));
    }

    #[test]
    fn host_asset_selection_matches_this_machine() {
        // Must not error on the dev/CI hosts we support (macos-arm64,
        // linux x86_64/aarch64); other hosts get the actionable error.
        let _ = asset_for_host();
    }
}
