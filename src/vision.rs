//! Guest-screen vision: capture, classify, motion, cadence. Everything
//! between pixels and verdicts lives here; `commands` orchestrates (run
//! flows, SSH waits, reporting composition) and never touches a frame
//! directly.
//!
//! Layers, innermost first: `screen_knn` (embeddings + cluster centers)
//! → `classify_screen` (nearest center to `ScreenState`) → `ScreenSampler` (frames, motion, phase
//! clocks on a `SamplingProfile`) → `capture_frame`/`take_screenshot`
//! (frame acquisition). `report_sample` stays with the orchestrator: it
//! composes vision with `ActivityMonitor`, and composition belongs there.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::timeouts;

pub(crate) const SCREENSHOT_PREFIX: &str = "screenshot-";

/// Per-process shot sequence: two takes inside one epoch second (sampler
/// save tick + an ad-hoc status/failure shot) must not share a filename.
/// Epoch keeps names sortable, the counter keeps them unique.
static SHOT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn screenshot_path(screenshots_dir: &Path) -> PathBuf {
    let n = SHOT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    screenshots_dir.join(format!(
        "{SCREENSHOT_PREFIX}{}-{n}.png",
        crate::provenance::now_epoch()
    ))
}

/// Coarse verdict on what the guest screen shows — the one-line answer
/// boot failures print so they are readable without VNC. Every frame is
/// judged alone by nearest-exemplar match: no history, no motion rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScreenState {
    /// No pixel visibly lit anywhere: literally nothing on screen. A
    /// spinner dot or a line of text on black is *content*, not this.
    Dark,
    /// A bare color field fills the frame with no visible text or UI
    /// (firmware/GPU output, test patterns). Deliberately NOT an error
    /// claim: the Windows Setup progress screen is a flat saturated blue
    /// too — color uniformity alone cannot tell the two apart (live
    /// 2026-10-04: the main installer screen was 99% #005A9E and the old
    /// 80% rule announced it as a "BSOD/recovery-like error"). Any
    /// text-bearing screen — setup, BSOD, recovery — maps to its own
    /// phase state, never this.
    FlatField,
    /// Error UI needing input: modal failure dialogs and bugcheck screens.
    /// One variant on purpose — the operational response is identical, and
    /// neither has enough captures to stand alone.
    ErrorScreen,
    /// The UEFI shell prompt is up (yellow shell text on black): no
    /// bootable device was found, so this boot is going nowhere. A shell
    /// is never routine progress — it is the boot having failed.
    ErrorShell,
    /// Windows Recovery screens (WinRE menus and failure text), identified
    /// by nearest-exemplar match — see [`crate::screen_knn`] for the
    /// class map. Recovery always needs input, so it is never content.
    ErrorRecovery,
    /// Windows Setup / installer progress screens (language select,
    /// progress bars, spinner). Routine forward motion.
    InstallerStep,
    /// First-boot OOBE transitionals ("Just a moment", logo splash,
    /// "Welcome"): post-install, pre-logon. Progress, never errors.
    OobeStep,
    /// Firmware boot art: TianoCore splash, boot logos. Transient screens
    /// between firmware handoff and the OS.
    FirmwareBoot,
    /// Lock screen / idle gradient. The guest is up but waiting; never an
    /// error in this flow (no desktop ever appears, so this bucket also
    /// covers miscellaneous idle gradients).
    Lockscreen,
}

impl std::fmt::Display for ScreenState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ScreenState::Dark => "screen is black (no visible output)",
            ScreenState::FlatField => "screen shows a solid color field (no UI visible)",
            ScreenState::ErrorScreen => "screen shows an ERROR (needs input)",
            ScreenState::ErrorShell => "screen shows the UEFI SHELL (no bootable device)",
            ScreenState::ErrorRecovery => "screen shows RECOVERY (Windows recovery, needs input)",
            ScreenState::InstallerStep => "screen shows Windows Setup / installer progress",
            ScreenState::OobeStep => "screen shows first-boot OOBE (post-install, pre-logon)",
            ScreenState::FirmwareBoot => {
                "screen shows firmware boot art (TianoCore splash / boot logo)"
            }
            ScreenState::Lockscreen => "screen shows lock screen / idle (guest up, waiting)",
        })
    }
}

