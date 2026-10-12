//! Cross-platform IPC to QEMU's monitor (HMP) and QMP endpoints.
//!
//! Unix: unix-domain sockets. Windows: named pipes (QEMU's `pipe:` chardev
//! on the QEMU side, `interprocess` client on ours). One `Stream` type, one
//! `connect`, per-platform QEMU argv — all protocol code above this is
//! transport-agnostic.
//!
//! Single-client transports (Windows named pipes, and QEMU's single-slot
//! unix listen backlog while its main loop is busy in hvf init) mean two
//! rules, learned live: never probe-connect (a connect-and-drop occupies
//! the only slot and refuses the real handshake with ECONNREFUSED), and
//! never overlap sessions. [`Monitor`] enforces the second: one handle per
//! VM run, shared by reference with every task of that run.
//!
//! NOTE (Windows): compiled, never run here — no Windows host in this lab.
//! Pipe names and the `pipe:` argv form follow QEMU docs; expect iteration.

use anyhow::{Context, Result};
use std::time::Duration;

use crate::timeouts;

pub const MON_SOCK_NAME: &str = "qemu-run-mon.sock";
pub const QMP_SOCK_NAME: &str = "qemu-run-qmp.sock";
pub const SERIAL_LOG_NAME: &str = "runserial.log";

/// Byte stream to a QEMU endpoint: Unix-domain socket on Unix, named
/// pipe on Windows. One enum from `interprocess` — no platform branches
/// at use sites.
pub type Stream = interprocess::local_socket::tokio::Stream;

/// Connect to the endpoint at `path`: socket path in the VM dir on Unix
/// (filesystem-path name, used verbatim), pipe base name from
/// [`vm_endpoint`] on Windows (namespaced name).
pub async fn connect(path: &str) -> Result<Stream> {
    use interprocess::local_socket::traits::tokio::Stream as _;
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        use std::path::Path;
        let name = Path::new(path)
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| anyhow::anyhow!("bad socket path {path:?}: {e}"))?;
        Stream::connect(name)
            .await
            .with_context(|| format!("connect {path}"))
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        let name = path
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| anyhow::anyhow!("bad pipe name {path:?}: {e}"))?;
        Stream::connect(name)
            .await
            .with_context(|| format!("connect {path}"))
    }
}

/// Runtime dir for sockets: `$XDG_RUNTIME_DIR` on Linux (RAM, 0700,
/// cleared on reboot), `$TMPDIR` on macOS (per-user, private). Never the
/// VM dir itself: `~/.cache` lives on persistent (sometimes network)
/// storage, and `sun_path` caps socket paths at 104 bytes on macOS —
/// deep checkouts breach it. Per-VM subdirs preserve isolation.
#[cfg(unix)]
pub fn runtime_dir() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        return std::path::PathBuf::from(xdg);
    }
    if let Ok(tmp) = std::env::var("TMPDIR")
        && !tmp.is_empty()
    {
        return std::path::PathBuf::from(tmp).join("windows-headless");
    }
    // Last resort: per-user subdir of /tmp (0700 at creation, below).
    // `$USER` spoofing buys an attacker nothing they don't already have.
    let user = std::env::var("USER").unwrap_or_else(|_| "shared".into());
    std::path::PathBuf::from("/tmp").join(format!("windows-headless-{user}"))
}

/// Transport address for `name` scoped to `vm_dir`. Unix: socket file in
/// a per-VM subdir of [`runtime_dir`] (private, short, never NFS home —
/// only the dir *stem* travels, so deep checkouts can't breach
/// `sun_path`). Windows: bare pipe base name (flat `\\.\pipe\`
/// namespace; [`qemu_arg`] adds the prefix for QEMU's `pipe:` chardev).
pub fn vm_endpoint(vm_dir: &std::path::Path, name: &str) -> String {
    #[cfg(unix)]
    {
        let dir = runtime_dir().join(format!("wh-{}", vm_stem(vm_dir)));
        let _ = std::fs::create_dir_all(&dir);
        // 0700 the fallback so a shared /tmp can't be squatted.
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let full = dir.join(name);
        // Fail loudly, not at bind(2): macOS caps sun_path at 104 bytes.
        assert!(
            full.to_string_lossy().len() < 104,
            "socket path too long for sun_path: {}",
            full.display()
        );
        full.to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    {
        format!("wh-{}-{}", vm_stem(vm_dir), base_name(name))
    }
}

/// Sanitized VM dir stem: alphanumeric + `-`, truncated so full socket
/// paths stay far under `sun_path` limits.
fn vm_stem(vm_dir: &std::path::Path) -> String {
    let stem = vm_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "vm".into());
    let clean: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    clean.chars().take(32).collect()
}

/// `qemu-run-mon.sock` → `qemu-run-mon` (pipe names have no extensions).
#[cfg(windows)]
fn base_name(name: &str) -> String {
    name.trim_end_matches(".sock").replace('.', "-")
}

/// QEMU `-monitor`/`-qmp` arg for `path`: `unix:` vs `pipe:`.
pub fn qemu_arg(path: &str) -> String {
    #[cfg(unix)]
    {
        format!("unix:{path},server,nowait")
    }
    #[cfg(windows)]
    {
        format!(r"pipe:\\.\pipe\{path},server,nowait")
    }
}

/// Remove a stale endpoint. Named pipes aren't fs entries: no-op there.
pub fn stale_remove(path: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(path);
    }
    #[cfg(windows)]
    {
        let _ = path;
    }
}

