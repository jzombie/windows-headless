//! Portable QEMU provisioning from `jzombie/lab-qemu-portable` releases
//! (feature `downloader`).
//!
//! Layout on disk (managed by `cache-manager`, never evicted):
//! `<cache>/qemu-portable/<version>/qemu-portable/{bin,lib,share}/`.
//! The bundled dylibs/so link via `@executable_path`/`$ORIGIN`-relative
//! paths and QEMU finds `share/qemu` (OVMF firmware) relative to the
//! executable, so the tree works from any CWD with no env vars.
use anyhow::{Context, Result};
use std::env::consts;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const UA: &str = "windows-headless/0.1";

/// Env var granting explicit consent to strip `com.apple.quarantine` off the
/// extracted portable trees on macOS. Absent/false = fail closed.
pub const ALLOW_QUARANTINE_STRIP_ENV: &str = "WINDOWS_HEADLESS_ALLOW_QUARANTINE_STRIP";

/// Quarantine attribute stripped off the extracted trees. Single source of
/// truth for the `xattr` invocation, log lines, and the manual remediation
/// shown to users — never a second literal copy.
pub const QUARANTINE_ATTR: &str = "com.apple.quarantine";

/// Binary invoked to clear [`QUARANTINE_ATTR`]. Same single-source rule.
#[cfg(target_os = "macos")]
const XATTR_BIN: &str = "xattr";

/// Shell equivalent of the automatic strip, for error messages and logs.
#[cfg(target_os = "macos")]
fn quarantine_strip_cmdline(dir: &Path) -> String {
    format!("{XATTR_BIN} -dr {QUARANTINE_ATTR} \"{}\"", dir.display())
}

/// Fail-closed Gatekeeper error: `kind` is `"QEMU"` or `"wimlib"`.
#[cfg(target_os = "macos")]
fn quarantine_gate_error(dir: &Path, kind: &str) -> anyhow::Error {
    anyhow::anyhow!(
        r#"macOS Gatekeeper will block the adhoc-signed {kind} binaries.

Execution halted before downloading anything.

To permit automatic stripping of {QUARANTINE_ATTR} from the cache dir, set:

  {ALLOW_QUARANTINE_STRIP_ENV}=1

Or strip it yourself:

  {manual_cmd}"#,
        manual_cmd = quarantine_strip_cmdline(dir),
    )
}

/// Env var granting explicit consent to skip SHA-256 verification when a
/// release ships no `SHA256SUMS.txt` (or no entry for our asset). Absent/false
/// = fail closed. Mirrored by the `--allow-unverified` CLI flags.
pub const ALLOW_UNVERIFIED_ENV: &str = "WINDOWS_HEADLESS_ALLOW_UNVERIFIED";

/// Sole quarantine authorization boundary: true iff the operator explicitly
/// consented. Checked by `ensure()`/`ensure_wimlib()` BEFORE any download.
/// Both call sites are macOS-only; allowed-dead elsewhere so the truth-table
/// test keeps running on Linux CI.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn quarantine_strip_allowed() -> bool {
    // Bound first: do NOT chain a temporary String into the macro scrutinee.
    let opt_in = std::env::var(ALLOW_QUARANTINE_STRIP_ENV)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(opt_in.as_str(), "1" | "true" | "yes" | "on")
}

/// Verification escape hatch: explicit CLI flag wins, else the env var.
fn unverified_allowed(explicit: bool) -> bool {
    if explicit {
        return true;
    }
    let v = std::env::var(ALLOW_UNVERIFIED_ENV)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(v.as_str(), "1" | "true" | "yes" | "on")
}