/// Classify raw RGB (as parsed by [`parse_ppm_p6`]). Every frame goes
/// through the k-NN index ([`crate::screen_knn`]): embed, cosine against
/// the baked cluster centers, top-1. Single frame, no history, no pixel
/// gates. Returns the state plus the cosine gap (`best` minus best of
/// any other class): wording hedges below
/// [`crate::screen_knn::KNN_UNCERTAIN_GAP`], never gating.
pub(crate) fn classify_screen(rgb: &[u8], width: u32) -> (ScreenState, f32) {
    use crate::screen_knn::ScreenClass as C;
    // Degenerate empty frame: nothing captured = nothing to see.
    if rgb.is_empty() || width == 0 {
        return (ScreenState::Dark, 1.0);
    }
    let height = rgb.len() / 3 / width as usize;
    assert_eq!(
        rgb.len(),
        width as usize * height * 3,
        "classify_screen size mismatch"
    );
    let q = crate::screen_knn::KnnIndex::embed_frame(rgb, width, height as u32);
    let (class, gap, _) = crate::screen_knn::shared_index()
        .predict(&q, None)
        .unwrap_or((C::Dark, 1.0, String::new()));
    let state = match class {
        C::Dark => ScreenState::Dark,
        C::FlatField => ScreenState::FlatField,
        C::ErrorShell => ScreenState::ErrorShell,
        C::InstallerStep => ScreenState::InstallerStep,
        C::Lockscreen => ScreenState::Lockscreen,
        C::FirmwareBoot => ScreenState::FirmwareBoot,
        C::OobeStep => ScreenState::OobeStep,
        C::ErrorRecovery => ScreenState::ErrorRecovery,
        C::ErrorScreen => ScreenState::ErrorScreen,
    };
    (state, gap)
}

/// Log-line wording for a classified frame: the [`ScreenState`] message,
/// hedged when an error read is uncertain. Uncertainty never gates —
/// the activity conjunction alone decides WARNINGs.
pub(crate) fn describe(state: ScreenState, gap: f32) -> String {
    let base = state.to_string();
    let is_error = matches!(
        state,
        ScreenState::ErrorScreen | ScreenState::ErrorShell | ScreenState::ErrorRecovery
    );
    if is_error && gap < crate::screen_knn::KNN_UNCERTAIN_GAP {
        format!("{base} (uncertain — may need input)")
    } else {
        base
    }
}

/// Raw frame capture shared by the sampler and [`take_screenshot`]:
/// screendump to a temp PPM inside the screenshots dir, parsed to RGB,
/// PPM deleted. The PPM lives beside the PNGs (never /tmp —
/// world-writable-sticky, symlink squatting, cross-VM collisions) under a
/// `frame-` prefix that never collides with `screenshot-*.png`.
async fn capture_frame(
    mon: &crate::ipc::Monitor,
    screenshots_dir: &Path,
) -> Option<(u32, u32, Vec<u8>)> {
    if std::fs::create_dir_all(screenshots_dir).is_err() {
        return None;
    }
    // O_EXCL temp file, never a hand-built epoch name: the sampler task
    // and ad-hoc takes run concurrently in one process, and two
    // same-second captures sharing `frame-<pid>-<epoch>.ppm` made one
    // delete the other's screendump mid-read (observed class: transient
    // parse failures). Drop at the end deletes it on every path.
    let ppm_file = match tempfile::Builder::new()
        .prefix("frame-")
        .suffix(".ppm")
        .tempfile_in(screenshots_dir)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: frame temp file failed ({e:#})");
            return None;
        }
    };
    let ppm = ppm_file.path().to_path_buf();
    // Quote the path: HMP splits unquoted args on whitespace and the cache
    // path routinely contains spaces (observed live: screendump "succeeded"
    // but the file never appeared, failing the PNG step with ENOENT).
    let shot = mon
        .command(&format!("screendump \"{}\"", ppm.display()))
        .await;
    if let Err(e) = shot {
        eprintln!("warning: screendump failed: {e:#}");
        return None;
    }
    // Always clean up the raw PPM: a failed conversion must not leak
    // multi-MB dumps on every cadence tick (drop deletes the temp file).
    let result = std::fs::read(&ppm)
        .map_err(anyhow::Error::from)
        .and_then(|d| {
            let (w, h, rgb) = parse_ppm_p6(&d)?;
            Ok((w, h, rgb))
        });
    drop(ppm_file);
    match result {
        Ok(ok) => Some(ok),
        Err(e) => {
            eprintln!("warning: ppm parse failed ({e:#})");
            None
        }
    }
}

