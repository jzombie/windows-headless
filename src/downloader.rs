//! ISO download helpers (feature `downloader`).
use anyhow::{Context, Result};
use std::path::Path;

/// virtio-win ISO vendored on our own releases (no Fedora bot-wall, no
/// rsync): `drivers-<date>` tag, `virtio-win.iso` + `virtio-win.iso.sha256`.
/// Values live in [`crate::pins`]; these aliases keep existing call sites.
pub const VIRTIO_RELEASE_REPO: &str = crate::pins::RELEASE_REPO;
pub const VIRTIO_RELEASE_TAG: &str = crate::pins::DRIVERS_RELEASE_TAG;
pub const VIRTIO_RELEASE_ASSET: &str = crate::paths::VIRTIO_ISO;
/// Guest SSH server tarball: OxDock release asset from
/// <https://github.com/jzombie/rust-oxdock> (per guest arch: x86_64 vs
/// aarch64 Windows MSVC). The guest runs no OpenSSH at all — this is the
/// only SSH software in the pipeline.
pub fn oxdock_asset_url(arch: crate::unattend::Architecture) -> String {
    format!(
        "https://github.com/jzombie/rust-oxdock/releases/download/{}/{}",
        crate::pins::OXDOCK_RELEASE_TAG,
        arch.oxdock_asset_name()
    )
}

/// Unpack the single `oxdock.exe` out of a verified release tarball into
/// `dest` (the ISO-cache exe slot `build-media` stages onto the USB).
/// Fails closed when the tarball holds no such entry — a wrong-asset
/// download must never stage a half-named binary.
pub fn extract_oxdock_exe(tarball: &Path, dest: &Path) -> Result<()> {
    use std::ffi::OsStr;
    let file =
        std::fs::File::open(tarball).with_context(|| format!("open {}", tarball.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    let entries = archive
        .entries()
        .with_context(|| format!("read tar index of {}", tarball.display()))?;
    for entry in entries {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.file_name() == Some(OsStr::new("oxdock.exe")) {
            entry
                .unpack(dest)
                .with_context(|| format!("unpack oxdock.exe to {}", dest.display()))?;
            eprintln!(
                "oxdock: {} -> {} ({} bytes)",
                tarball.display(),
                dest.display(),
                std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0)
            );
            return Ok(());
        }
    }
    anyhow::bail!("oxdock.exe not found in {}", tarball.display())
}
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Stream `url` to `dest`, computing SHA-256 on the fly. Returns hex digest.
/// Always (re)downloads; see [`download_if_missing`] for the caching variant.
pub async fn download(url: &str, dest: &Path) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("reqwest client")?;
    let mut resp = client.get(url).send().await?.error_for_status()?;
    let label = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dest.display().to_string());
    let mut progress = Progress::new(&label, resp.content_length());
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("create {}", dest.display()))?;
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;
    let mut hasher = sha2::Sha256::new();
    while let Some(chunk) = resp.chunk().await? {
        hasher.update(&chunk);
        progress.add(chunk.len());
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    progress.finish();
    Ok(hex::encode(hasher.finalize()))
}

/// Throttled stderr progress for multi-MB downloads (Eval ISOs are GBs;
/// without this the CLI sits silent for minutes). Dependency-free: byte
/// counting + one line at most per second, plus a final line. Status goes
/// to stderr per the crate stream rule — never stdout.
pub struct Progress {
    label: String,
    total: Option<u64>,
    downloaded: u64,
    start: std::time::Instant,
    last_print: std::time::Instant,
}

impl Progress {
    pub fn new(label: &str, total: Option<u64>) -> Self {
        let now = std::time::Instant::now();
        Self {
            label: label.to_string(),
            total,
            downloaded: 0,
            start: now,
            last_print: now,
        }
    }

    /// Record `n` more bytes; prints at most once per second.
    pub fn add(&mut self, n: usize) {
        self.downloaded += n as u64;
        let now = std::time::Instant::now();
        if now.duration_since(self.last_print).as_secs() >= 1 {
            self.last_print = now;
            eprintln!(
                "{}",
                format_progress(
                    &self.label,
                    self.downloaded,
                    self.total,
                    now.duration_since(self.start).as_secs_f64(),
                )
            );
        }
    }

