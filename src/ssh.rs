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

/// Guest readiness probe as one `cmd` line: four `reg.exe` reads with
/// marker-delimited sections. `&& echo YES || echo NO` keeps
/// key-existence checks locale-independent; value parsing keys on the
/// (never localized) `REG_DWORD` token.
fn reg_probe_command() -> String {
    [
        "echo [SETUP]",
        "reg query \"HKLM\\SYSTEM\\Setup\" /v SetupType",
        "echo [OOBE]",
        "reg query \"HKLM\\SYSTEM\\Setup\" /v OOBEInProgress",
        "echo [CBS]",
        "reg query \"HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Component Based Servicing\\RebootPending\" && echo YES || echo NO",
        "echo [WU]",
        "reg query \"HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\WindowsUpdate\\Auto Update\\RebootRequired\" && echo YES || echo NO",
        "echo [END]",
    ]
    .join(" & ")
}

/// Split reg-probe output into its marker-delimited sections. Only the
/// exact markers this probe emits are boundaries — anything else brackety
/// in `reg` output is data, never structure.
const KNOWN_MARKERS: &[&str] = &["SETUP", "OOBE", "CBS", "WU", "END"];

fn probe_sections(text: &str) -> Option<std::collections::HashMap<&str, Vec<&str>>> {
    let mut sections = std::collections::HashMap::new();
    let mut current: Option<&str> = None;
    let mut seen_marker = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            let name = &t[1..t.len() - 1];
            if let Some(marker) = KNOWN_MARKERS.iter().find(|m| **m == name) {
                current = Some(*marker);
                if *marker != "END" {
                    seen_marker = true;
                }
                sections.entry(*marker).or_insert_with(Vec::new);
                continue;
            }
        }
        if let Some(c) = current {
            sections.entry(c).or_insert_with(Vec::new).push(t);
        }
    }
    if seen_marker { Some(sections) } else { None }
}

/// DWORD value from a `reg query /v` section: `None` when the value (or
/// key) is absent — absent OOBE counts as done, absent SetupType as busy.
fn probe_dword(section: &[&str]) -> Option<u32> {
    section
        .iter()
        .find(|l| l.contains("REG_DWORD"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|hex| {
            let clean = hex
                .strip_prefix("0x")
                .or_else(|| hex.strip_prefix("0X"))
                .unwrap_or(hex);
            u32::from_str_radix(clean, 16).ok()
        })
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
/// banner timing: four registry reads through `cmd`+`reg.exe` (millisecond
/// startup, no PowerShell cold start per probe).
///
/// Returns `Ok(true)` = finished, `Ok(false)` = still working,
/// `Err` = SSH/login/exec failure (caller treats as "not yet" — sshd may
/// be mid-restart during reboots). No marker lines at all means the probe
/// command itself broke (quoting, channel) — loud error, never a silent
/// `Ok(false)` limp.
pub async fn probe_setup_done(host: &str, port: u16, user: &str, pass: &str) -> Result<bool> {
    let text = ssh_exec(host, port, user, pass, &reg_probe_command()).await?;
    let sections = probe_sections(&text)
        .ok_or_else(|| anyhow::anyhow!("unparseable probe output: {text:?}"))?;
    let get = |name: &str| sections.get(name).map(Vec::as_slice).unwrap_or(&[]);
    // Absent key/value: OOBE counts as done (not started), everything
    // else as busy — matching the old PowerShell probe's verdicts.
    let setup = probe_dword(get("SETUP"));
    let oobe = probe_dword(get("OOBE")).unwrap_or(0);
    let cbs = get("CBS").contains(&"YES");
    let wu = get("WU").contains(&"YES");
    // CBS/WU sections must resolve to an explicit verdict; a section with
    // neither YES nor NO means the chain broke mid-command.
    for (name, lines) in [("CBS", get("CBS")), ("WU", get("WU"))] {
        if !lines.iter().any(|l| *l == "YES" || *l == "NO") {
            anyhow::bail!("probe section {name} has no verdict: {lines:?}");
        }
    }
    if setup == Some(0) && oobe == 0 && !cbs && !wu {
        return Ok(true);
    }
    let line = format!(
        "BUSY setuptype={} oobe={oobe} cbs={cbs} wu={wu}",
        setup.map_or("unknown".to_string(), |v| v.to_string())
    );
    eprintln!("guest busy: {line}");
    Ok(false)
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
    fn reg_probe_parses_sample_output() {
        // Shaped like real `reg.exe` output: value sections carry
        // REG_DWORD lines, key-existence sections resolve via YES/NO.
        let text = "[SETUP]\n    SetupType    REG_DWORD    0x0\n[OOBE]\nERROR: The system was unable to find the specified registry key or value.\n[CBS]\nERROR: The system was unable to find the specified registry key or value.\nNO\n[WU]\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\WindowsUpdate\\Auto Update\\RebootRequired\nYES\n[END]\n";
        let sections = probe_sections(text).unwrap();
        let get = |name: &str| sections.get(name).map(Vec::as_slice).unwrap_or(&[]);
        assert_eq!(probe_dword(get("SETUP")), Some(0));
        assert_eq!(probe_dword(get("OOBE")), None);
        assert!(get("CBS").contains(&"NO"));
        assert!(get("WU").contains(&"YES"));
        // Busy SetupType parses as decimal from hex.
        let busy = "[SETUP]\n    SetupType    REG_DWORD    0x2\n[OOBE]\n    OOBEInProgress    REG_DWORD    0x1\n[END]\n";
        let s2 = probe_sections(busy).unwrap();
        assert_eq!(
            probe_dword(s2.get("SETUP").map(Vec::as_slice).unwrap_or(&[])),
            Some(2)
        );
        // No markers at all: broken channel, not a verdict.
        assert!(probe_sections("garbage\nmore garbage\n").is_none());
        // The command itself is one &-chained line with all four queries.
        let cmd = reg_probe_command();
        assert!(!cmd.contains('\n'));
        for marker in ["[SETUP]", "[OOBE]", "[CBS]", "[WU]", "[END]"] {
            assert!(cmd.contains(marker), "missing {marker}");
        }
    }
}