/// One screendump frame: `screendump` to PPM, converted in-crate to PNG
/// (README-displayable; tens to low hundreds of KB depending on screen
/// content vs 1.4 MB raw), PPM deleted. Frames land as
/// `<screenshots_dir>/screenshot-<epoch>.png`, each paired with the
/// [`ScreenState`] verdict from [`classify_screen`] so callers can
/// announce *what* the screen shows, not just where the file is.
/// Best-effort (logs, never fails): the monitor may be busy mid-command
/// and a missed frame is not worth failing an install over.
/// Returns the PNG path + state + margin on success so callers can
/// announce it.
pub(crate) async fn take_screenshot(
    mon: &crate::ipc::Monitor,
    screenshots_dir: &Path,
) -> Option<(PathBuf, ScreenState, f32)> {
    let (w, h, rgb) = capture_frame(mon, screenshots_dir).await?;
    // Single frame straight from the index: no history, no motion
    // rules. What the nearest center says is what gets reported.
    let (state, margin) = classify_screen(&rgb, w);
    let png_path = screenshot_path(screenshots_dir);
    match write_png(&png_path, w, h, &rgb) {
        Ok(()) => Some((png_path, state, margin)),
        Err(e) => {
            eprintln!("warning: png write failed ({e:#})");
            None
        }
    }
}

