//! Dev-time trainer for the micro-CNN in `src/screen_cnn.rs`.
//!
//! Pure Rust, no ML libraries: gradient descent with cross-entropy, fixed
//! seeds throughout (same corpus, same weights, every run). Output:
//! `artifacts/weights.bin` (little-endian f32, layout documented on
//! `screen_cnn::N_WEIGHTS`). The run always exits 0 and always writes;
//! a human decides merge from the printed report.
//!
//! Corpus layout (labels come from directory names, never a filename
//! list): `tests/fixtures/<windows-version>/<class-dir>/*.png`, where each
//! class directory maps to a `ScreenClass` variant (see `CLASS_DIRS`
//! below). The version directory carries no label: the classifier never
//! sees it, it only keeps captures attributable over time.
//!
//! Sampling is fully dynamic: no fixed variants repeat across epochs.
//! Every epoch draws `VIEWS_PER_CLASS` fresh stochastic views per class
//! from its own base captures (flip, brightness, tint, rescale —
//! photometric and geometric only, never new shapes). Bases are never
//! trained on directly.
//!
//! Stopping is patience on the holdout, not a fixed-accuracy gate: the
//! corpus holds a known-unlearnable pair, so 100% never fires. Best
//! holdout hits win (base-capture margin breaks ties), the best
//! checkpoint ships, and the run stops after `PATIENCE_EPOCHS` stale
//! epochs. Margin is reported, never gating.
//!
//! Regenerate: `cargo run --release --example train_screen_cnn` (release
//! matters: thousands of sampled views per epoch over hundreds of
//! epochs).

use windows_headless::screen_cnn::{
    BY_INDEX, DENSE_INPUTS, DENSE_TOTAL, GRID, INPUT_CHANNELS, KERNEL, N_CLASSES, N_FILTERS,
    N_WEIGHTS, POOL, POOL_FEATURES, POOLED, STREAMS, ScreenClass, crop_region,
    downsample_region_to_grid_rgb, downsample_to_grid_rgb, load_rgb_8bit,
};

/// Cap on epochs: patience stops the run first in practice; this bounds
/// the pathological case (metric oscillating forever) to a finite bill.
const EPOCHS: usize = 10_000;
/// Full-batch gradient descent step size. Scales with 1/DENSE_TOTAL:
/// 8192 pooled features move together per step across both streams
/// (was 64 features with 0.5 at GRID 32 / 4 filters / one stream), so
/// the rate drops to hold the output swing equal.
const LEARNING_RATE: f32 = 0.004;
/// Epochs without holdout improvement before stopping. If the corpus holds an
/// unlearnable pair, a fixed-accuracy gate never fires; patience
/// banks the best model instead of grinding plateaus. Generous on purpose:
/// whole classes (dark, flat-field) pop after epoch 100, and cutting the
/// run at dozens of epochs executes it before the easy wins land.
const PATIENCE_EPOCHS: usize = 50;
/// Logit gap that counts as comfortably won. Informational only (tiebreak
/// input and report line): nothing gates on it.
const REQUIRED_MARGIN: f32 = 2.0;
/// Fresh stochastic views drawn per class per epoch (288-frame batch).
const VIEWS_PER_CLASS: usize = 32;
const BATCH: usize = VIEWS_PER_CLASS * N_CLASSES;
/// Epoch-seed stream for the per-epoch sampling RNG (xor-folded with the
/// epoch number): same epoch, same views, every run.
const AUG_SEED: u64 = 0x0A09;
/// Fixed seed for He init.
const INIT_SEED: u64 = 0xC0FFEE;
/// Pixels per CNN input channel.
const GRID_PIXELS: usize = GRID * GRID;
/// Weights per convolution filter (all input channels, incl. Y-coord).
const FILTER_WEIGHTS: usize = INPUT_CHANNELS * KERNEL * KERNEL;

// ---------------------------------------------------------------------------
// tiny deterministic RNG (fixed seed: same corpus, same weights, every run)
// ---------------------------------------------------------------------------

/// Knuth MMIX linear congruential generator constants.
const LCG_MULTIPLIER: u64 = 6364136223846793005;
const LCG_INCREMENT: u64 = 1442695040888963407;
const LCG_OUTPUT_BITS: u32 = 33;

struct Lcg(u64);

impl Lcg {
    fn next_unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT);
        ((self.0 >> (64 - LCG_OUTPUT_BITS)) as f64) / (1u64 << LCG_OUTPUT_BITS) as f64
    }
}

// ---------------------------------------------------------------------------
// corpus: (GRID x GRID frame, label). Labels are ScreenClass discriminants.
// ---------------------------------------------------------------------------