/// Apply the quarantine strip to an already-authorized tree. No auth check, no
/// bail: authorization was proven by `quarantine_strip_allowed()` pre-fetch.
/// Async: `xattr` runs via `tokio::process::Command` so the executor yields
/// while the subprocess exits (a blocking `std::process::Command` here would
/// stall the Tokio worker). A swallowed `Result` would reintroduce the silent
/// path (extract succeeds, OS kills QEMU later with no explanation) — always
/// propagate with `.await?`.
async fn strip_quarantine(dir: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        tracing::info!(
            "quarantine: stripping {QUARANTINE_ATTR} on {} \
             (explicitly authorized via {ALLOW_QUARANTINE_STRIP_ENV})",
            dir.display()
        );

        match tokio::process::Command::new(XATTR_BIN)
            .arg("-dr")
            .arg(QUARANTINE_ATTR)
            .arg(dir)
            .output()
            .await
        {
            Ok(out) if out.status.success() => {}
            Ok(out) => tracing::warn!(
                "quarantine: `{}` failed: {}",
                quarantine_strip_cmdline(dir),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => tracing::warn!("quarantine: could not run `{XATTR_BIN}`: {e:#}"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = dir;
    }

    Ok(())
}

/// Fail-closed verification of `archive_path` against the release
/// `SHA256SUMS.txt`. Missing sums sheet or missing entry bails unless the
/// operator explicitly opted into unverified downloads. On hash mismatch the
/// corrupt archive is removed before returning the error. The sums sheet
/// itself is always removed (it is staging litter, never cache).
///
/// `expected_pin`: when `Some` (pinned tag + known platform), the sums-file
/// hash must additionally equal the code-pinned hash — this is what defeats a
/// release-infrastructure compromise that forges sums and archive together.
/// `None` for explicit test tags (bytes legitimately differ from the pin).
async fn verify_archive_against_sums(
    assets: &[(String, String)],
    asset_name: &str,
    archive_path: &Path,
    base: &Path,
    allow_unverified: bool,
    expected_pin: Option<&str>,
) -> Result<()> {
    let sums_url = assets
        .iter()
        .find(|(n, _)| n == crate::paths::SHA256SUMS_FILE)
        .map(|(_, u)| u.clone());
    let Some(sums_url) = sums_url else {
        if unverified_allowed(allow_unverified) {
            tracing::warn!(
                "{asset_name}: no {} in release; proceeding unverified (explicitly allowed via {ALLOW_UNVERIFIED_ENV} or --allow-unverified)",
                crate::paths::SHA256SUMS_FILE
            );
            return Ok(());
        }
        let _ = std::fs::remove_file(archive_path);
        anyhow::bail!(
            "{asset_name}: release ships no {sums} — refusing unverified download. \
             Set {ALLOW_UNVERIFIED_ENV}=1 or pass --allow-unverified to proceed anyway (not recommended)",
            sums = crate::paths::SHA256SUMS_FILE
        );
    };
    let sums_path = base.join(format!("{}.download", crate::paths::SHA256SUMS_FILE));
    fetch_to(&sums_url, &sums_path).await?;
    let body = std::fs::read_to_string(&sums_path)?;
    let _ = std::fs::remove_file(&sums_path);
    match parse_sums(&body, asset_name) {
        Some(expected) => {
            let actual = crate::downloader::sha256_of(archive_path).await?;
            if !actual.eq_ignore_ascii_case(&expected) {
                let _ = std::fs::remove_file(archive_path);
                anyhow::bail!(
                    "SHA-256 mismatch for {asset_name}: expected {expected}, got {actual}"
                );
            }
            if let Some(pin) = expected_pin
                && !expected.eq_ignore_ascii_case(pin)
            {
                let _ = std::fs::remove_file(archive_path);
                anyhow::bail!(
                    "SHA-256 sums entry for {asset_name} does not match the pinned hash \
                     (expected pin {pin}, release sums say {expected}): refusing possibly \
                     compromised release asset"
                );
            }
            Ok(())
        }
        None => {
            if unverified_allowed(allow_unverified) {
                tracing::warn!(
                    "{asset_name} not listed in {}; proceeding unverified (explicitly allowed)",
                    crate::paths::SHA256SUMS_FILE
                );
                return Ok(());
            }
            let _ = std::fs::remove_file(archive_path);
            anyhow::bail!(
                "{asset_name} not listed in {} — refusing unverified download. \
                 Set {ALLOW_UNVERIFIED_ENV}=1 or pass --allow-unverified to proceed anyway (not recommended)",
                crate::paths::SHA256SUMS_FILE
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    TarGz,
    TarXz,
}

/// A provisioned portable QEMU tree. `root` is the extracted `qemu-portable`
/// dir (the one containing `bin/`, `lib/`, `share/`); `version` is the
/// full release tag (`qemu-v11.1.1-57`), never a bare version.
#[derive(Debug, Clone)]
pub struct PortableQemu {
    pub root: PathBuf,
    pub version: String,
}

impl PortableQemu {
    pub fn qemu_img(&self) -> PathBuf {
        self.root
            .join(crate::paths::PORTABLE_BIN_DIR)
            .join(crate::paths::QEMU_IMG_NAME)
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
        ("macos", "aarch64") => Ok((crate::paths::MARKER_MACOS_ARM64, AssetKind::TarGz)),
        ("linux", "x86_64") => Ok((crate::paths::MARKER_LINUX_X86_64, AssetKind::TarXz)),
        ("linux", "aarch64") => Ok((crate::paths::MARKER_LINUX_AARCH64, AssetKind::TarXz)),
        (os, arch) => anyhow::bail!(
            "no portable QEMU build for {os}/{arch} (only macos-arm64 and linux x86_64/aarch64 are published); install system QEMU instead"
        ),
    }
}

/// Pinned SHA-256 for a QEMU portable asset marker at the pinned tag.
/// `None` for unknown markers (fail-closed downstream treats this as unverified,
/// which bails without the escape hatch — markers come from `asset_for_host`,
/// so `None` here means a new platform was added without pinning its hash).
/// `pub(crate)` for the `verify-pins` command.
pub(crate) fn pinned_qemu_sha256(marker: &str) -> Option<&'static str> {
    match marker {
        crate::paths::MARKER_MACOS_ARM64 => Some(crate::pins::QEMU_PORTABLE_SHA256_MACOS_ARM64),
        crate::paths::MARKER_LINUX_X86_64 => Some(crate::pins::QEMU_PORTABLE_SHA256_LINUX_X86_64),
        crate::paths::MARKER_LINUX_AARCH64 => Some(crate::pins::QEMU_PORTABLE_SHA256_LINUX_AARCH64),
        _ => None,
    }
}

/// Pinned SHA-256 for a wimlib portable asset marker at the pinned tag.
/// Same contract as [`pinned_qemu_sha256`].
/// `pub(crate)` for the `verify-pins` command.
pub(crate) fn pinned_wimlib_sha256(marker: &str) -> Option<&'static str> {
    match marker {
        crate::paths::MARKER_MACOS_ARM64 => Some(crate::pins::WIMLIB_PORTABLE_SHA256_MACOS_ARM64),
        crate::paths::MARKER_LINUX_X86_64 => Some(crate::pins::WIMLIB_PORTABLE_SHA256_LINUX_X86_64),
        crate::paths::MARKER_LINUX_AARCH64 => {
            Some(crate::pins::WIMLIB_PORTABLE_SHA256_LINUX_AARCH64)
        }
        _ => None,
    }
}

/// Pinned QEMU release tag: `run`/`install` use exactly this, `fetch-qemu`
/// downloads exactly this unless `--qemu-tag` overrides. Value lives in
/// [`crate::pins`]; this alias keeps existing call sites.
pub const DEFAULT_QEMU_PORTABLE_TAG: &str = crate::pins::QEMU_PORTABLE_TAG;

/// Parse a `SHA256SUMS.txt`-style body (`<hex>  <filename>` per line).
/// `pub(crate)` for the `verify-pins` command, which reuses release parsing
/// without downloading archives.
pub(crate) fn parse_sums(body: &str, asset: &str) -> Option<String> {
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

/// Download `url` to `dest` (via `.part` + rename, as in `downloader`),
/// with throttled stderr progress (see `downloader::Progress`).
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
    // Display the asset name, not our `.download` staging suffix.
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dest.display().to_string());
    let label = name.strip_suffix(".download").unwrap_or(&name).to_string();
    let mut progress = crate::downloader::Progress::new(&label, resp.content_length());
    let tmp = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp).await?;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = resp.chunk().await? {
        progress.add(chunk.len());
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    progress.finish();
    drop(file);
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

/// GitHub API client (JSON + User-Agent), shared by the tag endpoints.
fn github_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("reqwest client")
}

/// Unpack a `.tar.gz` / `.tar.xz` archive into `dest`, rejecting absolute
/// paths and `..` components (defense in depth over `unpack_in`).
fn extract(kind: AssetKind, archive: &Path, dest: &Path, inner_bin_dir: &str) -> Result<()> {
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
        if let Ok(rd) = std::fs::read_dir(dest.join(inner_bin_dir)) {
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

/// Ensure a portable QEMU tree in the project cache (never evicted).
///
/// - `dir`: explicit cache base, else `<project>/.cache/qemu-portable`
///   (same discovery as the ISO cache; `$WINDOWS_HEADLESS_QEMU_DIR`
///   overrides the base).
/// - `version`: exact release tag (for new-build testing); else the
///   pinned [`DEFAULT_QEMU_PORTABLE_TAG`]. Tags are used verbatim —
///   no normalization. Already-extracted trees are reused without
///   touching the network.
/// - `allow_unverified`: permit proceeding when the release ships no
///   `SHA256SUMS.txt` (or no entry for our asset). Fails closed by default;
///   also honored via `$WINDOWS_HEADLESS_ALLOW_UNVERIFIED`.
pub async fn ensure(
    dir: Option<PathBuf>,
    version: Option<String>,
    allow_unverified: bool,
) -> Result<PortableQemu> {
    // All directories via the shared crate::cache helper: explicit --dir /
    // env override win, else workspace discovery, else the OS user cache.
    // Every subdirectory (group, version) goes through the group API —
    // no raw create_dir_all.
    let root = crate::cache::resolve_cache_root(dir, "WINDOWS_HEADLESS_QEMU_DIR")?;
    // Ensure the base group exists even before we know the version, so the
    // fast path below can scan it.
    root.ensure_group(crate::paths::QEMU_GROUP)
        .context("create qemu-portable cache group")?;

    let (marker, kind) = asset_for_host()?;

    let tag: String = match version {
        Some(v) => v,
        None => DEFAULT_QEMU_PORTABLE_TAG.to_string(),
    };
    let group = root.group(format!("{}/{tag}", crate::paths::QEMU_GROUP));
    group.ensure_dir().context("create version cache group")?;
    let tree = group.path().join(crate::paths::QEMU_INNER);
    if tree
        .join(crate::paths::PORTABLE_BIN_DIR)
        .join(crate::paths::QEMU_IMG_NAME)
        .is_file()
    {
        return Ok(PortableQemu {
            root: tree,
            version: tag,
        });
    }

    // Fail fast on macOS BEFORE any network: Gatekeeper would block the
    // adhoc-signed binaries, so downloading first would only waste gigabytes.
    #[cfg(target_os = "macos")]
    if !quarantine_strip_allowed() {
        return Err(quarantine_gate_error(group.path(), "QEMU"));
    }

    // Pin the exact asset URLs from the release metadata so a renamed
    // asset fails loudly instead of 404ing a guessed URL.
    let assets = release_by_tag(&tag).await?;
    let asset_name = assets
        .iter()
        .map(|(n, _)| n.as_str())
        .find(|n| n.contains(marker) && n.starts_with(crate::paths::QEMU_ASSET_PREFIX))
        .context(format!(
            "release has no portable asset for this host ({marker})"
        ))?
        .to_string();
    let url = assets
        .iter()
        .find(|(n, _)| n == &asset_name)
        .map(|(_, u)| u.clone())
        .unwrap();

    let base = root.group_path(crate::paths::QEMU_GROUP);
    let archive_path = base.join(format!("{asset_name}.download"));
    fetch_to(&url, &archive_path).await?;

    // Fail-closed verification: missing sums sheet or entry bails unless the
    // operator explicitly allowed unverified downloads. Mismatches remove the
    // corrupt archive before returning. The pin additionally binds the sums
    // entry for the pinned tag, defeating forged sums+archive pairs.
    let expected_pin = (tag == DEFAULT_QEMU_PORTABLE_TAG).then(|| pinned_qemu_sha256(marker));
    let expected_pin = expected_pin.flatten();
    verify_archive_against_sums(
        &assets,
        &asset_name,
        &archive_path,
        &base,
        allow_unverified,
        expected_pin,
    )
    .await?;

    // No staging: extract straight into the group dir. Safe because the
    // sentinel check above guarantees no good tree lives here — `dest` is
    // absent or stale — and a killed fetch leaves a sentinel-less tree the
    // next run simply refetches. Validity = sentinel presence, nothing else.
    // (Concurrent double-fetch can interleave in one dir; single-user CLI,
    // and any interleaving still ends sentinel-less on failure.)
    let dest = group.path().to_path_buf();
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("clear stale {}", dest.display()))?;
    }
    // Group creation normalizes; the re-sweep after extract clears the
    // read-only modes tar writes per member. Single call, owned by
    // cache-manager — no local permission logic anywhere in this flow.
    // WHY: release tarballs ship 0444 lib files (verified live against
    // the macos-arm64 asset); without this, the quarantine strip below
    // fails EACCES on every member and Windows tree deletion breaks on
    // rotation. Belt (ensure_dir) and suspenders (this call): tar
    // re-applies read-only modes during unpack, after creation normalized.
    group.ensure_dir()?;
    extract(
        kind,
        &archive_path,
        &dest,
        &format!(
            "{}/{}",
            crate::paths::QEMU_INNER,
            crate::paths::PORTABLE_BIN_DIR
        ),
    )?;
    // WHY (explicitly): tar just re-applied 0444 modes per member
    // during unpack, undoing what creation normalized. This re-sweep —
    // through the group interface, owned by cache-manager — clears them
    // so the strip below, and every later cache operation, never hits
    // EACCES. The comment stays so nobody "simplifies" it away.
    group.ensure_dir()?;
    strip_quarantine(&dest).await?;
    let _ = std::fs::remove_file(&archive_path);
    let root = dest.join(crate::paths::QEMU_INNER);

    Ok(PortableQemu { root, version: tag })
}

/// Pinned portable tree in the project cache, without touching the
/// network. Used by `run`/`install` so end users never pass `--qemu-dir`
/// — and never silently run a different QEMU than the pin. Anything else
/// (e.g. a newer build under test) must be selected explicitly via
/// `--qemu-dir` or `fetch-qemu --qemu-tag`.
pub fn pinned_cached() -> Result<PortableQemu> {
    let root = crate::cache::resolve_cache_root(None, "WINDOWS_HEADLESS_QEMU_DIR")?;
    let base = root.group_path(crate::paths::QEMU_GROUP);
    let tag = DEFAULT_QEMU_PORTABLE_TAG.to_string();
    let tree = base.join(&tag).join(crate::paths::QEMU_INNER);
    if tree
        .join(crate::paths::PORTABLE_BIN_DIR)
        .join(crate::paths::QEMU_IMG_NAME)
        .is_file()
    {
        return Ok(PortableQemu {
            root: tree,
            version: tag,
        });
    }
    let found = cached_versions(&base);
    anyhow::bail!(
        "pinned portable QEMU {tag} not cached (found: {found}); run `fetch-qemu` (fetches the pin) or pass `--qemu-dir` to use another tree explicitly",
    )
}

/// Versions with an extracted tree under a `qemu-portable` cache base
/// (full tags, or `none`), for the `pinned_cached` error message.
fn cached_versions(base: &Path) -> String {
    let mut found: Vec<String> = std::fs::read_dir(base)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.path()
                        .join(crate::paths::QEMU_INNER)
                        .join(crate::paths::PORTABLE_BIN_DIR)
                        .join(crate::paths::QEMU_IMG_NAME)
                        .is_file()
                })
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    if found.is_empty() {
        "none".to_string()
    } else {
        found.join(", ")
    }
}