/// Parse binary P6 PPM → (width, height, RGB bytes). QEMU screendump
/// writes plain `P6\n<w> <h>\n255\n` + pixels; the parser also tolerates
/// comments and stray whitespace per the spec.
fn parse_ppm_p6(data: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut pos = 0;
    let mut token = || -> Result<String> {
        // Skip whitespace and `#` comments.
        loop {
            while pos < data.len() && data[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos < data.len() && data[pos] == b'#' {
                while pos < data.len() && data[pos] != b'\n' {
                    pos += 1;
                }
                continue;
            }
            break;
        }
        let start = pos;
        while pos < data.len() && !data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if start == pos {
            anyhow::bail!("truncated PPM header");
        }
        Ok(String::from_utf8_lossy(&data[start..pos]).into_owned())
    };
    if token()? != "P6" {
        anyhow::bail!("not a P6 PPM");
    }
    let w: u32 = token()?.parse().context("PPM width")?;
    let h: u32 = token()?.parse().context("PPM height")?;
    let max: u32 = token()?.parse().context("PPM maxval")?;
    if max != 255 {
        anyhow::bail!("unsupported PPM maxval {max} (want 255)");
    }
    // Exactly one whitespace byte separates header from pixels.
    if pos < data.len() && data[pos].is_ascii_whitespace() {
        pos += 1;
    }
    let expect = w as usize * h as usize * 3;
    if data.len() - pos < expect {
        anyhow::bail!(
            "truncated PPM pixels (have {}, want {expect})",
            data.len() - pos
        );
    }
    Ok((w, h, data[pos..pos + expect].to_vec()))
}

fn write_png(path: &Path, w: u32, h: u32, rgb: &[u8]) -> Result<()> {
    // Screenshots are flat UI: usually < 256 distinct colors, so exact
    // palette encoding (no dithering, no loss) beats RGB by ~7x
    // (measured: 42 KB RGB → 5.7 KB palette on a Setup screen).
    // Falls back to RGB when the frame is richer than 256 colors.
    use std::collections::HashMap;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut index_of: HashMap<[u8; 3], u8> = HashMap::new();
    let mut indexed = Vec::with_capacity(rgb.len() / 3);
    let mut fits = true;
    let (chunks, remainder) = rgb.as_chunks::<3>();
    if !remainder.is_empty() {
        fits = false;
    }
    for px in chunks {
        if !fits {
            break;
        }
        let key = [px[0], px[1], px[2]];
        let idx = match index_of.get(&key) {
            Some(&i) => i,
            None => {
                if palette.len() >= 256 {
                    fits = false;
                    break;
                }
                let i = palette.len() as u8;
                palette.push(key);
                index_of.insert(key, i);
                i
            }
        };
        indexed.push(idx);
    }
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Best);
    if fits {
        let flat: Vec<u8> = palette.iter().flatten().copied().collect();
        enc.set_color(png::ColorType::Indexed);
        enc.set_palette(flat);
        enc.write_header()?.write_image_data(&indexed)?;
    } else {
        enc.set_color(png::ColorType::Rgb);
        enc.write_header()?.write_image_data(rgb)?;
    }
    Ok(())
}

/// Cadence policy: answers "how often", owns no state, no I/O, no ticker.
/// The loops tick on [`SamplingProfile::tick`] and gate each layer on its
/// own due-check — one scheduler, three rates, zero alternate capture
/// paths.
pub struct SamplingProfile {
    /// Disk PNG cadence (zero = off).
    pub save_every: Duration,
    /// In-memory match cadence (zero = off).
    pub classify_every: Duration,
    /// Status-line narration cadence.
    pub narrate_every: Duration,
}

impl SamplingProfile {
    /// TCG classify default: pegged software emulation earns 3x spacing.
    pub const TCG_CLASSIFY_SECS: u64 = 30;

    /// Resolve after accelerator detection finalizes (post any KVM→TCG
    /// fallback): explicit flag wins, else accel default. Same predicate
    /// `cpu_model()` uses — no new detection, later call.
    pub fn resolve(save_secs: u64, classify: Option<u64>, accel: &str) -> Self {
        let classify_secs = classify.unwrap_or(if accel == "tcg" {
            Self::TCG_CLASSIFY_SECS
        } else {
            timeouts::DEFAULT_CLASSIFY_EVERY_SECS
        });
        Self {
            save_every: Duration::from_secs(save_secs),
            classify_every: Duration::from_secs(classify_secs),
            narrate_every: Duration::from_secs(timeouts::DEFAULT_BOOT_NARRATION_SECS),
        }
    }

    /// Single tick authority: min over active cadences (zero = that layer
    /// off). `None` when everything is off: nothing to schedule.
    pub fn tick(&self) -> Option<Duration> {
        [self.save_every, self.classify_every, self.narrate_every]
            .into_iter()
            .filter(|d| !d.is_zero())
            .min()
    }
}

/// What one sampler tick produced. The reporter announces; the sampler
/// never prints.
#[derive(Clone)]
pub(crate) enum SampleOutcome {
    /// PNG saved (doubles as the status line): path, state, prob margin.
    Saved(PathBuf, ScreenState, f32),
    /// In-memory classification only.
    Classified(ScreenState, f32),
    /// Nothing due, or the capture failed best-effort.
    Skipped,
}