/// True when something accepts on the endpoint (i.e. our QEMU is alive).
/// Advisory only: connect-and-drop, so never call it while a [`Monitor`]
/// or QMP session to the same endpoint is open on single-client transports
/// (it would steal the slot / report down). All our flows drop sessions
/// before probing — see `cmd_stop`.
pub async fn endpoint_alive(path: &str) -> bool {
    connect(path).await.is_ok()
}

/// Backwards-compatible name used across the codebase.
pub async fn qmp_alive(socket: &str) -> bool {
    endpoint_alive(socket).await
}

/// One HMP session per VM run. Holds the single connection behind a mutex
/// so the VNC handshake, screenshot task, and idle poll of the same run
/// serialize instead of overlapping (fatal on single-client transports).
/// Create after spawn, share by reference (`&Monitor`) with every task.
pub struct Monitor {
    sock: String,
    stream: tokio::sync::Mutex<Stream>,
}

impl Monitor {
    /// Connect once. Consumes the initial `(qemu)` greeting so [`Monitor::command`]
    /// can write-then-read on every call. For spawn races (slow hvf init)
    /// use [`Monitor::connect_retry`] instead.
    pub async fn connect(sock: &str) -> Result<Self> {
        let mut stream = connect(sock).await?;
        consume_greeting(&mut stream).await?;
        Ok(Self {
            sock: sock.to_string(),
            stream: tokio::sync::Mutex::new(stream),
        })
    }

    /// Keep connecting while `alive()` says the child lives, up to
    /// `timeout_secs`. Never fails fast on a live-but-slow QEMU — the old
    /// fixed-sleep spawn check murdered slow starters via `kill_on_drop`.
    pub async fn connect_retry(
        sock: &str,
        timeout_secs: u64,
        mut alive: impl FnMut() -> bool,
    ) -> Result<Self> {
        let mut waited = 0u64;
        loop {
            match connect(sock).await {
                Ok(mut stream) => {
                    // Greeting consumed here, once: every later command()
                    // is write-then-read on the same connection. (The first
                    // version re-read a greeting per command and hung
                    // forever waiting for bytes QEMU only sends after input.)
                    if let Err(e) = consume_greeting(&mut stream).await {
                        if !alive() {
                            anyhow::bail!("QEMU died during startup: {e:#}");
                        }
                        waited += timeouts::MONITOR_RETRY_STEP_SECS;
                        if waited >= timeout_secs {
                            anyhow::bail!(
                                "monitor at {sock} never greeted after {timeout_secs}s on a live QEMU ({e:#})"
                            );
                        }
                        // Retries are routine on slow starters (hvf/TCG):
                        // debug-only so the normal log shows one wait line.
                        tracing::debug!("monitor not greeted yet ({e:#}); retrying ...");
                        tokio::time::sleep(Duration::from_secs(timeouts::MONITOR_RETRY_STEP_SECS))
                            .await;
                        continue;
                    }
                    return Ok(Self {
                        sock: sock.to_string(),
                        stream: tokio::sync::Mutex::new(stream),
                    });
                }
                Err(e) => {
                    if !alive() {
                        anyhow::bail!("QEMU died during startup; monitor never came up: {e:#}");
                    }
                    waited += timeouts::MONITOR_RETRY_STEP_SECS;
                    if waited >= timeout_secs {
                        anyhow::bail!(
                            "monitor at {sock} not accepting after {timeout_secs}s on a live QEMU ({e:#})"
                        );
                    }
                    // Same as above: routine while QEMU starts, debug-only.
                    tracing::debug!("monitor not accepting yet ({e:#}); retrying ...");
                    tokio::time::sleep(Duration::from_secs(timeouts::MONITOR_RETRY_STEP_SECS))
                        .await;
                }
            }
        }
    }

    /// Endpoint path (for messages).
    pub fn sock(&self) -> &str {
        &self.sock
    }

    /// Run one HMP command; returns everything printed after it. The
    /// connection persists across commands, so this is write-then-read:
    /// the greeting was consumed at connect time. Reads raw bytes until
    /// the `(qemu)` marker: the prompt has no trailing newline, so
    /// line-based reads deadlock here (observed live).
    pub async fn command(&self, cmd: &str) -> Result<String> {
        use tokio::io::AsyncWriteExt;
        let mut stream = self.stream.lock().await;
        let mut buf = vec![0u8; 4096];
        let mut seen = Vec::new();
        stream
            .write_all(format!("{cmd}\n").as_bytes())
            .await
            .context("monitor write")?;
        stream.flush().await?;
        read_to_prompt(&mut stream, &mut buf, &mut seen).await?;
        Ok(String::from_utf8_lossy(&seen).into_owned())
    }
}

/// Read and discard bytes until the first `(qemu)` marker: the server
/// greeting, consumed once per connection.
async fn consume_greeting(stream: &mut Stream) -> Result<()> {
    let mut buf = vec![0u8; 4096];
    let mut seen = Vec::new();
    read_to_prompt(stream, &mut buf, &mut seen).await
}

async fn read_to_prompt(stream: &mut Stream, buf: &mut [u8], seen: &mut Vec<u8>) -> Result<()> {
    use tokio::io::AsyncReadExt;
    loop {
        let n = stream.read(buf).await.context("monitor read")?;
        if n == 0 {
            anyhow::bail!("monitor closed before prompt");
        }
        seen.extend_from_slice(&buf[..n]);
        if seen.windows(6).any(|w| w == b"(qemu)") {
            return Ok(());
        }
        if seen.len() > 65536 {
            anyhow::bail!("monitor greeting too long without prompt");
        }
    }
}
