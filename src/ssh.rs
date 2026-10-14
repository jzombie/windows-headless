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
///
/// WIRE RULE (OxDock transport): the guest builds the process command line
/// by joining argv, which backslash-escapes every `"` and doubles every
/// `%`. So this command must contain NO double quotes and NO `%%`:
/// - Unquoted `HKLM\...` paths (no spaces in these four — the two spaced
///   keys go through `set` variables + `call`, which needs single `%`).
/// - `set K=<spaced key>` + `call reg query %K%`: `call` forces a second
///   parse pass so the value (expanded then) is read correctly.
///
/// TODO: revert to plain quoted `reg query "HKLM\..."` paths once
/// rust-oxdock joins argv Windows-style (no `\"` mangling) and the
/// OXDOCK_RELEASE_TAG pin moves past the fix; then delete the `set`/`call`
/// dance and re-prove with `guest probe` live.
/// Every query sends its stderr to `nul`: a missing value/key is a *verdict*
/// here (OOBE not started, no servicing reboot pending), not a failure —
/// letting it through logged "ERROR: The system was unable to find the
/// specified registry key or value." twice on every healthy probe, which
/// read like a broken install.
fn reg_probe_command() -> String {
    [
        "echo [SETUP]",
        "reg query HKLM\\SYSTEM\\Setup /v SetupType 2>nul",
        "echo [OOBE]",
        "reg query HKLM\\SYSTEM\\Setup /v OOBEInProgress 2>nul",
        "echo [CBS]",
        "set WHCBSK=HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Component Based Servicing\\RebootPending& call reg query %WHCBSK% 2>nul && echo YES || echo NO",
        "echo [WU]",
        "set WHWUK=HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\WindowsUpdate\\Auto Update\\RebootRequired& call reg query %WHWUK% 2>nul && echo YES || echo NO",
        "echo [END]",
    ]
    .join(" & ")
}

/// Split reg-probe output into its marker-delimited sections. Markers are
/// matched as SUBSTRINGS in fixed order, not as whole lines: the ConPTY
/// sometimes glues a marker onto the previous line's tail (`0x0[OOBE]`
/// observed live — reg's trailing newline lost in the pty), and a
/// line-exact match would drop the whole section. Only the exact markers
/// this probe emits are boundaries — anything else brackety in `reg`
/// output is data, never structure.
///
/// TODO: back this to line-exact matching if the pty ever delivers
/// intact line discipline; the glued-marker regression test pins the
/// current behavior either way.
const KNOWN_MARKERS: &[&str] = &["SETUP", "OOBE", "CBS", "WU", "END"];

