use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

use crate::timeouts;

/// Poll `host:port` until a real SSH server answers. A bare TCP connect is
/// NOT enough: QEMU's user-mode NAT completes the host-side handshake while
/// the guest is still in UEFI/Setup, so `connect()` succeeds minutes before
/// any `sshd` exists (false "ready" + hung `ssh` client). We require the
/// `SSH-2.0-...` banner, which only a live sshd sends.
///
/// Default budget is caller-chosen; poll interval [`crate::timeouts::SSH_POLL_INTERVAL`].
/// On timeout the caller should `screendump` before failing (see canary test).
pub async fn wait_for_ssh(host: &str, port: u16, timeout_: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout_;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match TcpStream::connect((host, port)).await {
            Ok(stream) => {
                let mut reader = BufReader::new(stream);
                let mut banner = String::new();
                // Slirp may accept then stall; bound the banner wait so a
                // half-open forward degrades to another poll, not a hang.
                // Cap by the remaining budget so short timeouts stay short.
                let now = tokio::time::Instant::now();
                let budget = deadline
                    .saturating_duration_since(now)
                    .min(timeouts::SSH_BANNER_READ_BUDGET);
                match timeout(budget, reader.read_line(&mut banner)).await {
                    Ok(Ok(_)) if banner.starts_with("SSH-") => {
                        tracing::info!("SSH banner after {attempt} polls: {}", banner.trim());
                        return Ok(());
                    }
                    other => {
                        tracing::debug!("poll {attempt}: no SSH banner yet ({other:?})");
                    }
                }
            }
            Err(e) => {
                tracing::debug!("SSH poll {attempt} refused: {e:#}");
            }
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "timed out waiting for SSH on {host}:{port} after {attempt} polls — likely stuck on a GUI prompt"
            );
        }
        // Sleep the poll interval, capped at the remaining budget so short
        // timeouts (and tests) don't overshoot by a full interval. The
        // value is deliberately short (see `timeouts::SSH_POLL_INTERVAL`):
        // polling is cheap (one TCP SYN per cycle); latency is what the
        // user feels.
        sleep(timeouts::SSH_POLL_INTERVAL.min(deadline.saturating_duration_since(now))).await;
    }
}

// Readable PowerShell lives in templates/ (like autounattend.xml).
const PROBE_TPL: &str = include_str!("../templates/probe-ready.ps1");

/// Flatten the multiline probe asset into one exec-channel line:
/// strip comments/blank lines, join statements with `; `, backslash-escape
/// inner double quotes (Win32-OpenSSH routes exec through cmd, which strips
/// bare inner quotes — regressed live as "'BUSY' is not recognized").
fn flatten_probe(tpl: &str) -> String {
    tpl.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("; ")
        .replace('"', "\\\"")
}

/// Password-authenticated session (host key accepted blind — see
/// [`ssh_exec`]). Shared by exec, probe, and interactive shell.
struct AcceptAny;

impl russh::client::Handler for AcceptAny {
    type Error = anyhow::Error;
    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Loopback NAT to our own guest: TOFU is pointless, and the
        // host key changes on every reinstall. Never prompt.
        Ok(true)
    }
}

async fn authed_session(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
) -> Result<russh::client::Handle<AcceptAny>> {
    let config = Arc::new(russh::client::Config::default());
    let mut session = russh::client::connect(config, (host, port), AcceptAny).await?;
    session.authenticate_password(user, pass).await?;
    Ok(session)
}

