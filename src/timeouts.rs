//! Single source of truth for every time budget in the crate. Bumping a
//! timeout means editing here — not hunting `Duration::from_secs(…)`
//! literals across modules (they were spread over seven files, and the
//! user-facing messages duplicated the numbers a second time: "up to 10
//! min", "after 50 s", "waiting 60 s …").
//!
//! Shape rule: one const per concept, typed for its call site — `Duration`
//! where the API takes one, `u64` seconds where it counts seconds (clap
//! defaults, retry loops). Messages *format* the const instead of restating
//! the number, so prose can never drift from the value.
//!
//! Every value is the one the code used before this module existed: this is
//! a re-homing, not a retune. Tiny fixture durations inside unit tests stay
//! with those tests (they bound error paths, not real waits); the canary and
//! the example keep their own explicitly generous budgets below rather than
//! silently inheriting production ones.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Outer SSH deadlines — the two a user actually waits on
// ---------------------------------------------------------------------------

/// `run`: boot → first SSH banner. The printed "up to N min" line is
/// derived from this, so the message can't lie about the real budget.
/// (`install` uses [`INSTALL_SSH_DEADLINE_SECS`] instead.)
pub const BOOT_SSH_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// Default for `--install-timeout-secs` (`install`, `run --win-iso`):
/// how long a *still-progressing* unattended install may take to report
/// ready. Generous on purpose — a slow host applies the image for hours,
/// and a parked guest is caught separately by [`IDLE_BAIL_SECS`].
pub const INSTALL_SSH_DEADLINE_SECS: u64 = 6 * 60 * 60;

/// Default for `--screenshots-every-secs`: the install time-lapse and
/// `run`'s boot time-lapse cadence (also what `run --win-iso` uses for
/// its install-chain time-lapse, which has no flag of its own).
pub const SCREENSHOTS_EVERY_SECS: u64 = 60;

/// Internal snapshot tag for the landed state (`qemu-img snapshot -c`).
pub const GOLDEN_TAG: &str = "golden";

/// Bound for a live migration-to-file (`snapshot --live`): 4 GB of guest
/// RAM over loopback lands in seconds; this only ever fires on a wedged
/// migration, never on a slow-but-moving one.
pub const MIGRATION_BUDGET_SECS: u64 = 600;
/// Poll cadence while a live migration runs.
pub const MIGRATION_POLL_SECS: u64 = 2;

/// Bound for a clean guest shutdown ahead of a golden snapshot: the guest
/// gets this long to halt after `Stop-Computer` before the snapshot path
/// gives up and reports instead of capturing a dirty disk.
pub const GUEST_SHUTDOWN_SECS: u64 = 120;

// ---------------------------------------------------------------------------
// Per-poll SSH mechanics (the inner loops under those deadlines)
// ---------------------------------------------------------------------------

/// Budget for reading the `SSH-2.0-…` banner once TCP connects. Bounds the
/// banner wait so a slirp half-open forward (accepts, then stalls) degrades
/// to another poll instead of a hang.
pub const SSH_BANNER_READ_BUDGET: Duration = Duration::from_secs(10);

/// Budget for the TCP connect of a single banner probe (`banner_once`).
/// Separate from [`SSH_BANNER_READ_BUDGET`]: connect and read stall for
/// different reasons, and shortening one shouldn't silently shorten the other.
pub const SSH_BANNER_CONNECT_BUDGET: Duration = Duration::from_secs(10);

/// Sleep between SSH polls. Deliberately short: at 15 s users stared at a
/// ready desktop while the CLI still said "booting" (observed live), and a
/// poll is one TCP SYN — latency is what the user feels.
pub const SSH_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Hard budget for one guest-readiness probe (`probe_setup_done`). A
/// stalled handshake mid-reboot must surface as "not yet", never hang the
/// wait loop.
pub const READINESS_PROBE_BUDGET: Duration = Duration::from_secs(60);

