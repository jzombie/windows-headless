//! Single shared cache-root resolution for every cache group.
//!
//! Pre-patch behavior was six hand-rolled `match env::var(...)` blocks that
//! silently scattered `<cwd>/.cache` dirs for installed runs. Now one helper
//! built on `cache_manager::CacheResolver`: explicit `--dir` wins, then the
//! per-group env var, then Cargo workspace discovery, then the OS user cache
//! (`~/Library/Caches`, `$XDG_CACHE_HOME`, `%LOCALAPPDATA%`). No workspace +
//! no override = `Err`, never a silent CWD cache. The winner is announced via
//! `tracing::info!` (library-safe; the CLI installs an stderr subscriber, so
//! it shows by default and `RUST_LOG` overrides).
#[cfg(feature = "downloader")]
use anyhow::{Context, Result};
#[cfg(feature = "downloader")]
use cache_manager::{CacheResolver, CacheRoot};

/// Shared OS-cache identity for all windows-headless cache groups.
#[cfg(feature = "downloader")]
const OS_CACHE_QUALIFIER: &str = "com";
#[cfg(feature = "downloader")]
const OS_CACHE_ORGANIZATION: &str = "jzombie";
#[cfg(feature = "downloader")]
const OS_CACHE_APPLICATION: &str = "windows-headless";

/// Resolve the cache root for one group: `explicit` (CLI `--dir`) wins, then
/// `env_var` (e.g. `"WINDOWS_HEADLESS_QEMU_DIR"`), then workspace discovery,
/// then the OS user cache. Announces `cache: <path> (<source>)`.
#[cfg(feature = "downloader")]
pub fn resolve_cache_root(
    explicit: Option<std::path::PathBuf>,
    env_var: &str,
) -> Result<CacheRoot> {
    let resolver = CacheResolver {
        explicit,
        env_var: Some(env_var.to_string()),
        project_dirs: Some((
            OS_CACHE_QUALIFIER.to_string(),
            OS_CACHE_ORGANIZATION.to_string(),
            OS_CACHE_APPLICATION.to_string(),
        )),
        ..CacheResolver::default()
    };
    let (root, source) = resolver.resolve().with_context(|| {
        format!(
            "no cache location: set ${env_var} or run from inside the project checkout \
             (installed runs fall back to the OS user cache automatically)"
        )
    })?;
    tracing::info!("cache: {} ({source:?})", root.path().display());
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Hold the global test lock AND move CWD into a bare tmpdir (no
    /// Cargo.toml above), restoring both on drop. Serializes these tests
    /// against each other (env + CWD are process-global).
    struct BareCwd {
        previous: std::path::PathBuf,
        _lock: MutexGuard<'static, ()>,
    }

    impl BareCwd {
        fn enter() -> Self {
            let lock = test_lock().lock().expect("acquire test lock");
            let previous = std::env::current_dir().expect("current dir");
            let tmp = tempfile::tempdir().expect("tempdir");
            // Leak the TempDir path intentionally: the dir must survive for
            // the whole test while CWD points inside it.
            let bare = tmp.path().join("bare");
            std::fs::create_dir_all(&bare).expect("create bare");
            std::env::set_current_dir(&bare).expect("set cwd");
            // Forget the TempDir so the dir survives for the whole test while
            // CWD points inside it (removed when the process exits; tests only).
            std::mem::forget(tmp);
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for BareCwd {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }

    /// RAII guard for a process-global env var: captures the prior value on
    /// creation and restores it in `Drop`, so a panicking assertion can never
    /// poison the environment for later tests. Bind as `_env_guard` and keep
    /// it alive for the whole test.
    struct EnvGuard {
        name: String,
        prior: Option<String>,
    }

    impl EnvGuard {
        fn take(name: &str) -> Self {
            Self {
                name: name.to_string(),
                prior: std::env::var(name).ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: every test touching env holds the shared test lock via
            // `BareCwd`, so no two tests can race, and this exact var is
            // restored to its prior value (no other thread observes these
            // test-scoped names).
            unsafe {
                match &self.prior {
                    Some(v) => std::env::set_var(&self.name, v),
                    None => std::env::remove_var(&self.name),
                }
            }
        }
    }

    #[test]
    fn explicit_path_wins() {
        let _bare = BareCwd::enter();
        let dir = std::env::current_dir().unwrap().join("explicit-root");
        let root = resolve_cache_root(Some(dir.clone()), "WINDOWS_HEADLESS_CACHE_TEST_EXPLICIT")
            .expect("resolve");
        assert_eq!(root.path(), dir.as_path());
    }

    #[test]
    fn env_var_wins_over_os_cache() {
        let _bare = BareCwd::enter();
        let name = "WINDOWS_HEADLESS_CACHE_TEST_ENV";
        let _env_guard = EnvGuard::take(name);
        let want = std::env::current_dir().unwrap().join("env-root");
        // SAFETY: serialized by the test lock; test-scoped name, guard restores.
        unsafe { std::env::set_var(name, &want) };
        let root = resolve_cache_root(None, name).expect("resolve");
        assert_eq!(root.path(), want.as_path());
    }

    #[test]
    fn bare_cwd_falls_back_to_os_cache_never_cwd() {
        let _bare = BareCwd::enter();
        let name = "WINDOWS_HEADLESS_CACHE_TEST_OS";
        let _env_guard = EnvGuard::take(name);
        // SAFETY: serialized by the test lock; test-scoped name, guard restores.
        unsafe { std::env::remove_var(name) };
        let root = resolve_cache_root(None, name).expect("resolve");
        // Must NOT be <cwd>/.cache (the old silent scatter).
        assert!(
            !root.path().starts_with(std::env::current_dir().unwrap()),
            "scattered into CWD: {}",
            root.path().display()
        );
        assert!(
            root.path().to_string_lossy().contains("windows-headless"),
            "expected OS user cache path, got {}",
            root.path().display()
        );
    }
}