/// Fetch release metadata for an exact release tag (`qemu-v11.1.1-57`).
/// No latest-release fallback: under never-overwrite rebuild tags, latest
/// is never the answer to "give me the pin" — a missing tag is a hard
/// error naming the tag.
/// `pub(crate)` for the `verify-pins` command, which checks asset
/// availability without downloading archives.
pub(crate) async fn release_by_tag(tag: &str) -> Result<Vec<(String, String)>> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/tags/{tag}",
        crate::pins::RELEASE_REPO,
    );
    let body: serde_json::Value = github_client()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("portable QEMU release {tag} not found"))?
        .json()
        .await?;
    let (_, assets) = parse_release_body(&body)?;
    Ok(assets)
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

/// A provisioned portable wimlib tree (`wimlib-portable` release asset).
/// `root` is the extracted inner dir holding `bin/wimlib-imagex`.
#[derive(Debug, Clone)]
pub struct PortableWimlib {
    pub root: PathBuf,
    pub version: String,
}

impl PortableWimlib {
    pub fn wimlib_bin(&self) -> PathBuf {
        self.root
            .join(crate::paths::PORTABLE_BIN_DIR)
            .join(crate::paths::WIMLIB_IMGX_NAME)
    }
}

/// Pinned wimlib release tag: `build-media` uses exactly this,
/// `fetch-wimlib` downloads exactly this unless `--wimlib-tag` overrides.
/// Value lives in [`crate::pins`]; this alias keeps existing call sites.
pub const DEFAULT_WIMLIB_PORTABLE_TAG: &str = crate::pins::WIMLIB_PORTABLE_TAG;