/// What the guest looks like now. Owns the screendump, the frame buffer,
/// classification, and motion diff. Answers, never announces, never
/// policies: the loop ticks on the profile and reports via the
/// orchestrator's status line.
pub(crate) struct ScreenSampler {
    pub(crate) mon: std::sync::Arc<crate::ipc::Monitor>,
    dir: PathBuf,
    pub(crate) started: std::time::Instant,
    pub(crate) shot_n: u64,
    pub(crate) status_n: u64,
    last_save: Option<std::time::Instant>,
    last_classify: Option<std::time::Instant>,
    /// Previous tick's grid frame + geometry, for the motion diff.
    prev_grid: Option<(Vec<u8>, u32, u32)>,
    last_state: Option<(ScreenState, f32)>,
    pub(crate) last_outcome: SampleOutcome,
    state_since: std::time::Instant,
    static_since: std::time::Instant,
}

impl ScreenSampler {
    /// Tile-max diff per pixel (0–255 scale) below which the screen
    /// counts as static. Sits above VNC compression noise and cursor
    /// blink (verify against a real parked capture); an edge spinner
    /// tripping one tile clears it — a global mean would not.
    const MOTION_NOISE_FLOOR: f32 = 1.5;

    pub(crate) fn new(mon: std::sync::Arc<crate::ipc::Monitor>, dir: PathBuf) -> Self {
        let now = std::time::Instant::now();
        Self {
            mon,
            dir,
            started: now,
            shot_n: 0,
            status_n: 0,
            // `None` = due immediately: the first poll captures instead of
            // special-casing startup.
            last_save: None,
            last_classify: None,
            prev_grid: None,
            last_state: None,
            last_outcome: SampleOutcome::Skipped,
            state_since: now,
            static_since: now,
        }
    }

    /// One tick: at most one screendump, serving save or classify,
    /// whichever the profile says is due. No tick ever double-captures.
    pub(crate) async fn poll(&mut self, profile: &SamplingProfile) {
        let save_due = !profile.save_every.is_zero()
            && self
                .last_save
                .is_none_or(|t| t.elapsed() >= profile.save_every);
        if save_due {
            let Some((w, h, rgb)) = capture_frame(&self.mon, &self.dir).await else {
                self.last_outcome = SampleOutcome::Skipped;
                return;
            };
            let (state, margin) = classify_screen(&rgb, w);
            self.observe_frame(&rgb, w, h, state);
            let png_path = screenshot_path(&self.dir);
            match write_png(&png_path, w, h, &rgb) {
                Ok(()) => {
                    self.shot_n += 1;
                    self.status_n += 1;
                    self.last_save = Some(std::time::Instant::now());
                    self.last_classify = Some(std::time::Instant::now());
                    self.last_state = Some((state, margin));
                    self.last_outcome = SampleOutcome::Saved(png_path, state, margin);
                }
                Err(e) => {
                    eprintln!("warning: png write failed ({e:#})");
                    self.last_outcome = SampleOutcome::Skipped;
                }
            }
            return;
        }
        let classify_due = !profile.classify_every.is_zero()
            && self
                .last_classify
                .is_none_or(|t| t.elapsed() >= profile.classify_every);
        if classify_due && let Some((w, h, rgb)) = capture_frame(&self.mon, &self.dir).await {
            let (state, margin) = classify_screen(&rgb, w);
            self.observe_frame(&rgb, w, h, state);
            self.status_n += 1;
            self.last_classify = Some(std::time::Instant::now());
            self.last_state = Some((state, margin));
            self.last_outcome = SampleOutcome::Classified(state, margin);
            return;
        }
        self.last_outcome = SampleOutcome::Skipped;
    }