    /// Final line (always printed, even for tiny files).
    pub fn finish(&self) {
        eprintln!(
            "{}",
            format_progress(
                &self.label,
                self.downloaded,
                self.total,
                std::time::Instant::now()
                    .duration_since(self.start)
                    .as_secs_f64(),
            )
        );
    }
}

/// Pure rendering (unit-tested): `label: 12.5/34.7 MB (36%, 8.3 MB/s)`;
/// without a known total the fraction is omitted.
fn format_progress(label: &str, downloaded: u64, total: Option<u64>, elapsed_secs: f64) -> String {
    let speed = downloaded as f64 / elapsed_secs.max(0.001);
    match total {
        Some(t) if t > 0 => format!(
            "{}: {}/{} ({:.0}%, {}/s)",
            label,
            fmt_size(downloaded),
            fmt_size(t),
            downloaded as f64 / t as f64 * 100.0,
            fmt_size(speed as u64),
        ),
        _ => format!(
            "{}: {} ({}/s)",
            label,
            fmt_size(downloaded),
            fmt_size(speed as u64),
        ),
    }
}

/// Human size: `512 B`, `3.2 KB`, `12.5 MB`, `1.2 GB`.
fn fmt_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Resolve (and create) the ISO cache dir via the shared [`crate::cache`]
/// helper: `$WINDOWS_HEADLESS_ISO_DIR` wins, then workspace discovery, then
/// the OS user cache — never a silent `<cwd>/.cache`.
///
/// Eviction is intentionally **disabled**: the group is ensured with no
/// `EvictPolicy` (no `max_bytes`, no `max_files`, no `max_age`). ISOs are
/// multi-GB and the Eval URLs are time-limited, so size-capped eviction
/// would force surprise re-downloads.
pub fn iso_cache_dir() -> Result<std::path::PathBuf> {
    let root = crate::cache::resolve_cache_root(None, "WINDOWS_HEADLESS_ISO_DIR")?;
    root.ensure_group("iso").context("create ISO cache group")
}

/// Outcome of [`download_if_missing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadStatus {
    /// File already present (and hash-verified when `expected_sha256` given).
    Cached { path: std::path::PathBuf },
    /// Freshly downloaded; includes the computed SHA-256 hex digest.
    Downloaded {
        path: std::path::PathBuf,
        sha256: String,
    },
}