/// Release metadata for an exact tag in `repo` (`owner/name`).
async fn release_for_tag(repo: &str, tag: &str) -> Result<(String, Vec<(String, String)>)> {
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
    let body: serde_json::Value = github_client()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .json()
        .await?;
    parse_release_body(&body)
}

/// Download URL of `asset` in release `tag` of `repo` (fails loudly on rename).
pub async fn release_asset_url(repo: &str, tag: &str, asset: &str) -> Result<String> {
    let (_, assets) = release_for_tag(repo, tag).await?;
    assets
        .iter()
        .find(|(n, _)| n == asset)
        .map(|(_, u)| u.clone())
        .with_context(|| format!("release {tag} has no asset {asset}"))
}

/// Ensure a portable wimlib tree in the project cache (never evicted).
/// Same shape as [`ensure`]: `$WINDOWS_HEADLESS_WIMLIB_DIR` overrides,
/// else `<project>/.cache/wimlib-portable`; exact release tag or the
/// pinned [`DEFAULT_WIMLIB_PORTABLE_TAG`]. Tags are used verbatim.
/// Already-extracted trees are reused without touching the network.
/// `allow_unverified` mirrors [`ensure`]: fail closed unless set (or
/// `$WINDOWS_HEADLESS_ALLOW_UNVERIFIED`).
pub async fn ensure_wimlib(
    dir: Option<PathBuf>,
    version: Option<String>,
    allow_unverified: bool,
) -> Result<PortableWimlib> {
    let root = crate::cache::resolve_cache_root(dir, "WINDOWS_HEADLESS_WIMLIB_DIR")?;
    root.ensure_group(crate::paths::WIMLIB_GROUP)
        .context("create wimlib-portable cache group")?;
    let (marker, kind) = asset_for_host()?;
    let tag: String = match version {
        Some(v) => v,
        None => DEFAULT_WIMLIB_PORTABLE_TAG.to_string(),
    };
    let group = root.group(format!("{}/{tag}", crate::paths::WIMLIB_GROUP));
    group.ensure_dir().context("create version cache group")?;
    let tree = group.path().join(crate::paths::WIMLIB_INNER);
    if tree
        .join(crate::paths::PORTABLE_BIN_DIR)
        .join(crate::paths::WIMLIB_IMGX_NAME)
        .is_file()
    {
        return Ok(PortableWimlib {
            root: tree,
            version: tag,
        });
    }
    // Fail fast on macOS BEFORE any network (same Gatekeeper boundary as QEMU).
    #[cfg(target_os = "macos")]
    if !quarantine_strip_allowed() {
        return Err(quarantine_gate_error(group.path(), "wimlib"));
    }
    let (_, assets) = release_for_tag(crate::pins::RELEASE_REPO, &tag).await?;
    let asset_name = assets
        .iter()
        .map(|(n, _)| n.as_str())
        .find(|n| n.contains(marker) && n.starts_with(crate::paths::WIMLIB_ASSET_PREFIX))
        .with_context(|| format!("release {tag} has no portable asset for this host ({marker})"))?
        .to_string();
    let url = assets
        .iter()
        .find(|(n, _)| n == &asset_name)
        .map(|(_, u)| u.clone())
        .unwrap();
    let base = root.group_path(crate::paths::WIMLIB_GROUP);
    let archive_path = base.join(format!("{asset_name}.download"));
    fetch_to(&url, &archive_path).await?;
    let expected_pin = (tag == DEFAULT_WIMLIB_PORTABLE_TAG).then(|| pinned_wimlib_sha256(marker));
    let expected_pin = expected_pin.flatten();
    verify_archive_against_sums(
        &assets,
        &asset_name,
        &archive_path,
        &base,
        allow_unverified,
        expected_pin,
    )
    .await?;
    let dest = group.path().to_path_buf();
    // No staging (see QEMU flow above): sentinel check at entry guarantees
    // no good tree lives here; validity is sentinel presence alone.
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("clear stale {}", dest.display()))?;
    }
    // Group creation normalizes; the re-sweep after extract clears the
    // read-only modes tar writes per member. Owned by cache-manager.
    group.ensure_dir()?;
    extract(
        kind,
        &archive_path,
        &dest,
        &format!(
            "{}/{}",
            crate::paths::WIMLIB_INNER,
            crate::paths::PORTABLE_BIN_DIR
        ),
    )?;
    // WHY (explicitly): tar just re-applied 0444 modes per member
    // during unpack, undoing what creation normalized. This re-sweep —
    // through the group interface, owned by cache-manager — clears them
    // so the strip below, and every later cache operation, never hits
    // EACCES. The comment stays so nobody "simplifies" it away.
    group.ensure_dir()?;
    strip_quarantine(&dest).await?;
    let _ = std::fs::remove_file(&archive_path);
    let root = dest.join(crate::paths::WIMLIB_INNER);
    Ok(PortableWimlib { root, version: tag })
}