    /// Fold one captured frame into motion + phase tracking. Motion runs
    /// on the FULL frame (not a center crop): progress
    /// spinners live on edges (observed: installer spinner at ~4% from
    /// the left), and a crop reused for motion is blind to them. Tile-max
    /// keeps the noise immunity a full-frame mean would lose: localized
    /// animation trips one tile while flat background dilutes nothing.
    /// New geometry (driver handoff) or first frame: no baseline, so no
    /// claim — the buffer adopts the new geometry and the static timer
    /// holds.
    fn observe_frame(&mut self, rgb: &[u8], w: u32, h: u32, state: ScreenState) {
        let grid = crate::screen_knn::downsample_to_grid_rgb(rgb, w as usize, h as usize);
        let moved = match &self.prev_grid {
            Some((prev, pw, ph)) if *pw == w && *ph == h => {
                tile_max_diff(prev, &grid, crate::screen_knn::GRID) >= Self::MOTION_NOISE_FLOOR
            }
            _ => false,
        };
        // Hold the static timer across geometry changes and failed
        // captures (no frame = no evidence either way).
        if moved {
            self.static_since = std::time::Instant::now();
        }
        self.prev_grid = Some((grid.to_vec(), w, h));
        if self.last_state.map(|(s, _)| s) != Some(state) {
            self.state_since = std::time::Instant::now();
        }
    }

    /// Seconds in the current screen state.
    pub(crate) fn phase_secs(&self) -> u64 {
        self.state_since.elapsed().as_secs()
    }

    /// Latest classified state, if any frame has been captured yet.
    pub(crate) fn state(&self) -> Option<ScreenState> {
        self.last_state.map(|(s, _)| s)
    }

    /// Seconds since pixels last moved beyond the noise floor.
    pub(crate) fn static_secs(&self) -> u64 {
        self.static_since.elapsed().as_secs()
    }
}

/// Max over tiles of mean abs per-pixel diff (0–255 scale): localized
/// motion (an edge spinner moving dozens of pixels) trips its tile even
/// when a global mean would dilute it into the background. `dim` is the
/// square grid edge; RGB-interleaved, 8×8 tiles. Degenerate input reads
/// as maximum motion, never a panic.
fn tile_max_diff(a: &[u8], b: &[u8], dim: usize) -> f32 {
    const TILES: usize = 8;
    if a.len() != b.len() || a.is_empty() || a.len() != dim * dim * 3 || !dim.is_multiple_of(TILES)
    {
        return 255.0;
    }
    let tile = dim / TILES;
    let mut max = 0.0f32;
    for ty in 0..TILES {
        for tx in 0..TILES {
            let mut sum = 0u64;
            for y in 0..tile {
                for x in 0..tile {
                    let base = ((ty * tile + y) * dim + (tx * tile + x)) * 3;
                    for c in 0..3 {
                        sum += a[base + c].abs_diff(b[base + c]) as u64;
                    }
                }
            }
            let mean = sum as f32 / (tile * tile * 3) as f32;
            if mean > max {
                max = mean;
            }
        }
    }
    max
}

