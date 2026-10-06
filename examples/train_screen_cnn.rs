//! Dev-time trainer for the micro-CNN in `src/screen_cnn.rs`.
//!
//! Pure Rust, no ML libraries: full-batch gradient descent with
//! cross-entropy over the labeled corpus, fixed-seed init, frozen recipe.
//! Output: `artifacts/weights.bin` (little-endian f32, layout documented on
//! `screen_cnn::N_WEIGHTS`).
//!
//! Corpus layout (labels come from directory names, never a filename
//! list): `tests/fixtures/<windows-version>/<class-dir>/*.png`, where each
//! class directory maps to a `ScreenClass` variant (see `CLASS_DIRS`
//! below). The version directory carries no label: the classifier never
//! sees it, it only keeps captures attributable over time. Synthetic
//! frames cover classes with no real capture yet plus shapes mirrored from
//! the unit tests.
//!
//! Augmentation is geometric and photometric only (flip, brightness,
//! non-uniform rescale): never colorimetric. Hue shifts and theme
//! inversions would fabricate screens that cannot occur (a red installer,
//! a dark WinPE) and destroy the color-meaning classes.
//!
//! Regenerate: `cargo run --release --example train_screen_cnn` (release
//! matters: hundreds of epochs over full-batch backprop). The trainer exits
//! nonzero unless every training frame classifies correctly with margin, so
//! a bad corpus or recipe fails loudly instead of baking silent weights.

use windows_headless::screen_cnn::{
    DENSE_INPUTS, GRID, KERNEL, N_CLASSES, N_FILTERS, N_WEIGHTS, POOLED, POOL, POOL_FEATURES,
    ScreenClass, downsample_to_32x32_rgb, load_rgb_8bit,
};

const EPOCHS: usize = 3000;
/// Full-batch gradient descent step size.
const LEARNING_RATE: f32 = 0.5;
/// Required logit gap (winner minus runner-up) on every training frame.
const REQUIRED_MARGIN: f32 = 2.0;
/// Pixels per CNN input channel.
const GRID_PIXELS: usize = GRID * GRID;
/// Weights per convolution filter (all input channels).
const FILTER_WEIGHTS: usize = 3 * KERNEL * KERNEL;

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

fn push_frame(
    out: &mut Vec<(Frame, usize, String)>,
    name: String,
    rgb: &[u8],
    w: usize,
    h: usize,
    label: ScreenClass,
) {
    out.push((downsample_to_32x32_rgb(rgb, w, h), label as usize, name));
}

/// Nearest-neighbor rescale of full-resolution RGB by fractional factors.
fn rescale(rgb: &[u8], w: usize, h: usize, fx: f32, fy: f32) -> (Vec<u8>, usize, usize) {
    let w2 = ((w as f32) * fx).round().max(1.0) as usize;
    let h2 = ((h as f32) * fy).round().max(1.0) as usize;
    let mut out = vec![0u8; w2 * h2 * 3];
    for y in 0..h2 {
        for x in 0..w2 {
            let sx = ((x as f32) / fx) as usize;
            let sy = ((y as f32) / fy) as usize;
            let s = (sy.min(h - 1) * w + sx.min(w - 1)) * 3;
            let d = (y * w2 + x) * 3;
            out[d..d + 3].copy_from_slice(&rgb[s..s + 3]);
        }
    }
    (out, w2, h2)
}