/// Run one command over SSH (password auth), returning stdout.
/// Host key is accepted blind (loopback NAT to our own guest; the key
/// changes on every reinstall, so TOFU is pointless). Stderr lines are
/// echoed (PowerShell errors would otherwise vanish); the caller decides
/// what stdout means.
pub async fn ssh_exec(host: &str, port: u16, user: &str, pass: &str, cmd: &str) -> Result<String> {
    let session = authed_session(host, port, user, pass).await?;
    let mut ch = session.channel_open_session().await?;
    ch.exec(true, cmd).await?;
    // Bounded channel drain: PowerShell cold-start on a fresh boot can
    // stall tens of seconds, and a hung channel would otherwise burn the
    // caller's whole budget silently. Hard cap, then loud timeout.
    let out = tokio::time::timeout(timeouts::SSH_EXEC_DRAIN, async {
        let mut out = Vec::new();
        while let Some(msg) = ch.wait().await {
            match msg {
                russh::ChannelMsg::Data { data } => out.extend_from_slice(&data),
                russh::ChannelMsg::ExtendedData { data, .. } => {
                    // PowerShell parse errors land on stderr while stdout stays
                    // empty — without this, a broken command masquerades as
                    // empty output (observed live). Surface it loudly.
                    // NB: never log `cmd` itself here — it may carry a password.
                    eprintln!("ssh stderr: {}", String::from_utf8_lossy(&data).trim())
                }
                russh::ChannelMsg::ExitStatus { .. } => break,
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&out).into_owned())
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "exec timed out after {} s (PowerShell cold-start or hung channel)",
            timeouts::SSH_EXEC_DRAIN.as_secs()
        )
    })??;
    Ok(out)
}

/// Enable or disable Windows Automatic Repair ("Diagnosing your PC") on
/// the guest. Disabled = boots fail fast instead of parking at recovery;
/// snapshots are the safety net, not WinRE. Verified by reading the entry
/// back (`bcdedit /enum`) — same trust-nothing rule as `set_auto_updates`.
pub async fn set_recovery_enabled(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    enabled: bool,
) -> Result<()> {
    let val = if enabled { "Yes" } else { "No" };
    ssh_exec(
        host,
        port,
        user,
        pass,
        &format!("bcdedit /set {{current}} recoveryenabled {val}"),
    )
    .await?;
    let en = ssh_exec(host, port, user, pass, "bcdedit /enum {current}").await?;
    // `recoveryenabled    Yes|No` (whitespace varies).
    let ok = en.lines().any(|l| {
        let l = l.trim().to_ascii_lowercase();
        l.starts_with("recoveryenabled") && l.ends_with(&val.to_ascii_lowercase())
    });
    if !ok {
        anyhow::bail!("recoveryenabled {val} unverified (bcdedit /enum): {en:?}");
    }
    eprintln!(
        "recovery {val} (Automatic Repair {}, verified)",
        if enabled { "kept" } else { "disabled" }
    );
    Ok(())
}

/// Enable or disable Windows automatic updates (wuauserv) on the guest.
/// `false` stops the service and sets it to manual-start: no download,
/// no install, no surprise reboots. Manual updates over SSH still work.
/// Verifies by reading the config back (`sc qc`) — a silent `sc` failure
/// must never report success. Best-effort per command (warns, never fails)
/// — callers decide.
pub async fn set_auto_updates(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    enabled: bool,
) -> Result<()> {
    if enabled {
        ssh_exec(
            host,
            port,
            user,
            pass,
            "sc config wuauserv start= auto & sc start wuauserv",
        )
        .await?;
        let qc = ssh_exec(host, port, user, pass, "sc qc wuauserv").await?;
        if !qc.contains("AUTO_START") {
            anyhow::bail!("wuauserv enable unverified (sc qc): {qc:?}");
        }
        eprintln!("automatic updates enabled (wuauserv, verified)");
    } else {
        ssh_exec(
            host,
            port,
            user,
            pass,
            "sc config wuauserv start= disabled & sc stop wuauserv",
        )
        .await?;
        let qc = ssh_exec(host, port, user, pass, "sc qc wuauserv").await?;
        if !qc.contains("DISABLED") {
            anyhow::bail!("wuauserv disable unverified (sc qc): {qc:?}");
        }
        eprintln!("automatic updates disabled (wuauserv, verified)");
    }
    Ok(())
}