fn probe_sections(text: &str) -> Option<std::collections::HashMap<&str, Vec<&str>>> {
    let mut sections = std::collections::HashMap::new();
    let mut rest = text;
    let mut current: Option<&str> = None;
    let mut seen_any = false;
    for marker in KNOWN_MARKERS {
        let tag = format!("[{marker}]");
        let Some(pos) = rest.find(&tag) else {
            break;
        };
        seen_any = true;
        if let Some(cur) = current {
            sections
                .entry(cur)
                .or_insert_with(Vec::new)
                .extend(rest[..pos].lines().map(str::trim));
        }
        if *marker != "END" {
            sections.entry(*marker).or_insert_with(Vec::new);
        }
        current = Some(*marker);
        rest = &rest[pos + tag.len()..];
    }
    if !seen_any {
        return None;
    }
    // Trailing bytes after the last marker belong to it (a missing [END]
    // still leaves a parseable tail).
    if let Some(cur) = current {
        sections
            .entry(cur)
            .or_insert_with(Vec::new)
            .extend(rest.lines().map(str::trim));
    }
    Some(sections)
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
        _key: &russh::keys::PublicKeyOrCertificate,
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
/// changes on every reinstall, so TOFU is pointless).
///
/// Stderr is not echoed unconditionally: healthy guests produce plenty of
/// it (the readiness probe reads registry keys that legitimately don't
/// exist yet), and printing every line made normal runs look broken. It is
/// reported loudly only in the case that used to bite — empty stdout, where
/// a broken command would otherwise masquerade as empty output — and
/// otherwise at `tracing::debug!` (`RUST_LOG=debug`). The caller decides
/// what stdout means.
///
/// NB: never log `cmd` itself — it may carry a password.
///
/// WIRE RULE (OxDock transport): every exec runs as `cmd.exe /c <cmd>`
/// where `<cmd>` crossed a CreateProcess argv join that backslash-escapes
/// `"` and doubles `%`. Callers must therefore send commands with NO `"`
/// and NO `%%` (see `reg_probe_command`, `ps_encoded_command`). What does
/// NOT survive is fixed here, not per caller.
///
/// TODO: drop this rule (and the quote-free rewrites below) once rust-oxdock
/// passes argv through verbatim; re-verify with `guest probe` + a spaced
/// password rotation live after the pin bump.
///
/// ConPTY decor (cursor addressing, DSR replies, title sets) is stripped
/// from both streams: parsers see command output, not terminal paint.
pub async fn ssh_exec(host: &str, port: u16, user: &str, pass: &str, cmd: &str) -> Result<String> {
    let session = authed_session(host, port, user, pass).await?;
    let mut ch = session.channel_open_session().await?;
    ch.exec(true, cmd).await?;
    // Bounded channel drain: PowerShell cold-start on a fresh boot can
    // stall tens of seconds, and a hung channel would otherwise burn the
    // caller's whole budget silently. Hard cap, then loud timeout.
    let (out, err) = tokio::time::timeout(timeouts::SSH_EXEC_DRAIN, async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        while let Some(msg) = ch.wait().await {
            match msg {
                russh::ChannelMsg::Data { data } => out.extend_from_slice(&data),
                russh::ChannelMsg::ExtendedData { data, .. } => err.extend_from_slice(&data),
                russh::ChannelMsg::ExitStatus { .. } => break,
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>((out, err))
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "exec timed out after {} s (PowerShell cold-start or hung channel)",
            timeouts::SSH_EXEC_DRAIN.as_secs()
        )
    })??;
    let out = strip_ansi(&out);
    let err = strip_ansi(&err);
    match loud_stderr_message(&out, &err) {
        Some(msg) => eprintln!("{msg}"),
        None if !err.is_empty() => {
            tracing::debug!("guest stderr: {}", String::from_utf8_lossy(&err).trim())
        }
        None => {}
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Strip ANSI/VT escape sequences from a byte stream: CSI (`ESC [ ...
/// final`), OSC (`ESC ] ... BEL` / `ESC \`), charset designators, stray
/// ESC and BEL. OxDock runs every exec under a ConPTY, so even `reg.exe`
/// arrives wrapped in cursor addressing, DSR replies, and title sets —
/// parsers (`probe_sections`, `bcdedit_value_verified`) must not see them.
/// Plain text (including `\r\n`) passes through untouched.
///
/// TODO: drop this if the server ever offers pty-less exec (no terminal,
/// no paint); until then every ssh_exec caller depends on it.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Stray BEL (ConPTY title-set residue): never command output.
            0x07 => {
                i += 1;
            }
            0x1b if i + 1 < bytes.len() => match bytes[i + 1] {
                // CSI: ESC [ params... final byte 0x40..=0x7E.
                b'[' => {
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7E).contains(&bytes[j]) {
                        j += 1;
                    }
                    i = (j + 1).min(bytes.len());
                }
                // OSC: ESC ] ... BEL or ESC \.
                b']' => {
                    let mut j = i + 2;
                    while j < bytes.len() {
                        if bytes[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if bytes[j] == 0x1b
                            && j + 1 < bytes.len()
                            && bytes[j + 1] == b'\\'
                        {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                }
                // Charset designation / misc two-byte sequences.
                b'(' | b')' | b'#' | b'=' | b'>' => {
                    i = (i + 3).min(bytes.len());
                }
                // Lone ESC (truncated sequence at buffer end, etc.): drop.
                _ => {
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Wrap an arbitrary PowerShell script for the wire: `-EncodedCommand`
/// takes base64 UTF-16LE, whose alphabet (`A–Z a–z 0–9 + / =`) contains
/// no `"`, no `%`, no spaces — nothing the argv join can mangle. The
/// decoded script may contain anything (quotes, spaces, `%`, `!`), so
/// this is the channel for commands that cannot be written quote-free
/// (password rotation, cmdlet pipelines).
///
/// TODO: revert diet/rotation to plain quoted `-Command` strings once the
/// argv join is verbatim (see WIRE RULE above); keep one encoded call as
/// the arbitrary-password path only if rotation still needs it.
fn ps_encoded_command(script: &str) -> String {
    use base64::Engine;
    let wide: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    format!(
        "powershell -NoProfile -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(&wide)
    )
}

/// The one stderr report worth printing at the default level: `Some` only
/// when stderr changes how the result reads (empty stdout = a broken
/// command that would otherwise look like an empty answer). Routine stderr
/// alongside real stdout is `None` here and goes to `RUST_LOG=debug`.
fn loud_stderr_message(out: &[u8], err: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(err);
    let text = text.trim();
    if text.is_empty() || !out.iter().all(|b| b.is_ascii_whitespace()) {
        return None;
    }
    Some(format!("guest command produced no stdout — stderr: {text}"))
}

/// One `bcdedit /enum` line check: `key ... want` (whitespace varies).
fn bcdedit_value_verified(out: &str, key: &str, want: &str) -> bool {
    out.lines().any(|l| {
        let l = l.trim().to_ascii_lowercase();
        l.starts_with(key) && l.ends_with(&want.to_ascii_lowercase())
    })
}

/// Headless service diet + NLA profile, applied once over SSH after the
/// guest reports Ready — deliberately NOT a specialize step: each list
/// used to be its own synchronous PowerShell cold start serializing into
/// Setup. Two invocations here cost two cold starts off the critical
/// path. `-EncodedCommand` throughout: cmdlet pipelines need `|` (fatal
/// unquoted through `cmd`, fatal quoted through the argv join), so the
/// scripts travel as base64 instead. Best-effort (warn, never fail):
/// Setup is done, this is hygiene.
/// Defender real-time monitoring is tamper-locked headless — not attempted.
pub async fn apply_headless_diet(host: &str, port: u16, user: &str, pass: &str) -> Result<()> {
    const SERVICES: &str = "Spooler,WSearch,SysMain,DiagTrack,WerSvc,BITS,DoSvc,wisvc,RetailDemo,MapsBroker,Fax,WbioSrvc,PhoneSvc,RemoteRegistry,lfsvc,XblAuthManager,XblGameSave,dmwappushservice";
    for script in [
        "Get-NetConnectionProfile | Set-NetConnectionProfile -NetworkCategory Private",
        &format!(
            "Get-Service {SERVICES} | Set-Service -StartupType Disabled -ErrorAction SilentlyContinue"
        ),
    ] {
        ssh_exec(host, port, user, pass, &ps_encoded_command(script)).await?;
    }
    Ok(())
}

/// Enable or disable Windows Automatic Repair ("Diagnosing your PC") on
/// the guest. Disabled = boots fail fast instead of parking at recovery;
/// snapshots are the safety net, not WinRE. Toggles BOTH BCD switches:
/// `recoveryenabled` gates WinRE invocation, while the "Diagnosing your
/// PC" screen itself is the boot-status policy (`bootstatuspolicy`) —
/// IgnoreAllFailures makes winload skip it after an unclean stop.
/// Verified by reading the entry back (`bcdedit /enum`) — same
/// trust-nothing rule as `set_auto_updates`.
pub async fn set_recovery_enabled(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    enabled: bool,
) -> Result<()> {
    let val = if enabled { "Yes" } else { "No" };
    let policy = if enabled {
        "DisplayAllFailures"
    } else {
        "IgnoreAllFailures"
    };
    ssh_exec(
        host,
        port,
        user,
        pass,
        &format!("bcdedit /set {{current}} recoveryenabled {val}"),
    )
    .await?;
    ssh_exec(
        host,
        port,
        user,
        pass,
        &format!("bcdedit /set {{current}} bootstatuspolicy {policy}"),
    )
    .await?;
    let en = ssh_exec(host, port, user, pass, "bcdedit /enum {current}").await?;
    if !bcdedit_value_verified(&en, "recoveryenabled", val) {
        anyhow::bail!("recoveryenabled {val} unverified (bcdedit /enum): {en:?}");
    }
    if !bcdedit_value_verified(&en, "bootstatuspolicy", policy) {
        anyhow::bail!("bootstatuspolicy {policy} unverified (bcdedit /enum): {en:?}");
    }
    eprintln!(
        "recovery {val} (Automatic Repair {}, boot status policy {policy}, verified)",
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
    // `-EncodedCommand`: the password is arbitrary text (spaces, quotes,
    // `%`, `!`) that no amount of cmd-level quoting can carry intact, so
    // it travels as base64 and only materializes inside PowerShell.
    let script = format!(
        "Set-LocalUser -Name \"{user}\" -Password (ConvertTo-SecureString \"{new_pass}\" -AsPlainText -Force)"
    );
    tokio::time::timeout(
        timeouts::SSH_PASSWORD_ROTATION_BUDGET,
        ssh_exec(host, port, user, old_pass, &ps_encoded_command(&script)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("password change timed out"))??;
    let verified = tokio::time::timeout(
        timeouts::SSH_PASSWORD_ROTATION_BUDGET,
        probe_setup_done(host, port, user, new_pass),
    )
    .await
    .map_err(|_| anyhow::anyhow!("rotated-password login timed out"))??;
    if !verified.is_ready() {
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

/// Prove a usable interactive shell, not just an exec channel: open a
/// real pty shell (the exact path a user login takes — profile, prompt,
/// the lot), send a marker, and read it back. An exec `whoami` passing
/// while interactive logon hangs is a real failure mode (profile load,
/// shell startup); only a prompt answering proves the prompt exists.
/// Bounded by [`timeouts::READINESS_PROBE_BUDGET`]: answers in milliseconds
/// on a landed guest, loud error otherwise — never a hang.
pub async fn prove_interactive_shell(host: &str, port: u16, user: &str, pass: &str) -> Result<()> {
    const MARKER: &str = "GOLDEN_SHELL_PROBE_READY";
    let session = authed_session(host, port, user, pass).await?;
    let mut ch = session.channel_open_session().await?;
    ch.request_pty(false, DEFAULT_PTY_TERM, 80, 24, 0, 0, &[])
        .await?;
    ch.request_shell(false).await?;
    ch.data(format!("echo {MARKER}\r\n").as_bytes()).await?;
    tokio::time::timeout(timeouts::READINESS_PROBE_BUDGET, async {
        let mut seen = Vec::new();
        loop {
            match ch.wait().await {
                Some(russh::ChannelMsg::Data { data })
                | Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                    seen.extend_from_slice(&data);
                    if seen.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()) {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
                Some(russh::ChannelMsg::ExitStatus { .. }) | None => {
                    anyhow::bail!("shell closed before answering");
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "no shell prompt within {} s — interactive logon is not usable",
            timeouts::READINESS_PROBE_BUDGET.as_secs()
        )
    })??;
    Ok(())
}

/// What the guest itself reports about Setup: finished, or still working —
/// with a human-readable description of what is pending. Callers narrate
/// that description only when it changes, so a long install logs a handful
/// of progress lines instead of one per probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestProgress {
    Ready,
    /// e.g. `Windows Setup in progress (SetupType=2), OOBE pending`.
    Busy(String),
}

impl GuestProgress {
    pub fn is_ready(&self) -> bool {
        matches!(self, GuestProgress::Ready)
    }

    /// Human-readable verdict detail: `"ready"`, or what is pending.
    pub fn describe(&self) -> &str {
        match self {
            GuestProgress::Ready => "ready",
            GuestProgress::Busy(s) => s,
        }
    }
}

/// The guest's answer could not be interpreted: a defect in our probe
/// command or in parsing its output — NOT a guest reboot. Typed so waiters
/// report it as what it is instead of narrating a reboot that never happened.
#[derive(Debug)]
pub struct ProbeOutputBroken(pub String);

impl std::fmt::Display for ProbeOutputBroken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProbeOutputBroken {}

/// Guest readiness probe over a real SSH login (password auth). Asks the
/// guest itself whether Setup is finished instead of inferring it from
/// banner timing: four registry reads through `cmd`+`reg.exe` (millisecond
/// startup, no PowerShell cold start per probe).
///
/// Returns [`GuestProgress::Ready`] = finished, [`GuestProgress::Busy`] =
/// still working (with what is pending), `Err` = SSH/login/exec failure
/// (caller treats as "not yet" — sshd may be mid-restart during reboots).
/// No marker lines at all means the probe command itself broke (quoting,
/// channel) — a [`ProbeOutputBroken`] error, never a silent `Busy` limp.
pub async fn probe_setup_done(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
) -> Result<GuestProgress> {
    let text = ssh_exec(host, port, user, pass, &reg_probe_command()).await?;
    let sections = probe_sections(&text).ok_or_else(|| {
        anyhow::Error::new(ProbeOutputBroken(format!(
            "unparseable probe output: {text:?}"
        )))
    })?;
    progress_from_sections(&sections).map_err(|e| {
        // Name the section AND show what the guest actually sent: a missing
        // marker vs a missing verdict are different failures (ConPTY noise
        // eats lines; a broken query eats values), and the raw bytes tell
        // them apart instantly.
        if e.downcast_ref::<ProbeOutputBroken>().is_some() {
            anyhow::Error::new(ProbeOutputBroken(format!("{e:#} — raw: {text:?}")))
        } else {
            e
        }
    })
}

/// Verdict from already-parsed probe sections (pure — unit-tested; the SSH
/// transport lives in [`probe_setup_done`]).
fn progress_from_sections(
    sections: &std::collections::HashMap<&str, Vec<&str>>,
) -> Result<GuestProgress> {
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
            return Err(anyhow::Error::new(ProbeOutputBroken(format!(
                "probe section {name} has no verdict: {lines:?}"
            ))));
        }
    }
    if setup == Some(0) && oobe == 0 && !cbs && !wu {
        return Ok(GuestProgress::Ready);
    }
    // The raw tuple is diagnostic detail, not a progress report.
    tracing::debug!("guest probe busy: setup={setup:?} oobe={oobe} cbs={cbs} wu={wu}");
    Ok(GuestProgress::Busy(pending_description(
        setup, oobe, cbs, wu,
    )))
}

/// Phrasing for a not-ready verdict: exactly the four values the probe
/// read, never a guess about what Windows is doing beyond them.
fn pending_description(setup: Option<u32>, oobe: u32, cbs: bool, wu: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    match setup {
        Some(0) => {}
        Some(v) => parts.push(format!("Windows Setup in progress (SetupType={v})")),
        None => parts.push("Setup state unreadable".into()),
    }
    if oobe != 0 {
        parts.push("OOBE pending".into());
    }
    if cbs {
        parts.push("component-servicing reboot pending".into());
    }
    if wu {
        parts.push("Windows Update reboot pending".into());
    }
    if parts.is_empty() {
        parts.push("finishing up".into());
    }
    parts.join(", ")
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
        let state = probe_setup_done("127.0.0.1", 2232, "AdminUser", "P@ssw0rd123!")
            .await
            .unwrap();
        eprintln!("guest setup state: {state:?}");
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

    #[test]
    fn probe_sections_survive_glued_markers() {
        // Live ConPTY output 2026-10-14: reg's trailing newline lost, so
        // markers arrive glued to the previous value (`0x0[OOBE]`). A
        // line-exact match drops the section and the probe goes blind
        // ("CBS has no verdict") on a healthy guest.
        let text = "[SETUP] \r\n\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\Setup\r\n    SetupType    REG_DWORD    0x0[OOBE] HKEY_LOCAL_MACHINE\\SYSTEM\\Setup\r\n    OOBEInProgress    REG_DWORD    0x0[CBS] \r\nNO \r\n[WU] \r\nNO \r\n[END]\r\n";
        let sections = probe_sections(text).unwrap();
        assert!(progress_from_sections(&sections).unwrap().is_ready());
    }

    #[test]
    fn probe_verdicts_read_as_progress_not_failure() {
        // Ready: everything quiet.
        let ready = "[SETUP]\n    SetupType    REG_DWORD    0x0\n[OOBE]\n    OOBEInProgress    REG_DWORD    0x0\n[CBS]\nNO\n[WU]\nNO\n[END]\n";
        let sections = probe_sections(ready).unwrap();
        assert!(progress_from_sections(&sections).unwrap().is_ready());

        // Typical mid-install: one calm line naming exactly what was read.
        let mid = "[SETUP]\n    SetupType    REG_DWORD    0x2\n[OOBE]\n    OOBEInProgress    REG_DWORD    0x1\n[CBS]\nNO\n[WU]\nNO\n[END]\n";
        let sections = probe_sections(mid).unwrap();
        assert_eq!(
            progress_from_sections(&sections).unwrap(),
            GuestProgress::Busy("Windows Setup in progress (SetupType=2), OOBE pending".into())
        );

        // A section with no verdict means OUR probe broke: typed so waiters
        // report a probe problem instead of narrating a guest reboot.
        let broken = "[SETUP]\n    SetupType    REG_DWORD    0x0\n[OOBE]\n[CBS]\n[WU]\nNO\n[END]\n";
        let sections = probe_sections(broken).unwrap();
        let err = progress_from_sections(&sections).unwrap_err();
        assert!(
            err.downcast_ref::<ProbeOutputBroken>().is_some(),
            "want ProbeOutputBroken, got {err:#}"
        );
    }

    #[test]
    fn probe_command_suppresses_expected_stderr() {
        // A missing value/key is a *verdict* here (OOBE not started, no
        // servicing reboot), not an error: every query must redirect stderr
        // or a healthy install logs "ERROR: unable to find ..." twice per
        // probe and reads like a broken one.
        let cmd = reg_probe_command();
        let queries = cmd.matches("reg query").count();
        assert_eq!(queries, 4, "expected four reg queries: {cmd}");
        assert_eq!(cmd.matches("2>nul").count(), queries, "unsilenced: {cmd}");
    }

    /// Transport rule, pinned: the guest argv join backslash-escapes `"`
    /// and doubles `%`, so NO guest command may contain `"` or `%%`
    /// (single `%` for `call` deferred expansion is fine — the join
    /// doubling is exactly what `call` needs). A `"` here means reg
    /// looks up a `\"...`-prefixed key and the probe goes blind.
    #[test]
    fn guest_commands_are_wire_safe() {
        let cmds = [
            reg_probe_command(),
            "powershell -NoProfile -Command Stop-Computer -Force".to_string(),
            ps_encoded_command("Get-Service Spooler | Set-Service -StartupType Disabled"),
            ps_encoded_command("Set-LocalUser -Name \"u\" -Password (ConvertTo-SecureString \"p@ss w%!\" -AsPlainText -Force)"),
        ];
        for cmd in &cmds {
            assert!(!cmd.contains('"'), "double quote on the wire: {cmd}");
            assert!(!cmd.contains("%%"), "doubled percent on the wire: {cmd}");
            assert!(!cmd.contains('\n'), "multiline command: {cmd}");
        }
    }

    #[test]
    fn ps_encoded_command_round_trips_any_chars() {
        // Passwords with spaces, quotes, %, ! survive: only base64 alphabet
        // crosses the wire, the secret materializes inside PowerShell.
        let script = "Set-LocalUser -Name \"AdminUser\" -Password (ConvertTo-SecureString \"P@ss w%!\\\"\" -AsPlainText -Force)";
        let cmd = ps_encoded_command(script);
        assert!(cmd.starts_with("powershell -NoProfile -EncodedCommand "));
        let b64 = cmd.split_whitespace().last().unwrap();
        assert!(!b64.contains(['"', '%', ' ', '&', '|']));
        use base64::Engine;
        let wide = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let (chunks, rest) = wide.as_chunks::<2>();
        assert!(rest.is_empty());
        let units: Vec<u16> = chunks
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), script);
    }

    #[test]
    fn strip_ansi_keeps_text_drops_paint() {
        // ConPTY framing observed live around real reg.exe output.
        let painted = b"\x1b[6n\x1b[?9001hHKEY_LOCAL_MACHINE\\SYSTEM\x1b]0;C:\\WINDOWS\\system32\\cmd.exe\x07\r\n    SetupType    REG_DWORD    0x0\r\n\x1b[?9001l\x1b[1004l";
        assert_eq!(
            strip_ansi(painted),
            b"HKEY_LOCAL_MACHINE\\SYSTEM\r\n    SetupType    REG_DWORD    0x0\r\n".to_vec()
        );
        // Plain text (incl. CR LF) is untouched; lone ESC/BEL never survive.
        assert_eq!(strip_ansi(b"plain\r\ntext"), b"plain\r\ntext");
        assert_eq!(strip_ansi(b"a\x1bb"), b"ab");
        assert_eq!(strip_ansi(b"a\x07b"), b"ab");
        assert_eq!(strip_ansi(b""), b"");
    }

    #[test]
    fn stderr_is_loud_only_when_it_changes_the_reading() {
        let err = b"ERROR: the key does not exist\r\n";
        // No stdout: without the loud report a broken command would masquer
        // as an empty-but-successful answer (observed live).
        assert!(loud_stderr_message(b"", err).is_some());
        // Routine stderr beside real stdout: that is progress, not an error.
        assert!(loud_stderr_message(b"HKEY_LOCAL_MACHINE\\...\r\n", err).is_none());
        assert!(loud_stderr_message(b"ok", b"").is_none());
    }

    #[test]
    fn bcdedit_enum_lines_verify_both_switches() {
        // Shaped like real `bcdedit /enum {current}` output: variable
        // whitespace, mixed value widths.
        let enum_out = "Windows Boot Manager\n---------------------\nidentifier              {bootmgr}\n\nWindows Boot Loader\n-------------------\nidentifier              {current}\npath                    \\Windows\\System32\\winload.efi\ndescription             Windows 11\nrecoveryenabled         No\nbootstatuspolicy        IgnoreAllFailures\n";
        assert!(bcdedit_value_verified(enum_out, "recoveryenabled", "No"));
        assert!(bcdedit_value_verified(
            enum_out,
            "bootstatuspolicy",
            "IgnoreAllFailures"
        ));
        // Wrong policy (default DisplayAllFailures) must NOT verify.
        assert!(!bcdedit_value_verified(
            enum_out,
            "bootstatuspolicy",
            "DisplayAllFailures"
        ));
        // Keys must anchor at line start: "nrecoveryenabled" fails.
        assert!(!bcdedit_value_verified(
            "nrecoveryenabled  No\n",
            "recoveryenabled",
            "No"
        ));
        // Missing key entirely: verify-never-guess.
        assert!(!bcdedit_value_verified(
            "garbage\n",
            "recoveryenabled",
            "No"
        ));
    }
}
