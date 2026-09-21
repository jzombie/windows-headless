//! ISO download helpers (feature `downloader`).
use anyhow::{Context, Result};
use cache_manager::CacheRoot;
use std::path::Path;

pub const STABLE_VIRTIO_URL: &str =
    "https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Stream `url` to `dest`, computing SHA-256 on the fly. Returns hex digest.
/// Always (re)downloads; see [`download_if_missing`] for the caching variant.
pub async fn download(url: &str, dest: &Path) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("reqwest client")?;
    let mut resp = client.get(url).send().await?.error_for_status()?;
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("create {}", dest.display()))?;
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;
    let mut hasher = sha2::Sha256::new();
    while let Some(chunk) = resp.chunk().await? {
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(hex::encode(hasher.finalize()))
}

/// Resolve (and create) the ISO cache dir via `cache-manager`.
///
/// - `$WINDOWS_HEADLESS_ISO_DIR` set: `<that>/iso`.
/// - Otherwise: `<project>/.cache/iso`, where `<project>` is the nearest
///   `Cargo.toml` workspace root discovered from the current directory
///   (so plain `cargo run` from this repo uses `./.cache`).
///
/// Eviction is intentionally **disabled**: the group is ensured with no
/// `EvictPolicy` (no `max_bytes`, no `max_files`, no `max_age`). ISOs are
/// multi-GB and the Eval URLs are time-limited, so size-capped eviction
/// would force surprise re-downloads.
pub fn iso_cache_dir() -> Result<std::path::PathBuf> {
    let root = match std::env::var("WINDOWS_HEADLESS_ISO_DIR") {
        Ok(dir) => CacheRoot::from_root(dir),
        Err(_) => CacheRoot::from_discovery().context("locate project cache root")?,
    };
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
///
/// Note: Eval-ISO URLs are time-limited, so callers should only pass
/// `expected_sha256` for stable artifacts like `virtio-win.iso`.
pub async fn download_if_missing(
    url: &str,
    dest: &Path,
    expected_sha256: Option<&str>,
) -> Result<DownloadStatus> {
    if let Ok(meta) = std::fs::metadata(dest)
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
    let sha256 = download(url, &tmp).await?;
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
        let status = download_if_missing("https://example.invalid/x.iso", &dest, None)
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
        let status = download_if_missing("https://example.invalid/x.iso", &dest, Some(expected))
            .await
            .unwrap();
        assert_eq!(status, DownloadStatus::Cached { path: dest.clone() });
        assert_eq!(sha256_of(&dest).await.unwrap(), expected);
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
        let root = CacheRoot::from_discovery().unwrap();
        assert!(
            root.path().ends_with(".cache"),
            "got {}",
            root.path().display()
        );
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