/// Pinned portable wimlib tree, without touching the network. Used by
/// `build-media` so end users never pass `--wimlib-bin` — and never
/// silently split with a different wimlib than the pin.
pub fn pinned_cached_wimlib() -> Result<PortableWimlib> {
    let root = crate::cache::resolve_cache_root(None, "WINDOWS_HEADLESS_WIMLIB_DIR")?;
    let base = root.group_path(crate::paths::WIMLIB_GROUP);
    let tag = DEFAULT_WIMLIB_PORTABLE_TAG.to_string();
    let tree = base.join(&tag).join(crate::paths::WIMLIB_INNER);
    if tree
        .join(crate::paths::PORTABLE_BIN_DIR)
        .join(crate::paths::WIMLIB_IMGX_NAME)
        .is_file()
    {
        return Ok(PortableWimlib {
            root: tree,
            version: tag,
        });
    }
    let found = cached_wimlib_versions(&base);
    anyhow::bail!(
        "pinned portable wimlib {tag} not cached (found: {found}); run `fetch-wimlib` (fetches the pin)"
    )
}

/// Versions with an extracted tree under a `wimlib-portable` cache base,
/// for the `pinned_cached_wimlib` error message.
fn cached_wimlib_versions(base: &Path) -> String {
    let mut found: Vec<String> = std::fs::read_dir(base)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.path()
                        .join(crate::paths::WIMLIB_INNER)
                        .join(crate::paths::PORTABLE_BIN_DIR)
                        .join(crate::paths::WIMLIB_IMGX_NAME)
                        .is_file()
                })
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    if found.is_empty() {
        "none".to_string()
    } else {
        found.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn default_qemu_tag_is_the_pin() {
        assert_eq!(DEFAULT_QEMU_PORTABLE_TAG, crate::pins::QEMU_PORTABLE_TAG);
    }

    #[test]
    fn pinned_cached_finds_pin_and_rejects_others() {
        let tmp = tempfile::tempdir().unwrap();
        // Cache dirs are tags, verbatim.
        let pin_bin = tmp
            .path()
            .join("qemu-portable")
            .join(DEFAULT_QEMU_PORTABLE_TAG)
            .join("qemu-portable/bin");
        std::fs::create_dir_all(&pin_bin).unwrap();
        std::fs::write(pin_bin.join("qemu-img"), b"x").unwrap();
        // A newer tree must NOT shadow the pin.
        let other_bin = tmp
            .path()
            .join("qemu-portable/qemu-v99.99.99/qemu-portable/bin");
        std::fs::create_dir_all(&other_bin).unwrap();
        std::fs::write(other_bin.join("qemu-img"), b"x").unwrap();
        // SAFETY: process-local env, restored below; no other test reads
        // this variable (only pinned_cached, called solely here).
        unsafe { std::env::set_var("WINDOWS_HEADLESS_QEMU_DIR", tmp.path()) };
        let got = pinned_cached().unwrap();
        assert_eq!(got.version, DEFAULT_QEMU_PORTABLE_TAG);
        assert!(
            got.root
                .ends_with(format!("{}/qemu-portable", DEFAULT_QEMU_PORTABLE_TAG))
        );
        std::fs::remove_dir_all(
            tmp.path()
                .join("qemu-portable")
                .join(DEFAULT_QEMU_PORTABLE_TAG),
        )
        .unwrap();
        assert!(pinned_cached().is_err());
        unsafe { std::env::remove_var("WINDOWS_HEADLESS_QEMU_DIR") };
    }

    #[test]
    fn default_wimlib_tag_is_the_pin() {
        assert_eq!(
            DEFAULT_WIMLIB_PORTABLE_TAG,
            crate::pins::WIMLIB_PORTABLE_TAG
        );
    }

    #[test]
    fn pinned_cached_wimlib_finds_pin_and_rejects_others() {
        let tmp = tempfile::tempdir().unwrap();
        let pin_bin = tmp
            .path()
            .join("wimlib-portable")
            .join(DEFAULT_WIMLIB_PORTABLE_TAG)
            .join("wimlib-portable/bin");
        std::fs::create_dir_all(&pin_bin).unwrap();
        std::fs::write(pin_bin.join("wimlib-imagex"), b"x").unwrap();
        let other_bin = tmp
            .path()
            .join("wimlib-portable/wimlib-v9.9.9/wimlib-portable/bin");
        std::fs::create_dir_all(&other_bin).unwrap();
        std::fs::write(other_bin.join("wimlib-imagex"), b"x").unwrap();
        // SAFETY: process-local env, restored below; no other test reads
        // this variable (only pinned_cached_wimlib, called solely here).
        unsafe { std::env::set_var("WINDOWS_HEADLESS_WIMLIB_DIR", tmp.path()) };
        let got = pinned_cached_wimlib().unwrap();
        assert_eq!(got.version, DEFAULT_WIMLIB_PORTABLE_TAG);
        assert!(
            got.root
                .ends_with(format!("{}/wimlib-portable", DEFAULT_WIMLIB_PORTABLE_TAG))
        );
        std::fs::remove_dir_all(
            tmp.path()
                .join("wimlib-portable")
                .join(DEFAULT_WIMLIB_PORTABLE_TAG),
        )
        .unwrap();
        assert!(pinned_cached_wimlib().is_err());
        unsafe { std::env::remove_var("WINDOWS_HEADLESS_WIMLIB_DIR") };
    }

    #[test]
    fn host_asset_selection_matches_this_machine() {
        // Must not error on the dev/CI hosts we support (macos-arm64,
        // linux x86_64/aarch64); other hosts get the actionable error.
        let _ = asset_for_host();
    }

    #[test]
    fn quarantine_gate_truth_table() {
        // SAFETY: this test owns ALLOW_QUARANTINE_STRIP_ENV; no other test
        // reads it (only quarantine_strip_allowed, called solely here).
        let prior = std::env::var(ALLOW_QUARANTINE_STRIP_ENV).ok();
        unsafe { std::env::remove_var(ALLOW_QUARANTINE_STRIP_ENV) };
        assert!(!quarantine_strip_allowed());
        for v in ["1", "true", "yes", "on", "TRUE", "Yes", "ON"] {
            unsafe { std::env::set_var(ALLOW_QUARANTINE_STRIP_ENV, v) };
            assert!(quarantine_strip_allowed(), "want allowed for {v:?}");
        }
        for v in ["0", "false", "no", "", "maybe"] {
            unsafe { std::env::set_var(ALLOW_QUARANTINE_STRIP_ENV, v) };
            assert!(!quarantine_strip_allowed(), "want denied for {v:?}");
        }
        match prior {
            Some(v) => unsafe { std::env::set_var(ALLOW_QUARANTINE_STRIP_ENV, v) },
            None => unsafe { std::env::remove_var(ALLOW_QUARANTINE_STRIP_ENV) },
        }
    }

    #[test]
    fn unverified_gate_truth_table() {
        // SAFETY: same single-owner discipline as the quarantine gate test.
        let prior = std::env::var(ALLOW_UNVERIFIED_ENV).ok();
        unsafe { std::env::remove_var(ALLOW_UNVERIFIED_ENV) };
        assert!(!unverified_allowed(false));
        assert!(unverified_allowed(true));
        unsafe { std::env::set_var(ALLOW_UNVERIFIED_ENV, "1") };
        assert!(unverified_allowed(false));
        unsafe { std::env::set_var(ALLOW_UNVERIFIED_ENV, "0") };
        assert!(!unverified_allowed(false));
        assert!(unverified_allowed(true));
        match prior {
            Some(v) => unsafe { std::env::set_var(ALLOW_UNVERIFIED_ENV, v) },
            None => unsafe { std::env::remove_var(ALLOW_UNVERIFIED_ENV) },
        }
    }

    // TODO: Remove
    #[test]
    fn no_staging_abstractions_anywhere() {
        // No staging, anywhere: no guard types, no staging dirs, no
        // publish step. Fetches extract straight into the group dir;
        // validity is the sentinel file alone. Patterns are assembled at
        // runtime so this test never matches its own source.
        let src = std::fs::read_to_string(file!()).unwrap();
        let banned = [
            ["struct Staging", "Guard {"].concat(),
            ["Staging", "Dir::"].concat(),
            ["publish_staged", "("].concat(),
            [".staging", "-{"].concat(),
        ];
        for b in &banned {
            assert!(!src.contains(b), "staging abstraction resurrected");
        }
    }
}