type Frame = [u8; 3 * GRID * GRID];
/// Both streams of one capture: full view plus center-half crop.
type Streams = [Frame; STREAMS];
/// One labeled base capture: both streams, class index, source name.
/// Bases are the sampling pool, never trained on directly: every training
/// frame is a fresh stochastic view drawn per epoch.
type Base = (Streams, usize, String);
/// Corpus: per-file base captures plus the same frames grouped by class
/// for balanced per-epoch sampling.
type Corpus = (Vec<Base>, [Vec<Streams>; N_CLASSES]);

/// One stochastic draw's parameters, shared across both streams so the
/// full view and its crop stay coherent (a flipped full frame with an
/// unflipped crop would be a frame that cannot occur).
struct ViewParams {
    flip: bool,
    bright: f32,
    shifts: [f32; 3],
    fx: f32,
    fy: f32,
}

fn draw_view_params(rng: &mut Lcg) -> ViewParams {
    ViewParams {
        flip: rng.next_unit() < 0.5,
        bright: 0.85 + rng.next_unit() as f32 * 0.30,
        shifts: [
            (rng.next_unit() * 16.0 - 8.0) as f32,
            (rng.next_unit() * 16.0 - 8.0) as f32,
            (rng.next_unit() * 16.0 - 8.0) as f32,
        ],
        fx: 0.85 + rng.next_unit() as f32 * 0.30,
        fy: 0.85 + rng.next_unit() as f32 * 0.30,
    }
}