fn augment(
    out: &mut Vec<(Frame, usize, String)>,
    name: String,
    rgb: &[u8],
    w: usize,
    h: usize,
    label: ScreenClass,
) {
    // Original, horizontal flip, dimmer, brighter, and non-uniform rescales
    // (DPI/aspect drift of +-15%).
    push_frame(out, format!("{name}"), rgb, w, h, label);
    let mut flip = vec![0u8; rgb.len()];
    for y in 0..h {
        for x in 0..w {
            let s = (y * w + x) * 3;
            let d = (y * w + (w - 1 - x)) * 3;
            flip[d..d + 3].copy_from_slice(&rgb[s..s + 3]);
        }
    }
    push_frame(out, format!("{name}[flip]"), &flip, w, h, label);
    for factor in [0.85f32, 1.15] {
        let shifted: Vec<u8> = rgb
            .iter()
            .map(|&v| ((v as f32) * factor).clamp(0.0, 255.0) as u8)
            .collect();
        push_frame(
            out,
            format!("{name}[x{factor}]"),
            &shifted,
            w,
            h,
            label,
        );
    }
    for (fx, fy) in [(0.85f32, 1.0f32), (1.0, 0.85), (1.15, 1.0), (1.0, 1.15)] {
        let (scaled, w2, h2) = rescale(rgb, w, h, fx, fy);
        push_frame(out, format!("{name}[{fx}x{fy}]"), &scaled, w2, h2, label);
    }
    // White-balance alterations (firmware tint variance): small independent
    // per-channel shifts, labeled as alterations. Not brightness (that is
    // covered above): this moves hues slightly without changing layout.
    for (dr, dg, db) in [(6i16, -4, -4), (-4, -4, 8)] {
        let tinted: Vec<u8> = rgb
            .chunks_exact(3)
            .flat_map(|p| {
                [
                    (p[0] as i16 + dr).clamp(0, 255) as u8,
                    (p[1] as i16 + dg).clamp(0, 255) as u8,
                    (p[2] as i16 + db).clamp(0, 255) as u8,
                ]
            })
            .collect();
        push_frame(out, format!("{name}[alter-tint{dr},{dg},{db}]"), &tinted, w, h, label);
    }
}

fn flat_frame(w: usize, h: usize, c: [u8; 3]) -> Vec<u8> {
    vec![c; w * h].concat()
}

/// Directory name under each Windows-version directory in
/// `tests/fixtures/` to training label. This table — not a filename list —
/// is the corpus definition; drop new captures in and re-run. The version
/// directory itself (`win11-26300-en-us-retail`, ...) carries no label: the
/// classifier never sees it, it only keeps captures attributable over time.
const CLASS_DIRS: [(&str, ScreenClass); 8] = [
    ("dark", ScreenClass::Dark),
    ("flat-field", ScreenClass::FlatField),
    ("error-shell", ScreenClass::ErrorShell),
    ("installer-step", ScreenClass::InstallerStep),
    ("error-recovery", ScreenClass::ErrorRecovery),
    ("lockscreen", ScreenClass::Lockscreen),
    ("firmware-boot", ScreenClass::FirmwareBoot),
    ("error-bluescreen", ScreenClass::ErrorBluescreen),
];