/// Download `url` to `dest` unless it is already there.
///
/// - File exists and non-empty with no `expected_sha256`: skipped.
/// - File exists with `expected_sha256`: re-downloaded only on hash mismatch.
/// - Missing or empty: downloaded (parent dirs created).
/// - `force`: skip all of the above and re-download unconditionally (for
///   URL-rotated artifacts like the Eval ISO, where the cache slot name
///   can't see that the upstream bytes changed).
///
/// Note: Eval-ISO URLs are time-limited, so callers should only pass
/// `expected_sha256` for stable artifacts like `virtio-win.iso`.
pub async fn download_if_missing(
    url: &str,
    dest: &Path,
    expected_sha256: Option<&str>,
    force: bool,
) -> Result<DownloadStatus> {
    if !force
        && let Ok(meta) = std::fs::metadata(dest)
        && meta.len() > 0
    {
        match expected_sha256 {
            Some(expected) => {
                let actual = sha256_of(dest).await?;
                if actual.eq_ignore_ascii_case(expected) {
                    tracing::info!("using cached {}", dest.display());
                    return Ok(DownloadStatus::Cached {
                        path: dest.to_path_buf(),
                    });
                }
                tracing::warn!(
                    "hash mismatch for {} (expected {expected}, got {actual}); re-downloading",
                    dest.display()
                );
            }
            None => {
                tracing::info!("using cached {}", dest.display());
                return Ok(DownloadStatus::Cached {
                    path: dest.to_path_buf(),
                });
            }
        }
    }
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    // Download to a temp sibling first so a failed run never leaves a
    // truncated file that a later call would mistake for a valid cache entry.
    let tmp = dest.with_extension("part");
    let sha256 = match download(url, &tmp).await {
        Ok(s) => s,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    if let Some(expected) = expected_sha256
        && !sha256.eq_ignore_ascii_case(expected)
    {
        // Ordering is load-bearing: remove the poisoned tmp BEFORE bailing.
        let _ = tokio::fs::remove_file(&tmp).await;
        anyhow::bail!(
            "SHA-256 mismatch for {}: expected {expected}, got {sha256}",
            dest.display()
        );
    }
    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
    Ok(DownloadStatus::Downloaded {
        path: dest.to_path_buf(),
        sha256,
    })
}

/// SHA-256 hex digest of an existing file.
pub async fn sha256_of(path: &Path) -> Result<String> {
    use sha2::Digest;
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}
/// Fetch a small text URL (SHA256SUMS sidecars, version pins).
pub async fn fetch_text(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("reqwest client")?;
    let body = client
        .get(url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .text()
        .await?;
    Ok(body)
}

/// `(download URL, SHA-256)` for the vendored virtio-win ISO: asset URL
/// from release metadata (rename-proof) + hash from the `.sha256` sidecar.
///
/// The sidecar hash must additionally match the code pin
/// ([`crate::pins::VIRTIO_WIN_SHA256`]): a forged sidecar+ISO pair fails the
/// pin check, closing the TOFU gap. Mismatch here means the drivers release
/// was tampered with or the pin is stale after a drivers-tag bump.
///
/// Always resolve, even when the file is already cached — do NOT
/// short-circuit on existence. The sidecar hash is the update channel:
/// bumping the drivers pin changes the expected hash, which is what makes
/// `download_if_missing` re-download the stale file; it is also the only
/// check that sees corruption beyond the ISO magic bytes (a truncated
/// multi-hundred-MB download still has valid magic). A cheaper
/// exists-or-magic check would cement stale files silently.
pub async fn virtio_release_info() -> Result<(String, String)> {
    let url = crate::qemu_portable::release_asset_url(
        VIRTIO_RELEASE_REPO,
        VIRTIO_RELEASE_TAG,
        VIRTIO_RELEASE_ASSET,
    )
    .await?;
    let sha_url = crate::qemu_portable::release_asset_url(
        VIRTIO_RELEASE_REPO,
        VIRTIO_RELEASE_TAG,
        crate::paths::VIRTIO_ISO_SHA256,
    )
    .await?;
    let body = fetch_text(&sha_url).await?;
    let sha = body
        .split_whitespace()
        .next()
        .with_context(|| format!("empty {}", crate::paths::VIRTIO_ISO_SHA256))?
        .to_string();
    if !sha.eq_ignore_ascii_case(crate::pins::VIRTIO_WIN_SHA256) {
        anyhow::bail!(
            "virtio-win.iso sidecar hash {sha} does not match the pinned hash {} \
             (tampered drivers release or stale pin after a drivers-tag bump)",
            crate::pins::VIRTIO_WIN_SHA256
        );
    }
    Ok((url, sha))
}

/// Verify `path` is a real ISO image (ISO9660 `CD001` magic at sector 16).
/// Catches the classic failure where a bot-wall or error page (HTTP 200
/// HTML) gets saved as `.iso` — which later hangs EDK2 at boot with zero
/// block I/O and a black screen, far from the actual cause.
pub fn validate_iso_image(path: &Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    fn printable_head(path: &Path, len: usize) -> String {
        std::fs::read(path)
            .map(|b| {
                b.into_iter()
                    .take(len)
                    .map(|c| {
                        if c.is_ascii_graphic() || c == b' ' {
                            c as char
                        } else {
                            '·'
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < 32774 {
        anyhow::bail!(
            "{} is too small ({} bytes) to be an ISO image — starts with: {}; \
             likely a bot-check/error page saved as .iso — delete it and re-download",
            path.display(),
            len,
            printable_head(path, 120)
        );
    }
    let mut magic = [0u8; 5];
    file.seek(SeekFrom::Start(32769))
        .context("seek ISO magic")?;
    file.read_exact(&mut magic).context("read ISO magic")?;
    if &magic != b"CD001" {
        anyhow::bail!(
            "{} is not an ISO image ({} bytes, starts with: {}); \
             likely a bot-check/error page saved as .iso — delete it and re-download",
            path.display(),
            len,
            printable_head(path, 120)
        );
    }
    Ok(())
}
/// Resolve the concrete Eval ISO URL: explicit `--win-iso-url` wins, else
/// `$WIN11_EVAL_ISO_URL`. There is intentionally no page scraper — the
/// EvalCenter issues links behind an interactive, bot-guarded form, so the
/// only reliable source is a fresh, time-limited link copied from a browser
/// (or a community fetcher like Fido/Mido.sh — see README).
pub fn resolve_eval_iso_url(cli_url: Option<String>) -> Result<String> {
    if let Some(u) = cli_url {
        return Ok(u);
    }
    std::env::var("WIN11_EVAL_ISO_URL").map_err(|_| {
        anyhow::anyhow!(
            "no Eval ISO URL: pass --win-iso-url <url> or set WIN11_EVAL_ISO_URL.\n\
             Get a fresh link in your browser (EvalCenter -> Windows 11 Enterprise -> download; links are time-limited).\n\
             See README for Fido/Mido.sh alternatives."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn skips_existing_file_without_hash() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("virtio-win.iso");
        std::fs::write(&dest, b"fake-iso-bytes").unwrap();
        let status = download_if_missing("https://example.invalid/x.iso", &dest, None, false)
            .await
            .unwrap();
        assert_eq!(status, DownloadStatus::Cached { path: dest });
    }

    #[tokio::test]
    async fn skips_existing_file_with_matching_hash() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.iso");
        std::fs::write(&dest, b"abc").unwrap();
        // SHA-256 of "abc".
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let status = download_if_missing(
            "https://example.invalid/x.iso",
            &dest,
            Some(expected),
            false,
        )
        .await
        .unwrap();
        assert_eq!(status, DownloadStatus::Cached { path: dest.clone() });
        assert_eq!(sha256_of(&dest).await.unwrap(), expected);
    }

    /// Serve `body` once over loopback HTTP; returns the URL to fetch.
    async fn serve_once(body: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 8192];
                // Read the request (ignore content); then serve the body.
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            }
        });
        format!("http://{addr}/payload.bin")
    }

    #[tokio::test]
    async fn fresh_download_verifies_hash_and_cleans_tmp_on_mismatch() {
        // SHA-256 of "abc".
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        // Matching hash: downloads, verifies, renames into place.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("fresh.bin");
        let url = serve_once(b"abc").await;
        let status = download_if_missing(&url, &dest, Some(expected), false)
            .await
            .unwrap();
        match status {
            DownloadStatus::Downloaded { path, sha256 } => {
                assert_eq!(path, dest);
                assert_eq!(sha256, expected);
            }
            other => panic!("expected Downloaded, got {other:?}"),
        }
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");

        // Mismatching hash: bails BEFORE rename, removes the `.part` tmp,
        // and never creates `dest`.
        let dir2 = tempfile::tempdir().unwrap();
        let dest2 = dir2.path().join("bad.bin");
        let url2 = serve_once(b"abc").await;
        let err = download_if_missing(&url2, &dest2, Some(&"0".repeat(64)), false)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("SHA-256 mismatch"),
            "unexpected error: {err:#}"
        );
        assert!(!dest2.exists());
        assert!(!dest2.with_extension("part").exists());
    }

    #[tokio::test]
    async fn force_bypasses_valid_cache() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.iso");
        std::fs::write(&dest, b"abc").unwrap();
        // example.invalid never resolves: success would mean the cache
        // short-circuit fired, failure means force attempted the download.
        let err = download_if_missing("https://example.invalid/x.iso", &dest, None, true)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("example.invalid"),
            "unexpected error: {err:#}"
        );
        // Untouched: the failed forced download leaves no .part behind
        // masquerading as cache (download() targets the tmp sibling).
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
    }

    #[test]
    fn cache_dir_override_and_no_eviction() {
        // Single test owns the env var to avoid racing parallel tests.
        // (unsafe blocks: std::env mutation is process-global; tests are
        // the documented single-threaded-owner case here.)
        let tmp = tempfile::tempdir().unwrap();
        let override_root = tmp.path().join("custom-root");
        unsafe { std::env::set_var("WINDOWS_HEADLESS_ISO_DIR", &override_root) };

        let dir = iso_cache_dir().unwrap();
        assert_eq!(dir, override_root.join("iso"));
        assert!(dir.is_dir());

        // Previously cached entries survive re-resolution: no eviction policy
        // (no max_bytes / max_files / max_age) is ever applied.
        std::fs::write(dir.join("virtio-win.iso"), b"fake-iso-bytes").unwrap();
        let dir2 = iso_cache_dir().unwrap();
        assert_eq!(dir2, dir);
        assert_eq!(
            std::fs::read(dir2.join("virtio-win.iso")).unwrap(),
            b"fake-iso-bytes"
        );

        unsafe {
            std::env::remove_var("WINDOWS_HEADLESS_ISO_DIR");
        }
    }

    #[test]
    fn eval_url_resolution_prefers_flag_then_env() {
        unsafe {
            std::env::remove_var("WIN11_EVAL_ISO_URL");
        }
        assert!(resolve_eval_iso_url(None).is_err());
        unsafe {
            std::env::set_var("WIN11_EVAL_ISO_URL", "https://example.invalid/env.iso");
        }
        assert_eq!(
            resolve_eval_iso_url(None).unwrap(),
            "https://example.invalid/env.iso"
        );
        assert_eq!(
            resolve_eval_iso_url(Some("https://example.invalid/flag.iso".into())).unwrap(),
            "https://example.invalid/flag.iso"
        );
        unsafe {
            std::env::remove_var("WIN11_EVAL_ISO_URL");
        }
    }

    #[test]
    fn discovery_resolves_project_dot_cache() {
        // Read-only w.r.t. process globals: from_discovery only reads CWD.
        // `cargo test` runs with CWD at the package root, which contains
        // [workspace], so this must resolve to <repo>/.cache without I/O.
        let root = cache_manager::CacheRoot::from_discovery().unwrap();
        assert!(
            root.path().ends_with(".cache"),
            "got {}",
            root.path().display()
        );
    }

    #[test]
    fn progress_renders_with_and_without_total() {
        // 12.5 MiB of 34.7 MiB at ~8.3 MB/s.
        let line = format_progress("qemu.tar.gz", 13_107_200, Some(36_392_960), 1.5);
        assert_eq!(line, "qemu.tar.gz: 12.5 MB/34.7 MB (36%, 8.3 MB/s)");
        // Unknown total: fraction omitted, never divides by zero
        // (elapsed clamps to 1 ms).
        let line = format_progress("x.iso", 1_048_576, None, 0.0);
        assert_eq!(line, "x.iso: 1.0 MB (1000.0 MB/s)");
        let line = format_progress("x.iso", 1_048_576, Some(0), 2.0);
        assert_eq!(line, "x.iso: 1.0 MB (512.0 KB/s)");
        // Small files render in KB/B, large ones in GB.
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(2048), "2.0 KB");
        assert_eq!(fmt_size(8_589_934_592), "8.0 GB");
    }

    #[test]
    fn iso_magic_rejects_html_stubs() {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("bogus.iso");
        std::fs::write(&stub, b"<!doctype html>bot check").unwrap();
        assert!(validate_iso_image(&stub).is_err());
        // Minimal valid ISO9660: CD001 at offset 32769.
        let mut good = vec![0u8; 32774];
        good[32769..32774].copy_from_slice(b"CD001");
        let ok = dir.path().join("good.iso");
        std::fs::write(&ok, &good).unwrap();
        assert!(validate_iso_image(&ok).is_ok());
    }
}