/// Rotate the guest password via `net user` over the existing SSH session,
/// then verify the new credential with a fresh login. Returns `Ok(())`
/// only after the new password authenticates — callers can treat success
/// as proof the rotation landed.
/// After success the old password is dead: only `new_pass` authenticates.
/// Never logs either password (see [`ssh_exec`]).
pub async fn change_guest_password(
    host: &str,
    port: u16,
    user: &str,
    old_pass: &str,
    new_pass: &str,
) -> Result<()> {
    // Hard budgets throughout: a stalled handshake must surface as an
    // error, never hang the caller (same lesson as the install wait loop).
    tokio::time::timeout(
        timeouts::SSH_PASSWORD_ROTATION_BUDGET,
        ssh_exec(
            host,
            port,
            user,
            old_pass,
            &format!("net user {user} \"{new_pass}\""),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("password change timed out"))??;
    let verified = tokio::time::timeout(
        timeouts::SSH_PASSWORD_ROTATION_BUDGET,
        probe_setup_done(host, port, user, new_pass),
    )
    .await
    .map_err(|_| anyhow::anyhow!("rotated-password login timed out"))??;
    if !verified {
        anyhow::bail!("rotated password rejected by guest");
    }
    Ok(())
}

/// Terminal type requested for interactive shells (`DEFAULT_PTY_TERM`).
/// Single source so the next person isn't grepping line 174 for it.
pub const DEFAULT_PTY_TERM: &str = "xterm";

/// Interactive shell over SSH: pty + local stdio, no host `ssh` binary.
/// Cross-platform raw mode via crossterm (restored on exit via the guard);
/// Ctrl-C inside goes to the guest, Ctrl-D / `exit` ends the session.
pub async fn interactive_shell(host: &str, port: u16, user: &str, pass: &str) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let session = authed_session(host, port, user, pass).await?;
    let mut ch = session.channel_open_session().await?;
    // xterm, not dumb: fullscreen apps (editors, pagers) query the term
    // type and emit garbage or refuse without one. Bytes pass through
    // untouched, so capabilities only grow.
    // Real terminal size (fixed 80x24 wraps into trash on bigger windows —
    // observed live); the loop below keeps it live.
    let mut term_size = crossterm::terminal::size().unwrap_or((80, 24));
    ch.request_pty(
        false,
        DEFAULT_PTY_TERM,
        term_size.0.into(),
        term_size.1.into(),
        0,
        0,
        &[],
    )
    .await?;
    ch.request_shell(false).await?;
    // Raw mode guard: console mode restored on drop, even on panic.
    struct RawGuard;
    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    crossterm::terminal::enable_raw_mode().map_err(|e| {
        anyhow::anyhow!("raw mode needs a terminal ({e:#}); refusing shell on piped stdio")
    })?;
    let _guard = RawGuard;
    eprintln!("shell open — Ctrl-D or `exit` to leave (Ctrl-C always goes to the guest)");
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut inbuf = vec![0u8; 1024];
    // No escape hatch, deliberately: every byte — including Ctrl-C, no
    // matter how often — belongs to the guest. An earlier triple-ETX
    // escape misfired on normal mashing and dropped users out of the
    // shell into the kill prompt (observed live). Leave via Ctrl-D/exit.
    // Resize ticker doubles as the loop's heartbeat: 500 ms lag max on
    // window changes, and no signal/event plumbing (crossterm events would
    // fight the raw stdin reader for the same fd).
    let mut resize_tick = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = resize_tick.tick() => {
                let cur = crossterm::terminal::size().unwrap_or(term_size);
                if cur != term_size {
                    term_size = cur;
                    let _ = ch.window_change(cur.0.into(), cur.1.into(), 0, 0).await;
                }
            }
            r = stdin.read(&mut inbuf) => {
                let n = r?;
                if n == 0 {
                    break; // Ctrl-D
                }
                ch.data(&inbuf[..n]).await?;
            }
            msg = ch.wait() => {
                match msg {
                    Some(russh::ChannelMsg::Data { data }) => {
                        stdout.write_all(&data).await?;
                        stdout.flush().await?;
                    }
                    Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                        stdout.write_all(&data).await?;
                        stdout.flush().await?;
                    }
                    Some(russh::ChannelMsg::ExitStatus { .. }) | None => break,
                    _ => {}
                }
            }
        }
    }
    // Drain strays typed during the handoff (arrow keys, extra Ctrl-C):
    // cooked mode is back, but bytes already in flight would echo as
    // trash (observed live as ^[[A^C) into the next prompt.
    while crossterm::event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
        let _ = crossterm::event::read();
    }
    Ok(())
}