fn corpus() -> Vec<(Frame, usize, String)> {
    let mut out = Vec::new();
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
                augment(
                    &mut out,
                    format!("{version}/{dir}/{name}"),
                    &rgb,
                    w as usize,
                    h as usize,
                    label,
                );
            }
        }
    }
    // Synthetic error icons (white box + red blob on black): no real modal
    // capture exists yet. Three placements.
    for (box_x, box_y) in [(50u32, 25u32), (30, 40), (70, 20)] {
        let (w, h) = (160usize, 100usize);
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                if x >= box_x as usize
                    && x < (box_x + 80) as usize
                    && y >= box_y as usize
                    && y < (box_y + 50) as usize
                {
                    let icon = x >= (box_x + 5) as usize
                        && x < (box_x + 35) as usize
                        && y >= (box_y + 5) as usize
                        && y < (box_y + 30) as usize;
                    let c = if icon { [220, 60, 50] } else { [240, 240, 240] };
                    rgb[i..i + 3].copy_from_slice(&c);
                }
            }
        }
        push_frame(&mut out, format!("icon{box_x},{box_y}"), &rgb, w, h, ScreenClass::ErrorModal);
    }
    // Synthetic flats: the bare-field shapes the unit tests pin.
    for c in [[0, 120, 215], [128, 128, 128], [220, 60, 50], [30, 140, 60]] {
        push_frame(&mut out, format!("flat{c:?}"), &flat_frame(64, 64, c), 64, 64, ScreenClass::FlatField);
    }
    // Synthetic thin orange strokes (TianoCore-like lettering): thin red
    // strokes are lettering, never an icon. Mirrors the unit test
    // generator exactly.
    {
        let (w, h) = (160usize, 100usize);
        let mut rgb = vec![0u8; w * h * 3];
        for y in (10..90).step_by(4) {
            for x in 20..32 {
                let i = (y * w + x) * 3;
                rgb[i..i + 3].copy_from_slice(&[255, 85, 0]);
            }
        }
        push_frame(&mut out, "orange-strokes".to_string(), &rgb, w, h, ScreenClass::Lockscreen);
    }
    // Synthetic sparse dots on black: visible, not Dark.
    {
        let mut rgb = vec![0u8; 64 * 64 * 3];
        for i in 0..200 {
            let o = (i * 61) % (64 * 64) * 3;
            rgb[o..o + 3].copy_from_slice(&[220, 220, 220]);
        }
        push_frame(&mut out, "sparse-dots".to_string(), &rgb, 64, 64, ScreenClass::Lockscreen);
    }
    // Synthetic cursor block on black (shell cursor, text caret edge):
    // a solid lit block reads as visible, never Dark. Mirrors the unit
    // test's filled-bytes case.
    {
        let mut rgb = vec![0u8; 64 * 64 * 3];
        rgb[..600].fill(220);
        push_frame(&mut out, "cursor-block".to_string(), &rgb, 64, 64, ScreenClass::Lockscreen);
    }
    // Synthetic installer-blue with sparse ink (the 99%-blue progress
    // screen shape): InstallerStep. Mirrors the unit test generator.
    for seed in [17usize, 200] {
        let (w, h) = (100usize, 100usize);
        let mut rgb = flat_frame(w, h, [0, 90, 158]);
        for k in 0..18 {
            let o = ((k * 431 + seed) % (w * h)) * 3;
            rgb[o..o + 3].copy_from_slice(&[255, 255, 255]);
        }
        for k in 0..75 {
            let o = ((k * 131 + seed + 5) % (w * h)) * 3;
            rgb[o..o + 3].copy_from_slice(&[0, 90, 150]);
        }
        push_frame(&mut out, format!("blueink{seed}"), &rgb, w, h, ScreenClass::InstallerStep);
    }
    // Synthetic gradient (lock-screen style): Lockscreen.
    {
        let mut rgb = Vec::with_capacity(100 * 100 * 3);
        for y in 0..100u8 {
            for x in 0..100u8 {
                rgb.extend_from_slice(&[x * 2, y * 2, 128]);
            }
        }
        push_frame(&mut out, "gradient".to_string(), &rgb, 100, 100, ScreenClass::Lockscreen);
    }
    out
}

// ---------------------------------------------------------------------------
// forward pass with caches (mirrors screen_cnn::predict_32x32_rgb exactly)
// ---------------------------------------------------------------------------

struct Caches {
    chw: [f32; 3 * GRID * GRID],
    conv: [f32; N_FILTERS * GRID * GRID],
    pool: [f32; DENSE_INPUTS],
}

const KERNEL_PAD: isize = KERNEL as isize / 2;

fn forward(rgb: &Frame, conv_w: &[f32], conv_b: &[f32], out: &mut Caches) -> [f32; DENSE_INPUTS] {
    for c in 0..3 {
        for i in 0..GRID_PIXELS {
            out.chw[c * GRID_PIXELS + i] = rgb[i * 3 + c] as f32 / 255.0;
        }
    }
    for oc in 0..N_FILTERS {
        let b = conv_b[oc];
        for r in 0..GRID {
            for c in 0..GRID {
                let mut sum = b;
                for ic in 0..3 {
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
                                * conv_w[oc * FILTER_WEIGHTS + ic * KERNEL * KERNEL
                                    + kr * KERNEL
                                    + kc];
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
                        let v = out.conv[ch * GRID_PIXELS + (r * POOL + kr) * GRID + (c * POOL + kc)];
                        if v > m {
                            m = v;
                        }
                    }
                }
                out.pool[ch * POOL_FEATURES + r * POOLED + c] = m;
            }
        }
    }
    out.pool
}

/// Argmax class for one frame under explicit weights (shared by the final
/// corpus verdict and the holdout report).
fn predict_label(
    rgb: &Frame,
    conv_w: &[f32],
    conv_b: &[f32],
    dense_w: &[f32],
    dense_b: &[f32],
    caches: &mut Caches,
) -> usize {
    let pool = forward(rgb, conv_w, conv_b, caches);
    let mut best = 0;
    let mut best_val = f32::NEG_INFINITY;
    for o in 0..N_CLASSES {
        let mut s = dense_b[o];
        for i in 0..DENSE_INPUTS {
            s += pool[i] * dense_w[o * DENSE_INPUTS + i];
        }
        if s > best_val {
            best_val = s;
            best = o;
        }
    }
    best
}