/// Sleep between readiness probes (each probe is a full login + PowerShell
/// cold start — no need to hammer).
pub const READINESS_PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// Bound on one exec-channel drain. PowerShell cold-start on a fresh boot
/// can stall tens of seconds; beyond this the channel is hung, and the
/// error must be loud instead of burning the caller's whole budget silently.
pub const SSH_EXEC_DRAIN: Duration = Duration::from_secs(50);

/// Budget for each half of a guest password rotation: the `net user` exec
/// and the verifying re-login. A stalled handshake must surface as an error,
/// never hang the caller.
pub const SSH_PASSWORD_ROTATION_BUDGET: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// QEMU lifecycle
// ---------------------------------------------------------------------------

/// Grace after spawn before checking whether QEMU rejected its argv — a
/// healthy boot is still starting here, so a live child means success and
/// only an *exited* child fails fast (instead of polling SSH for minutes
/// against a dead process).
pub const SPAWN_GUARD: Duration = Duration::from_secs(3);

/// Budget for the first successful monitor (HMP) handshake after spawn.
/// Never fails fast on a live-but-slow QEMU: an early bail left slow hvf
/// starters to `kill_on_drop`.
pub const MONITOR_CONNECT_SECS: u64 = 60;

/// Sleep between monitor connect retries — and the amount `connect_retry`
/// adds to its own accounting, so the reported elapsed time stays honest.
pub const MONITOR_RETRY_STEP_SECS: u64 = 2;

/// How long to wait for the monitor socket *file* to appear after spawn
/// (bind = QEMU starting).
pub const MONITOR_SOCKET_WAIT_SECS: u64 = 30;

/// Poll interval while waiting for the monitor socket file.
pub const MONITOR_SOCKET_POLL_SECS: u64 = 1;

/// How many times `stop` re-probes QMP after `quit` before declaring the
/// VM wedged, and the sleep between probes (20 × 500 ms ≈ 10 s total).
pub const QMP_QUIT_POLL_COUNT: u64 = 20;
pub const QMP_QUIT_POLL_INTERVAL: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Watchdogs and narration
// ---------------------------------------------------------------------------

/// Stall watchdog: warn when block I/O has been completely frozen this long
/// with no SSH banner (a working boot never flatlines — local NVMe only,
/// no network boot). Warn-once, then [`IDLE_BAIL_SECS`] bails with evidence.
pub const IDLE_WARN_SECS: u64 = 60;

/// Stall watchdog bail: frozen disk I/O + no banner this long = parked at
/// an input screen (recovery, EULA). Fails fast with evidence instead of
/// polling SSH against a guest that has stopped moving — this is what lets
/// [`INSTALL_SSH_DEADLINE_SECS`] stay generous.
pub const IDLE_BAIL_SECS: u64 = 120;

/// Cadence of the "... still waiting for SSH (Ns elapsed)" boot narration.
pub const BOOT_STATUS_TICK: Duration = Duration::from_secs(30);

/// Cadence of the "still installing: … — Ns elapsed, N reboot(s) so far"
/// heartbeat during the readiness wait. Probes run every
/// [`READINESS_PROBE_INTERVAL`]; printing on every one buried the log, so
/// progress is narrated on change plus this slower pulse (proof of life,
/// not event spam).
pub const PROGRESS_HEARTBEAT: Duration = Duration::from_secs(60);

/// Grace the runtime gives leaked blocking threads at process exit before
/// abandoning them (a pending `stdin.read()` in the shell loop otherwise
/// parks a pool thread forever and the CLI never exits).
pub const RUNTIME_EXIT_GRACE: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Tests and examples — intentionally their own budgets
// ---------------------------------------------------------------------------

/// Weekly canary: latest Eval ISO end-to-end, bounded by one deadline used
/// both as the outer `timeout` and the inner `wait_for_ssh` budget.
pub const CANARY_SSH_DEADLINE: Duration = Duration::from_secs(25 * 60);

/// `examples/boot.rs`: first-setup wait on a cold, unproven image.
pub const EXAMPLE_SSH_DEADLINE: Duration = Duration::from_secs(900);

/// `examples/boot.rs`: ACPI-less graceful shutdown budget.
pub const EXAMPLE_SHUTDOWN_GRACE: Duration = Duration::from_secs(90);