/// Guest readiness probe over a real SSH login (password auth). Asks the
/// guest itself whether Setup is finished instead of inferring it from
/// banner timing:
///
/// - `HKLM\SYSTEM\Setup`: `SetupType == 0`, no `OOBEInProgress`.
/// - No CBS `RebootPending`, no WindowsUpdate `RebootRequired`.
///
/// Returns `Ok(true)` = finished, `Ok(false)` = still working (with the
/// reason echoed to stderr by the caller), `Err` = SSH/login/exec failure
/// (caller treats as "not yet" — sshd may be mid-restart during reboots).
pub async fn probe_setup_done(host: &str, port: u16, user: &str, pass: &str) -> Result<bool> {
    // The asset stays human-readable multiline; the exec channel wants a
    // single line — see flatten_probe() for the why behind each step.
    let one_line = flatten_probe(PROBE_TPL);
    let text = ssh_exec(
        host,
        port,
        user,
        pass,
        &format!("powershell -NoProfile -Command \"{one_line}\""),
    )
    .await?;
    let line = text
        .lines()
        .find(|l| l.contains("READY") || l.contains("BUSY"))
        .unwrap_or("");
    if line.contains("READY") {
        return Ok(true);
    }
    if line.contains("BUSY") {
        eprintln!("guest busy: {}", line.trim());
        return Ok(false);
    }
    // No verdict line at all: the probe itself is broken (quoting, asset
    // drift), not the guest. Loud error — a silent Ok(false) here once
    // let a broken probe limp on until a fluke READY declared victory.
    anyhow::bail!("unparseable probe output: {text:?}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fast_fail_on_closed_port() {
        let err = wait_for_ssh("127.0.0.1", 1, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"));
    }

    #[tokio::test]
    async fn succeeds_on_ssh_banner() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(b"SSH-2.0-OpenSSH_for_Windows_9.5\r\n")
                .await
                .unwrap();
            // Hold open briefly so the reader sees EOF only after the banner.
            sleep(Duration::from_secs(5)).await;
        });
        wait_for_ssh("127.0.0.1", port, Duration::from_secs(30))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn silent_accept_is_not_ready() {
        // Regression: QEMU slirp completes TCP while the guest has no sshd.
        // A listener that accepts but never sends a banner must NOT resolve.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
            }
        });
        let err = wait_for_ssh("127.0.0.1", port, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"));
    }

    /// Live guest probe: needs the proven VM up (`install` finished,
    /// SSH on 2232 with default creds). Run explicitly:
    /// `cargo test -- --ignored live_probe_reports_state`.
    #[tokio::test]
    #[ignore]
    async fn live_probe_reports_state() {
        let done = probe_setup_done("127.0.0.1", 2232, "AdminUser", "P@ssw0rd123!")
            .await
            .unwrap();
        eprintln!("guest setup done: {done}");
    }

    /// Live exec check (read-only): same transport the password rotation
    /// uses. `cargo test -- --ignored live_exec_whoami`.
    #[tokio::test]
    #[ignore]
    async fn live_exec_whoami() {
        let out = ssh_exec("127.0.0.1", 2232, "AdminUser", "P@ssw0rd123!", "whoami")
            .await
            .unwrap();
        assert!(out.contains("adminuser"), "unexpected whoami: {out:?}");
    }

    #[test]
    fn probe_flattens_without_bare_quotes() {
        // Regression: the templated probe once shipped unescaped inner
        // quotes; cmd stripped them and PowerShell died with
        // "'BUSY' is not recognized as the name of a cmdlet".
        let flat = flatten_probe(PROBE_TPL);
        assert!(!flat.contains('\n'), "newlines break exec parsing");
        // Every double quote must be backslash-escaped (cmd strips bare
        // inner quotes). Registry backslashes (HKLM:\...) are fine.
        let mut prev = '\0';
        for c in flat.chars() {
            if c == '"' {
                assert_eq!(prev, '\\', "bare double quote in flattened probe");
            }
            prev = c;
        }
        // Statements separated (space-join once merged them into one
        // command: "positional parameter ... '='" parse error).
        assert!(flat.contains("; "), "statements must join with '; '");
        assert!(flat.contains("READY"), "flattening must keep the verdicts");
        assert!(flat.contains("BUSY"), "flattening must keep the verdicts");
    }
}