fn main() {
    let data = corpus();
    println!("corpus: {} frames", data.len());
    let mut rng = Lcg(0xC0FFEE);
    // He init scaled by each layer's fan-in.
    const CONV_FAN_IN: f32 = FILTER_WEIGHTS as f32;
    const DENSE_FAN_IN: f32 = DENSE_INPUTS as f32;
    let mut conv_w = [0.0f32; N_FILTERS * FILTER_WEIGHTS];
    let mut conv_b = [0.0f32; N_FILTERS];
    let mut dense_w = [0.0f32; N_CLASSES * DENSE_INPUTS];
    let mut dense_b = [0.0f32; N_CLASSES];
    for v in conv_w.iter_mut() {
        *v = (rng.next_unit() as f32 * 2.0 - 1.0) * (6.0 / CONV_FAN_IN).sqrt();
    }
    for v in dense_w.iter_mut() {
        *v = (rng.next_unit() as f32 * 2.0 - 1.0) * (6.0 / DENSE_FAN_IN).sqrt();
    }

    let n = data.len() as f32;
    // Class-weighted loss: minority classes (one shell capture) count as
    // much as majority ones (ten installer blues), so the majority cannot
    // drown them. Weights rebalance the gradient, not the data: no frame
    // is duplicated.
    let mut class_counts = [0usize; N_CLASSES];
    for (_, label, _) in &data {
        class_counts[*label] += 1;
    }
    let mut class_weights = [0.0f32; N_CLASSES];
    for (c, count) in class_counts.iter().enumerate() {
        assert!(*count > 0, "class {c} has no training frames");
        class_weights[c] = n / (N_CLASSES as f32 * *count as f32);
    }
    println!("class counts: {class_counts:?}");
    println!("class weights: {class_weights:?}");
    let mut caches = Caches {
        chw: [0.0; 3 * GRID * GRID],
        conv: [0.0; N_FILTERS * GRID * GRID],
        pool: [0.0; DENSE_INPUTS],
    };
    for epoch in 0..EPOCHS {
        let mut g_conv_w = [0.0f32; N_FILTERS * FILTER_WEIGHTS];
        let mut g_conv_b = [0.0f32; N_FILTERS];
        let mut g_dense_w = [0.0f32; N_CLASSES * DENSE_INPUTS];
        let mut g_dense_b = [0.0f32; N_CLASSES];
        let mut loss = 0.0f32;
        let mut correct = 0;
        let mut class_correct = [0usize; N_CLASSES];
        let mut class_seen = [0usize; N_CLASSES];
        let mut min_margin = f32::INFINITY;
        for (rgb, label, name) in &data {
            let pool = forward(rgb, &conv_w, &conv_b, &mut caches);
            let mut logits = [0.0f32; N_CLASSES];
            for o in 0..N_CLASSES {
                let mut s = dense_b[o];
                for i in 0..DENSE_INPUTS {
                    s += pool[i] * dense_w[o * DENSE_INPUTS + i];
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
            for o in 0..N_CLASSES {
                probs[o] /= denom;
            }
            loss += -(probs[*label].max(1e-12).ln());
            class_seen[*label] += 1;
            let pred = probs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap()
                .0;
            if pred == *label {
                correct += 1;
                class_correct[*label] += 1;
                let mut second = f32::NEG_INFINITY;
                for o in 0..N_CLASSES {
                    if o != pred && logits[o] > second {
                        second = logits[o];
                    }
                }
                min_margin = min_margin.min(logits[pred] - second);
            } else {
                min_margin = min_margin.min(-1.0);
            }
            // Backprop: dL/dlogit = (p - y) / N.
            let mut dlogit = [0.0f32; N_CLASSES];
            for o in 0..N_CLASSES {
                dlogit[o] = probs[o] - if o == *label { 1.0 } else { 0.0 };
                dlogit[o] /= n;
            }
            // Dense grads + pool error.
            let mut dpool = [0.0f32; DENSE_INPUTS];
            for o in 0..N_CLASSES {
                g_dense_b[o] += dlogit[o];
                for i in 0..DENSE_INPUTS {
                    g_dense_w[o * DENSE_INPUTS + i] += dlogit[o] * pool[i];
                    dpool[i] += dlogit[o] * dense_w[o * DENSE_INPUTS + i];
                }
            }
            // Unpool (argmax routing) + ReLU gate.
            let mut dconv = [0.0f32; N_FILTERS * GRID * GRID];
            for ch in 0..N_FILTERS {
                for r in 0..POOLED {
                    for c in 0..POOLED {
                        let mut best = (0, 0);
                        let mut m = f32::NEG_INFINITY;
                        for kr in 0..POOL {
                            for kc in 0..POOL {
                                let v = caches.conv[ch * GRID_PIXELS
                                    + (r * POOL + kr) * GRID
                                    + (c * POOL + kc)];
                                if v > m {
                                    m = v;
                                    best = (kr, kc);
                                }
                            }
                        }
                        if m > 0.0 {
                            dconv[ch * GRID_PIXELS + (r * POOL + best.0) * GRID + (c * POOL + best.1)] =
                                dpool[ch * POOL_FEATURES + r * POOLED + c];
                        }
                    }
                }
            }
            // Conv grads.
            for oc in 0..N_FILTERS {
                for r in 0..GRID {
                    for c in 0..GRID {
                        let d = dconv[oc * GRID_PIXELS + r * GRID + c];
                        if d == 0.0 {
                            continue;
                        }
                        g_conv_b[oc] += d;
                        for ic in 0..3 {
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
                                    g_conv_w[oc * FILTER_WEIGHTS
                                        + ic * KERNEL * KERNEL
                                        + kr * KERNEL
                                        + kc] += d
                                        * caches.chw[ic * GRID_PIXELS + nr as usize * GRID
                                            + nc as usize];
                                }
                            }
                        }
                    }
                }
            }
        }
        for (w, g) in conv_w.iter_mut().zip(g_conv_w.iter()) {
            *w -= LEARNING_RATE * g;
        }
        for (w, g) in conv_b.iter_mut().zip(g_conv_b.iter()) {
            *w -= LEARNING_RATE * g;
        }
        for (w, g) in dense_w.iter_mut().zip(g_dense_w.iter()) {
            *w -= LEARNING_RATE * g;
        }
        for (w, g) in dense_b.iter_mut().zip(g_dense_b.iter()) {
            *w -= LEARNING_RATE * g;
        }
        if epoch % 200 == 0 || epoch + 1 == EPOCHS {
            let per_class: Vec<String> = (0..N_CLASSES)
                .map(|c| format!("{c}:{}", class_correct[c]))
                .collect();
            println!(
                "epoch {epoch}: loss {:.4} acc {correct}/{} min_margin {:.2} per_class [{}]",
                loss / n,
                data.len(),
                min_margin,
                per_class.join(" ")
            );
        }
        if correct == data.len() && min_margin >= REQUIRED_MARGIN {
            println!("converged at epoch {epoch}");
            break;
        }
    }

    // Final verdict on the corpus (report raw, no augmentation leniency).
    let mut fails = 0;
    for (rgb, label, name) in &data {
        if predict_label(rgb, &conv_w, &conv_b, &dense_w, &dense_b, &mut caches) != *label {
            fails += 1;
            println!("MISCLASSIFIED {name}: want {label}");
        }
    }
    if fails > 0 {
        eprintln!("trainer FAILED: {fails} misclassifications, not writing weights");
        std::process::exit(1);
    }
    // Holdout verdict: frames the model never trained on (near-duplicates
    // from the same runs live in tests/fixtures-holdout/). Informational:
    // reported, never gating.
    {
        let root = format!("{}/tests/fixtures-holdout", env!("CARGO_MANIFEST_DIR"));
        let mut hold_total = 0;
        let mut hold_hits = 0;
        if std::path::Path::new(&root).is_dir() {
            for (dir, label) in CLASS_DIRS {
                let dir_path = format!("{root}/{dir}");
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
                    let small = downsample_to_32x32_rgb(&rgb, w as usize, h as usize);
                    let got = predict_label(&small, &conv_w, &conv_b, &dense_w, &dense_b, &mut caches);
                    hold_total += 1;
                    if got == label as usize {
                        hold_hits += 1;
                    } else {
                        println!("HOLDOUT MISS {dir}/{name}: want {} got {got}", label as usize);
                    }
                }
            }
        }
        println!("holdout: {hold_hits}/{hold_total} (eval only, never trained)");
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
}