/// Fresh stochastic view of a real capture, drawn at GRID resolution:
/// random horizontal flip, continuous brightness and tint, and a small
/// edge-clamped rescale (DPI/aspect drift). Same alteration family as the
/// old fixed set, but redrawn every epoch so no epoch ever repeats. No
/// new shapes ever: every view derives from a real capture.
fn apply_view(params: &ViewParams, base: &Frame) -> Frame {
    let mut out = [0u8; 3 * GRID * GRID];
    for y in 0..GRID {
        // Edge-clamped nearest neighbor: zoom-outs reuse edge pixels
        // rather than inventing borders. Flip reads the mirrored column.
        let sy = ((y as f64 / f64::from(params.fy)) as usize).min(GRID - 1);
        for x in 0..GRID {
            let sx = ((x as f64 / f64::from(params.fx)) as usize).min(GRID - 1);
            let sx = if params.flip { GRID - 1 - sx } else { sx };
            let s = (sy * GRID + sx) * 3;
            let d = (y * GRID + x) * 3;
            // Uniform photometrics: no background-color assumptions. Setup,
            // WinPE, and OOBE all render pure-black canvases identical to
            // firmware's, so a black floor would bake in a class signal
            // that does not exist. Tint shifts every pixel whole; spatial
            // structure (CoordConv anchors, layout, glyph density) carries
            // the firmware-vs-setup separation instead.
            for c in 0..3 {
                out[d + c] = (f32::from(base[s + c]) * params.bright + params.shifts[c])
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// Directory name under each Windows-version directory in
/// `tests/fixtures/` to training label. This table — not a filename list —
/// is the corpus definition; drop new captures in and re-run. The version
/// directory itself carries no label: the classifier never sees it, it only
/// keeps captures attributable over time.
///
/// Holdout convention: files ending in `.holdout.png` train nothing and are
/// evaluated only (unseen layouts, synthetic-to-real transfer). The class
/// directory still supplies the expected label, so no list is maintained.
const CLASS_DIRS: [(&str, ScreenClass); 10] = [
    ("dark", ScreenClass::Dark),
    ("flat-field", ScreenClass::FlatField),
    ("error-shell", ScreenClass::ErrorShell),
    ("installer-step", ScreenClass::InstallerStep),
    ("error-recovery", ScreenClass::ErrorRecovery),
    ("error-modal", ScreenClass::ErrorScreen),
    ("lockscreen", ScreenClass::Lockscreen),
    ("firmware-boot", ScreenClass::FirmwareBoot),
    ("oobe-step", ScreenClass::OobeStep),
    ("error-bluescreen", ScreenClass::ErrorScreen),
];

fn corpus() -> Corpus {
    // One base frame per capture, plus the same frames grouped by class
    // for balanced sampling. Bases are never trained on directly: every
    // training frame is a fresh stochastic view drawn per epoch.
    let mut files: Vec<Base> = Vec::new();
    let mut bases: [Vec<Streams>; N_CLASSES] = Default::default();
    let root = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    // Two levels: Windows version (build + language + edition, unattributed,
    // keeps captures identifiable over time) then class directory (the label).
    let mut versions: Vec<_> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read {root}: {e}"))
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    versions.sort();
    for version in versions {
        for (dir, label) in CLASS_DIRS {
            let dir_path = format!("{root}/{version}/{dir}");
            if !std::path::Path::new(&dir_path).is_dir() {
                continue;
            }
            let mut names: Vec<_> = std::fs::read_dir(&dir_path)
                .unwrap_or_else(|e| panic!("read {dir_path}: {e}"))
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".png"))
                .collect();
            names.sort();
            for name in names {
                let (rgb, w, h) = load_rgb_8bit(&format!("{dir_path}/{name}"));
                let (w, h) = (w as usize, h as usize);
                let full = downsample_to_grid_rgb(&rgb, w, h);
                let (x0, y0, x1, y1) = crop_region(w, h);
                let crop = downsample_region_to_grid_rgb(&rgb, w, h, x0, y0, x1, y1);
                let streams = [full, crop];
                bases[label as usize].push(streams);
                files.push((streams, label as usize, format!("{version}/{dir}/{name}")));
            }
        }
    }
    // No synthetic frames: every training view derives from a real
    // capture (flip, brightness, rescale, tint). Shapes no real frame
    // produces are not taught.
    (files, bases)
}

// ---------------------------------------------------------------------------
// forward pass with caches (mirrors screen_cnn::predict_two_stream exactly)
// ---------------------------------------------------------------------------

struct Caches {
    chw: [f32; INPUT_CHANNELS * GRID * GRID],
    conv: [f32; N_FILTERS * GRID * GRID],
}

const KERNEL_PAD: isize = KERNEL as isize / 2;

/// One stream's conv+pool into `pool_out`: shared filters, reused caches.
/// Mirrors the per-stream block of `predict_two_stream`.
/// `y_min`/`y_max` is the stream's vertical span in global frame
/// coordinates: full view (-1.0, 1.0), center-half crop (-0.5, 0.5).
/// Shared filters demand globally consistent coordinates — synthesizing
/// [-1, 1] per stream would tell the same kernel that the crop's top row
/// is both the monitor top (stream 0) and the frame middle (stream 1).
fn forward_stream(
    rgb: &Frame,
    conv_w: &[f32],
    conv_b: &[f32],
    out: &mut Caches,
    pool_out: &mut [f32],
    y_min: f32,
    y_max: f32,
) {
    for c in 0..3 {
        for i in 0..GRID_PIXELS {
            out.chw[c * GRID_PIXELS + i] = rgb[i * 3 + c] as f32 / 255.0;
        }
    }
    // Static Y-coordinate plane (CoordConv anchor, mirrors the lib).
    for r in 0..GRID {
        let y_coord = y_min + (r as f32 / (GRID - 1) as f32) * (y_max - y_min);
        for c in 0..GRID {
            out.chw[3 * GRID_PIXELS + r * GRID + c] = y_coord;
        }
    }
    for oc in 0..N_FILTERS {
        let b = conv_b[oc];
        for r in 0..GRID {
            for c in 0..GRID {
                let mut sum = b;
                for ic in 0..INPUT_CHANNELS {
                    for kr in 0..KERNEL {
                        let nr = r as isize + kr as isize - KERNEL_PAD;
                        if nr < 0 || nr >= GRID as isize {
                            continue;
                        }
                        for kc in 0..KERNEL {
                            let nc = c as isize + kc as isize - KERNEL_PAD;
                            if nc < 0 || nc >= GRID as isize {
                                continue;
                            }
                            sum += out.chw[ic * GRID_PIXELS + nr as usize * GRID + nc as usize]
                                * conv_w
                                    [oc * FILTER_WEIGHTS + ic * KERNEL * KERNEL + kr * KERNEL + kc];
                        }
                    }
                }
                out.conv[oc * GRID_PIXELS + r * GRID + c] = sum.max(0.0);
            }
        }
    }
    for ch in 0..N_FILTERS {
        for r in 0..POOLED {
            for c in 0..POOLED {
                let mut m = f32::NEG_INFINITY;
                for kr in 0..POOL {
                    for kc in 0..POOL {
                        let v =
                            out.conv[ch * GRID_PIXELS + (r * POOL + kr) * GRID + (c * POOL + kc)];
                        if v > m {
                            m = v;
                        }
                    }
                }
                pool_out[ch * POOL_FEATURES + r * POOLED + c] = m;
            }
        }
    }
}

/// Global vertical span of one stream: full view covers the frame
/// (-1.0, 1.0), the center-half crop covers its middle quarter by area
/// (-0.5, 0.5). Single source so `forward` and the backward
/// rematerialization can never disagree on a stream's coordinates.
fn stream_y_range(s: usize) -> (f32, f32) {
    if s == 0 { (-1.0, 1.0) } else { (-0.5, 0.5) }
}

fn forward(
    streams: &Streams,
    conv_w: &[f32],
    conv_b: &[f32],
    out: &mut Caches,
) -> [f32; DENSE_TOTAL] {
    let mut pool = [0.0f32; DENSE_TOTAL];
    for (s, rgb) in streams.iter().enumerate() {
        let lo = s * DENSE_INPUTS;
        let (y_min, y_max) = stream_y_range(s);
        forward_stream(
            rgb,
            conv_w,
            conv_b,
            out,
            &mut pool[lo..lo + DENSE_INPUTS],
            y_min,
            y_max,
        );
    }
    pool
}

/// Accumulated per-epoch gradient plus the batch stats the log line
/// reports. One struct so the epoch loop reads as accumulate-then-step.
struct GradAccum {
    g_conv_w: [f32; N_FILTERS * FILTER_WEIGHTS],
    g_conv_b: [f32; N_FILTERS],
    g_dense_w: [f32; N_CLASSES * DENSE_TOTAL],
    g_dense_b: [f32; N_CLASSES],
    loss: f32,
    correct: usize,
    class_correct: [usize; N_CLASSES],
    class_seen: [usize; N_CLASSES],
    min_margin: f32,
}

impl GradAccum {
    fn zero() -> Self {
        Self {
            g_conv_w: [0.0; N_FILTERS * FILTER_WEIGHTS],
            g_conv_b: [0.0; N_FILTERS],
            g_dense_w: [0.0; N_CLASSES * DENSE_TOTAL],
            g_dense_b: [0.0; N_CLASSES],
            loss: 0.0,
            correct: 0,
            class_correct: [0; N_CLASSES],
            class_seen: [0; N_CLASSES],
            min_margin: f32::INFINITY,
        }
    }
}

/// Cross-entropy of one frame against a label from precomputed softmax
/// parts. Shared by `frame_stats` and `train_frame` so eval and training
/// can never disagree on the loss.
fn logit_loss(logits: &[f32; N_CLASSES], label: usize, max: f32, denom: f32) -> f32 {
    -(((logits[label] - max).exp() / denom).max(1e-12).ln())
}

/// One batch frame's forward statistics: predicted class, margin over the
/// runner-up, and true-label cross-entropy. Shared by the batch loop (which
/// also backprops) and `evaluate` above (which only reports).
fn frame_stats(
    streams: &Streams,
    label: usize,
    conv_w: &[f32],
    conv_b: &[f32],
    dense_w: &[f32],
    dense_b: &[f32],
    caches: &mut Caches,
) -> (usize, f32, f32) {
    let pool = forward(streams, conv_w, conv_b, caches);
    let mut logits = [0.0f32; N_CLASSES];
    for o in 0..N_CLASSES {
        let mut s = dense_b[o];
        for i in 0..DENSE_TOTAL {
            s += pool[i] * dense_w[o * DENSE_TOTAL + i];
        }
        logits[o] = s;
    }
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let mut denom = 0.0f32;
    let mut probs = [0.0f32; N_CLASSES];
    for o in 0..N_CLASSES {
        probs[o] = (logits[o] - max).exp();
        denom += probs[o];
    }
    for p in probs.iter_mut() {
        *p /= denom;
    }
    let pred = probs
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .unwrap()
        .0;
    let mut second = f32::NEG_INFINITY;
    for (o, &v) in logits.iter().enumerate() {
        if o != pred && v > second {
            second = v;
        }
    }
    let margin = logits[pred] - second;
    (pred, margin, logit_loss(&logits, label, max, denom))
}

/// Accumulate one batch frame's loss statistics and gradient. Backprop:
/// dL/dlogit = (p - y) / N. Unweighted: every class draws the same view
/// count per epoch, so the balance lives in the sampling and the stats
/// line reports exactly what trains.
fn train_frame(
    streams: &Streams,
    label: usize,
    net: TrainNet<'_>,
    caches: &mut Caches,
    acc: &mut GradAccum,
    n: f32,
) {
    let pool = forward(streams, net.conv_w, net.conv_b, caches);
    // Recompute logits (forward returns pooled features): matches
    // frame_stats without returning the pooled vector.
    let mut logits = [0.0f32; N_CLASSES];
    for (o, logit) in logits.iter_mut().enumerate() {
        let mut s = net.dense_b[o];
        for (i, &p) in pool.iter().enumerate() {
            s += p * net.dense_w[o * DENSE_TOTAL + i];
        }
        *logit = s;
    }
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let denom: f32 = logits.iter().map(|v| (v - max).exp()).sum();
    let mut probs = [0.0f32; N_CLASSES];
    for (p, &v) in probs.iter_mut().zip(logits.iter()) {
        *p = (v - max).exp() / denom;
    }
    acc.loss += logit_loss(&logits, label, max, denom);
    acc.class_seen[label] += 1;
    let mut second = f32::NEG_INFINITY;
    let mut pred = 0;
    for (o, &p) in probs.iter().enumerate() {
        if p > probs[pred] {
            pred = o;
        }
    }
    for (o, &v) in logits.iter().enumerate() {
        if o != pred && v > second {
            second = v;
        }
    }
    if pred == label {
        acc.correct += 1;
        acc.class_correct[label] += 1;
        // Misses count in acc; margin measures the correctly-called
        // frames only, so the number stays informative while misses
        // remain instead of pinning at -1 forever.
        acc.min_margin = acc.min_margin.min(logits[pred] - second);
    }
    let mut dlogit = [0.0f32; N_CLASSES];
    for o in 0..N_CLASSES {
        dlogit[o] = probs[o] - if o == label { 1.0 } else { 0.0 };
        dlogit[o] /= n;
    }
    backward(streams, pool, dlogit, net, caches, acc);
}

/// Live weights: one struct so helpers take the net as a single argument
/// instead of four slices. Copy (shared refs): every frame reborrows it
/// freely inside the batch loop.
#[derive(Clone, Copy)]
struct TrainNet<'w> {
    conv_w: &'w [f32; N_FILTERS * FILTER_WEIGHTS],
    conv_b: &'w [f32; N_FILTERS],
    dense_w: &'w [f32; N_CLASSES * DENSE_TOTAL],
    dense_b: &'w [f32; N_CLASSES],
}

/// Backprop one frame's logit error through the two-stream net, adding
/// into the accumulator: dense grads, then per-stream rematerialized
/// conv grads (forward leaves only the last stream cached). Conv filters
/// are shared, so both streams accumulate into the same weights.
fn backward(
    streams: &Streams,
    pool: [f32; DENSE_TOTAL],
    dlogit: [f32; N_CLASSES],
    net: TrainNet<'_>,
    caches: &mut Caches,
    acc: &mut GradAccum,
) {
    // Dense grads + pool error (both streams concatenate).
    let mut dpool = [0.0f32; DENSE_TOTAL];
    for (o, &d) in dlogit.iter().enumerate() {
        acc.g_dense_b[o] += d;
        for i in 0..DENSE_TOTAL {
            acc.g_dense_w[o * DENSE_TOTAL + i] += d * pool[i];
            dpool[i] += d * net.dense_w[o * DENSE_TOTAL + i];
        }
    }
    let mut scratch = [0.0f32; DENSE_INPUTS];
    for s in 0..STREAMS {
        let (y_min, y_max) = stream_y_range(s);
        forward_stream(
            &streams[s],
            net.conv_w,
            net.conv_b,
            caches,
            &mut scratch,
            y_min,
            y_max,
        );
        let err = &dpool[s * DENSE_INPUTS..(s + 1) * DENSE_INPUTS];
        // Unpool (argmax routing) + ReLU gate.
        let mut dconv = [0.0f32; N_FILTERS * GRID * GRID];
        for ch in 0..N_FILTERS {
            for r in 0..POOLED {
                for c in 0..POOLED {
                    let mut best = (0, 0);
                    let mut m = f32::NEG_INFINITY;
                    for kr in 0..POOL {
                        for kc in 0..POOL {
                            let v = caches.conv
                                [ch * GRID_PIXELS + (r * POOL + kr) * GRID + (c * POOL + kc)];
                            if v > m {
                                m = v;
                                best = (kr, kc);
                            }
                        }
                    }
                    if m > 0.0 {
                        dconv
                            [ch * GRID_PIXELS + (r * POOL + best.0) * GRID + (c * POOL + best.1)] =
                            err[ch * POOL_FEATURES + r * POOLED + c];
                    }
                }
            }
        }
        // Conv grads (accumulate: filters are shared across streams).
        for oc in 0..N_FILTERS {
            for r in 0..GRID {
                for c in 0..GRID {
                    let d = dconv[oc * GRID_PIXELS + r * GRID + c];
                    if d == 0.0 {
                        continue;
                    }
                    acc.g_conv_b[oc] += d;
                    for ic in 0..INPUT_CHANNELS {
                        for kr in 0..KERNEL {
                            let nr = r as isize + kr as isize - KERNEL_PAD;
                            if nr < 0 || nr >= GRID as isize {
                                continue;
                            }
                            for kc in 0..KERNEL {
                                let nc = c as isize + kc as isize - KERNEL_PAD;
                                if nc < 0 || nc >= GRID as isize {
                                    continue;
                                }
                                acc.g_conv_w[oc * FILTER_WEIGHTS
                                    + ic * KERNEL * KERNEL
                                    + kr * KERNEL
                                    + kc] += d * caches.chw
                                    [ic * GRID_PIXELS + nr as usize * GRID + nc as usize];
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Best-checkpoint state for patience stopping: lowest holdout loss, and
/// the weights that earned it. What ships is the best observed model,
/// never the last epoch's.
struct Checkpoint {
    hold_loss: f32,
    hold_hits: usize,
    hold_total: usize,
    margin: f32,
    epoch: usize,
    conv_w: [f32; N_FILTERS * FILTER_WEIGHTS],
    conv_b: [f32; N_FILTERS],
    dense_w: [f32; N_CLASSES * DENSE_TOTAL],
    dense_b: [f32; N_CLASSES],
    stale_evals: usize,
}

/// Minimum holdout-loss drop that counts as improvement. Below this the
/// clock keeps running: without a floor, floating-point dust resets
/// patience forever and the run never stops.
const MIN_LOSS_DELTA: f32 = 1e-4;

impl Checkpoint {
    fn initial(
        conv_w: &[f32; N_FILTERS * FILTER_WEIGHTS],
        conv_b: &[f32; N_FILTERS],
        dense_w: &[f32; N_CLASSES * DENSE_TOTAL],
        dense_b: &[f32; N_CLASSES],
    ) -> Self {
        Self {
            hold_loss: f32::INFINITY,
            hold_hits: 0,
            hold_total: 0,
            margin: f32::NEG_INFINITY,
            epoch: 0,
            conv_w: *conv_w,
            conv_b: *conv_b,
            dense_w: *dense_w,
            dense_b: *dense_b,
            stale_evals: 0,
        }
    }
    /// Fold one epoch's reports in. A holdout-loss drop beyond
    /// MIN_LOSS_DELTA refreshes the shipped weights and resets the clock;
    /// anything else advances it. Hit counts and margin report only.
    fn observe(
        &mut self,
        epoch: usize,
        hold: &EvalReport,
        base_margin: f32,
        net: TrainNet<'_>,
    ) -> bool {
        if hold.mean_loss < self.hold_loss - MIN_LOSS_DELTA {
            self.hold_loss = hold.mean_loss;
            self.hold_hits = hold.hits;
            self.hold_total = hold.total;
            self.margin = base_margin;
            self.epoch = epoch;
            self.conv_w = *net.conv_w;
            self.conv_b = *net.conv_b;
            self.dense_w = *net.dense_w;
            self.dense_b = *net.dense_b;
            self.stale_evals = 0;
            true
        } else {
            self.stale_evals += 1;
            false
        }
    }
}

/// One deterministic evaluation over a named sample set: hits, worst-case
/// margin over correctly-called frames, per-class hits, mean loss, and the
/// miss list. Shared by the base-capture gate, the final verdict, and the
/// holdout report, so the three can never drift apart again.
struct EvalReport {
    hits: usize,
    total: usize,
    margin: f32,
    per_class: [usize; N_CLASSES],
    mean_loss: f32,
    misses: Vec<(String, usize, usize, f32)>,
}

fn evaluate(
    samples: &[Base],
    conv_w: &[f32],
    conv_b: &[f32],
    dense_w: &[f32],
    dense_b: &[f32],
    caches: &mut Caches,
) -> EvalReport {
    let mut report = EvalReport {
        hits: 0,
        total: samples.len(),
        margin: f32::INFINITY,
        per_class: [0; N_CLASSES],
        mean_loss: 0.0,
        misses: Vec::new(),
    };
    for (streams, label, name) in samples {
        let (got, margin, loss) =
            frame_stats(streams, *label, conv_w, conv_b, dense_w, dense_b, caches);
        report.mean_loss += loss;
        if got == *label {
            report.hits += 1;
            report.per_class[*label] += 1;
            report.margin = report.margin.min(margin);
        } else {
            report.misses.push((name.clone(), *label, got, loss));
        }
    }
    if report.total > 0 {
        report.mean_loss /= report.total as f32;
    }
    report
}

/// Holdout samples, loaded once at startup: `tests/fixtures-holdout/`
/// `<class>/*.png` (near-duplicate run frames plus unseen layouts — the
/// class directory supplies the expected label, no lists).
fn load_holdout() -> Vec<Base> {
    let mut out = Vec::new();
    let hold_root = format!("{}/tests/fixtures-holdout", env!("CARGO_MANIFEST_DIR"));
    if !std::path::Path::new(&hold_root).is_dir() {
        return out;
    }
    for (dir, label) in CLASS_DIRS {
        let dir_path = format!("{hold_root}/{dir}");
        if !std::path::Path::new(&dir_path).is_dir() {
            continue;
        }
        let mut names: Vec<_> = std::fs::read_dir(&dir_path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".png"))
            .collect();
        names.sort();
        for name in names {
            let (rgb, w, h) = load_rgb_8bit(&format!("{dir_path}/{name}"));
            let (w, h) = (w as usize, h as usize);
            let full = downsample_to_grid_rgb(&rgb, w, h);
            let (x0, y0, x1, y1) = crop_region(w, h);
            let crop = downsample_region_to_grid_rgb(&rgb, w, h, x0, y0, x1, y1);
            out.push(([full, crop], label as usize, format!("{dir}/{name}")));
        }
    }
    out
}

fn main() {
    let (files, bases) = corpus();
    let holdout = load_holdout();
    println!("base captures: {} files", files.len());
    println!("holdout files: {}", holdout.len());
    let mut rng = Lcg(INIT_SEED);
    // He init scaled by each layer's fan-in.
    const CONV_FAN_IN: f32 = FILTER_WEIGHTS as f32;
    const DENSE_FAN_IN: f32 = DENSE_TOTAL as f32;
    let mut conv_w = [0.0f32; N_FILTERS * FILTER_WEIGHTS];
    let mut conv_b = [0.0f32; N_FILTERS];
    let mut dense_w = [0.0f32; N_CLASSES * DENSE_TOTAL];
    let mut dense_b = [0.0f32; N_CLASSES];
    for v in conv_w.iter_mut() {
        *v = (rng.next_unit() as f32 * 2.0 - 1.0) * (6.0 / CONV_FAN_IN).sqrt();
    }
    for v in dense_w.iter_mut() {
        *v = (rng.next_unit() as f32 * 2.0 - 1.0) * (6.0 / DENSE_FAN_IN).sqrt();
    }
    let n = BATCH as f32;
    let mut file_counts = [0usize; N_CLASSES];
    for (_, label, _) in &files {
        file_counts[*label] += 1;
    }
    for (c, count) in file_counts.iter().enumerate() {
        assert!(
            *count > 0,
            "class {:?} has no training captures",
            BY_INDEX[c]
        );
    }
    println!(
        "base files: {}",
        BY_INDEX
            .iter()
            .enumerate()
            .map(|(c, v)| format!("{v:?}:{}", file_counts[c]))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("views/class/epoch: {VIEWS_PER_CLASS} (batch {BATCH})");
    println!(
        "patience metric: HOLDOUT loss (min drop {MIN_LOSS_DELTA}), stops after {PATIENCE_EPOCHS} stale epochs"
    );
    // Reused every epoch: `clear` keeps the allocation, so per-epoch
    // allocator traffic is zero (views carry no names; the verdict names
    // files, never batch frames).
    let mut batch: Vec<(Streams, usize)> = Vec::with_capacity(BATCH);
    let mut caches = Caches {
        chw: [0.0; INPUT_CHANNELS * GRID * GRID],
        conv: [0.0; N_FILTERS * GRID * GRID],
    };
    // Best-checkpoint state for patience stopping: holdout score, base
    // margin tiebreak, and the weights that earned them. What ships is the
    // best observed model, never the last epoch's.
    let mut best = Checkpoint::initial(&conv_w, &conv_b, &dense_w, &dense_b);
    for epoch in 0..EPOCHS {
        // Fresh batch every epoch: cycle each class's own base captures
        // with new stochastic draws. Epoch-seeded, so the same epoch
        // always draws the same views across runs.
        let mut aug_rng = Lcg(AUG_SEED ^ (epoch as u64).wrapping_mul(0x9E3779B97F4A7C15));
        batch.clear();
        for (c, pool) in bases.iter().enumerate() {
            for k in 0..VIEWS_PER_CLASS {
                // One draw, both streams: coherence (see ViewParams).
                let params = draw_view_params(&mut aug_rng);
                let base = &pool[k % pool.len()];
                batch.push((
                    [apply_view(&params, &base[0]), apply_view(&params, &base[1])],
                    c,
                ));
            }
        }
        let mut acc = GradAccum::zero();
        let net = TrainNet {
            conv_w: &conv_w,
            conv_b: &conv_b,
            dense_w: &dense_w,
            dense_b: &dense_b,
        };
        for (streams, label) in &batch {
            train_frame(streams, *label, net, &mut caches, &mut acc, n);
        }
        for (w, g) in conv_w.iter_mut().zip(acc.g_conv_w.iter()) {
            *w -= LEARNING_RATE * g;
        }
        for (w, g) in conv_b.iter_mut().zip(acc.g_conv_b.iter()) {
            *w -= LEARNING_RATE * g;
        }
        for (w, g) in dense_w.iter_mut().zip(acc.g_dense_w.iter()) {
            *w -= LEARNING_RATE * g;
        }
        for (w, g) in dense_b.iter_mut().zip(acc.g_dense_b.iter()) {
            *w -= LEARNING_RATE * g;
        }

        // TODO: Leave commented-out or just log every epoch?
        // if epoch % 10 == 0 || epoch + 1 == EPOCHS {
        {
            let per_class: Vec<String> = BY_INDEX
                .iter()
                .enumerate()
                .map(|(c, v)| format!("{v:?}:{}", acc.class_correct[c]))
                .collect();
            println!(
                "epoch {epoch}: TRAIN loss {:.4} acc {}/{BATCH} min_margin {:.2} per_class [{}]",
                acc.loss / n,
                acc.correct,
                acc.min_margin,
                per_class.join(" ")
            );
        }
        // Gate: deterministic eval over the raw base captures (never
        // trained on directly), so redrawn views can never move the
        // goalposts. Post-update weights: this judges what ships.
        let base = evaluate(&files, &conv_w, &conv_b, &dense_w, &dense_b, &mut caches);
        // Patience on the holdout, not a fixed-accuracy gate: the corpus
        // holds a known-unlearnable pair, so 100% never fires. Best
        // holdout wins; base margin breaks ties toward robust models.
        let hold = evaluate(&holdout, &conv_w, &conv_b, &dense_w, &dense_b, &mut caches);
        for (name, want, got, loss) in &hold.misses {
            println!(
                "HOLDOUT MISS {name}: want {:?}({want}) got {:?}({got}) loss {loss:.4}",
                BY_INDEX[*want], BY_INDEX[*got]
            );
        }
        best.observe(
            epoch,
            &hold,
            base.margin,
            TrainNet {
                conv_w: &conv_w,
                conv_b: &conv_b,
                dense_w: &dense_w,
                dense_b: &dense_b,
            },
        );
        // Patience resets on strict holdout improvement only (inside
        // observe): a margin tiebreak refreshes the shipped weights without
        // resetting the clock, or a 22/23 seesaw with creeping margin
        // stalls the stop forever.
        let stale_evals = best.stale_evals;
        // One labeled line per epoch: TRAIN is the sampled batch, BASE
        // the raw captures, HOLDOUT the unseen set patience judges. Both
        // losses print as decimals so progress reads across the line. BEST
        // carries the loss patience actually judges, so the target is
        // visible every epoch.
        println!(
            "epoch {epoch}: HOLDOUT {}/{} loss {:.4} | BASE {}/{} margin {:.2} | STALE {stale_evals}/{PATIENCE_EPOCHS} | BEST loss {:.4} ({}/{})@e{}",
            hold.hits,
            hold.total,
            hold.mean_loss,
            base.hits,
            files.len(),
            base.margin,
            best.hold_loss,
            best.hold_hits,
            best.hold_total,
            best.epoch
        );
        if base.hits == files.len() && base.margin >= REQUIRED_MARGIN {
            println!("converged at epoch {epoch}");
            break;
        }
        if best.stale_evals >= PATIENCE_EPOCHS {
            println!(
                "early stop at epoch {epoch} ({PATIENCE_EPOCHS} epochs without improvement; best was {})",
                best.epoch
            );
            break;
        }
    }

    // Ship the best observed weights, never the last epoch's.
    (conv_w, conv_b, dense_w, dense_b) = (best.conv_w, best.conv_b, best.dense_w, best.dense_b);
    println!(
        "best epoch {}: holdout loss {:.4} ({}/{}), base_margin {:.2}",
        best.epoch, best.hold_loss, best.hold_hits, best.hold_total, best.margin
    );

    // Final verdict on the raw base captures (no augmentation leniency).
    let verdict = evaluate(&files, &conv_w, &conv_b, &dense_w, &dense_b, &mut caches);
    for (name, want, got, loss) in &verdict.misses {
        println!(
            "MISCLASSIFIED {name}: want {:?}({want}) got {:?}({got}) loss {loss:.4}",
            BY_INDEX[*want], BY_INDEX[*got]
        );
    }
    // Holdout on the shipped (best) weights. Decisive for stopping, but
    // the run always exits 0: a human decides merge, and the misses print
    // by name above and below.
    let hold = evaluate(&holdout, &conv_w, &conv_b, &dense_w, &dense_b, &mut caches);
    for (name, want, got, loss) in &hold.misses {
        println!(
            "HOLDOUT MISS {name}: want {:?}({want}) got {:?}({got}) loss {loss:.4}",
            BY_INDEX[*want], BY_INDEX[*got]
        );
    }
    let out_path = format!("{}/artifacts/weights.bin", env!("CARGO_MANIFEST_DIR"));
    let mut bytes = Vec::with_capacity(N_WEIGHTS * 4);
    for v in conv_w
        .iter()
        .chain(conv_b.iter())
        .chain(dense_w.iter())
        .chain(dense_b.iter())
    {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(bytes.len(), N_WEIGHTS * 4);
    std::fs::create_dir_all(format!("{}/artifacts", env!("CARGO_MANIFEST_DIR"))).unwrap();
    std::fs::write(&out_path, &bytes).unwrap();
    println!("wrote {out_path} ({} bytes)", bytes.len());
    println!(
        "done: best epoch {}, base misses {}, holdout {}/{} loss {:.4}",
        best.epoch,
        verdict.misses.len(),
        hold.hits,
        hold.total,
        hold.mean_loss
    );
}