/// Compact durations, always HH:MM:SS: readable at 3 hours, no math.
pub(crate) fn fmt_secs(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Compact byte counts: 512B, 12MB, 1.2GB (one decimal under 10 units).
pub(crate) fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < 4 {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 && u > 0 {
        format!("{v:.1}{}", UNITS[u])
    } else {
        format!("{v:.0}{}", UNITS[u])
    }
}

pub(crate) fn is_error_state(s: ScreenState) -> bool {
    matches!(
        s,
        ScreenState::ErrorScreen | ScreenState::ErrorShell | ScreenState::ErrorRecovery
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: usize, h: usize, rgb: [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 3);
        for _ in 0..w * h {
            v.extend_from_slice(&rgb);
        }
        v
    }

    #[test]
    fn screen_state_black_means_nothing_visible() {
        assert_eq!(
            classify_screen(&flat(64, 64, [0, 0, 0]), 64).0,
            ScreenState::Dark
        );
        // Degenerate empty frame: nothing captured = nothing to see.
        assert_eq!(classify_screen(&[], 0).0, ScreenState::Dark);
    }

    #[test]
    fn uncertain_errors_hedge_confident_ones_do_not() {
        // Gap scale, not prob scale: the hedge threshold is
        // KNN_UNCERTAIN_GAP (0.01), so 0.005 hedges and 0.5 states.
        let hedged = describe(ScreenState::ErrorScreen, 0.005);
        assert!(hedged.contains("uncertain"), "{hedged}");
        let firm = describe(ScreenState::ErrorScreen, 0.5);
        assert!(!firm.contains("uncertain"), "{firm}");
        // Progress states never hedge: only error reads carry doubt.
        let progress = describe(ScreenState::InstallerStep, 0.0);
        assert!(!progress.contains("uncertain"), "{progress}");
    }

    #[test]
    fn durations_read_hh_mm_ss() {
        assert_eq!(fmt_secs(0), "00:00:00");
        assert_eq!(fmt_secs(95), "00:01:35");
        assert_eq!(fmt_secs(252), "00:04:12");
        assert_eq!(fmt_secs(3600), "01:00:00");
        assert_eq!(fmt_secs(3 * 3600 + 41 * 60 + 7), "03:41:07");
    }

    #[test]
    fn byte_counts_stay_compact() {
        assert_eq!(fmt_bytes(0), "0B");
        assert_eq!(fmt_bytes(512), "512B");
        assert_eq!(fmt_bytes(1536), "1.5KB");
        assert_eq!(fmt_bytes(12 * 1024 * 1024), "12MB");
    }

    #[test]
    fn tile_max_catches_edge_motion_global_mean_misses() {
        // The installer-spinner case: a small changed block in the
        // corner tile. Global mean stays under the floor while the hot
        // tile clears it — a crop/full-mean-only metric reads static.
        let dim = 32usize;
        let still = vec![10u8; dim * dim * 3];
        let mut moved = still.clone();
        for y in 0..2 {
            for x in 0..2 {
                for c in 0..3 {
                    moved[(y * dim + x) * 3 + c] = 210;
                }
            }
        }
        let global: f32 = still
            .iter()
            .zip(moved.iter())
            .map(|(&x, &y)| x.abs_diff(y) as u64)
            .sum::<u64>() as f32
            / still.len() as f32;
        assert!(global < ScreenSampler::MOTION_NOISE_FLOOR);
        assert!(tile_max_diff(&still, &moved, dim) >= ScreenSampler::MOTION_NOISE_FLOOR);
        assert_eq!(tile_max_diff(&still, &still, dim), 0.0);
        assert_eq!(tile_max_diff(&[], &[], dim), 255.0);
        assert_eq!(tile_max_diff(&still, &moved, 30), 255.0);
    }

    #[test]
    fn ppm_p6_roundtrip() {
        // 2x1 red/green pixels with a comment in the header.
        let mut raw = b"P6\n# qemu screendump\n2 1\n255\n".to_vec();
        raw.extend_from_slice(&[255, 0, 0, 0, 255, 0]);
        let (w, h, rgb) = parse_ppm_p6(&raw).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgb, vec![255, 0, 0, 0, 255, 0]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.png");
        write_png(&p, w, h, &rgb).unwrap();
        let back = std::fs::read(&p).unwrap();
        assert_eq!(&back[1..4], b"PNG", "not a PNG");
        // 2 colors → indexed palette (IHDR color type 3), exact pixels.
        assert_eq!(
            back[25], 3,
            "expected palette PNG, got color type {}",
            back[25]
        );
        assert!(
            back.len() < 100,
            "2px palette PNG should be tiny, got {}",
            back.len()
        );
    }

    #[test]
    fn ppm_rejects_bad_magic_and_truncation() {
        assert!(parse_ppm_p6(b"P5\n2 1\n255\n\x00\x00").is_err());
        assert!(parse_ppm_p6(b"P6\n2 1\n255\n\xff").is_err());
    }
}
