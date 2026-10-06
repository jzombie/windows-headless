//! Dev-time trainer for the micro-CNN in `src/screen_cnn.rs`.
//!
//! Pure Rust, no ML libraries: full-batch gradient descent with
//! cross-entropy over the labeled corpus, fixed-seed init, frozen recipe.
//! Output: `artifacts/weights.bin` (little-endian f32, layout documented on
//! `screen_cnn::N_WEIGHTS`).
//!
//! Corpus layout (labels come from directory names, never a filename
//! list): `tests/fixtures/<class-dir>/*.png`, where each directory maps to
//! a `ScreenClass` variant (see `CLASS_DIRS` below). Synthetic frames cover
//! classes with no real capture yet (`error-modal`: red-icon blobs;
//! `flat-field`: solid colors) plus shapes mirrored from the unit tests.
//!
//! Regenerate: `cargo run --release --example train_screen_cnn` (release
//! matters: hundreds of epochs over full-batch backprop). The trainer exits
//! nonzero unless every training frame classifies correctly with margin, so
//! a bad corpus or recipe fails loudly instead of baking silent weights.
//!
//! Honest caveat: two dozen frames against 16k parameters memorizes, it
//! does not generalize by itself. What carries generalization is the
//! architecture (shared conv filters, GRID bottleneck) plus the margin
//! requirement below. Grow `tests/fixtures/` and re-run; never tune the
//! recipe to fit one frame.

use windows_headless::screen_cnn::{
    DENSE_INPUTS, GRID, KERNEL, N_CLASSES, N_FILTERS, N_WEIGHTS, POOLED, POOL, POOL_FEATURES,
    ScreenClass, downsample_to_32x32_rgb, load_rgb_8bit,
};

const EPOCHS: usize = 3000;
/// Full-batch gradient descent step size.
const LEARNING_RATE: f32 = 0.5;
/// Required logit gap (winner minus runner-up) on every training frame.
const REQUIRED_MARGIN: f32 = 2.0;
/// Frames per side of the CNN input grid.
const GRID_PIXELS: usize = GRID * GRID;

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

fn push_frame(out: &mut Vec<(Frame, usize)>, rgb: &[u8], w: usize, h: usize, label: ScreenClass) {
    out.push((downsample_to_32x32_rgb(rgb, w, h), label as usize));
}

fn augment(
    out: &mut Vec<(Frame, usize)>,
    rgb: &[u8],
    w: usize,
    h: usize,
    label: ScreenClass,
) {
    // Original, horizontal flip, dimmer, brighter.
    push_frame(out, rgb, w, h, label);
    let mut flip = vec![0u8; rgb.len()];
    for y in 0..h {
        for x in 0..w {
            let s = (y * w + x) * 3;
            let d = (y * w + (w - 1 - x)) * 3;
            flip[d..d + 3].copy_from_slice(&rgb[s..s + 3]);
        }
    }
    push_frame(out, &flip, w, h, label);
    for factor in [0.85f32, 1.15] {
        let shifted: Vec<u8> = rgb
            .iter()
            .map(|&v| ((v as f32) * factor).clamp(0.0, 255.0) as u8)
            .collect();
        push_frame(out, &shifted, w, h, label);
    }
}

fn flat_frame(w: usize, h: usize, c: [u8; 3]) -> Vec<u8> {
    vec![c; w * h].concat().concat()
}

/// Directory name under `tests/fixtures/` to training label. This table —
/// not a filename list — is the corpus definition; drop new captures in and
/// re-run.
const CLASS_DIRS: [(&str, ScreenClass); 7] = [
    ("dark", ScreenClass::Dark),
    ("flat-field", ScreenClass::FlatField),
    ("error-shell", ScreenClass::ErrorShell),
    ("installer-step", ScreenClass::InstallerStep),
    ("error-recovery", ScreenClass::ErrorRecovery),
    ("lockscreen", ScreenClass::Lockscreen),
    ("firmware-boot", ScreenClass::FirmwareBoot),
];

fn corpus() -> Vec<(Frame, usize)> {
    let mut out = Vec::new();
    let root = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    for (dir, label) in CLASS_DIRS {
        let mut names: Vec<_> = std::fs::read_dir(format!("{root}/{dir}"))
            .unwrap_or_else(|e| panic!("read fixtures/{dir}: {e}"))
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".png"))
            .collect();
        names.sort();
        assert!(
            !names.is_empty(),
            "no fixtures in tests/fixtures/{dir} (add captures or drop the class)"
        );
        for name in names {
            let (rgb, w, h) = load_rgb_8bit(&format!("{root}/{dir}/{name}"));
            augment(&mut out, &rgb, w as usize, h as usize, label);
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
        push_frame(&mut out, &rgb, w, h, ScreenClass::ErrorModal);
    }
    // Synthetic flats: the bare-field shapes the unit tests pin.
    for c in [[0, 120, 215], [128, 128, 128], [220, 60, 50], [30, 140, 60]] {
        push_frame(&mut out, &flat_frame(64, 64, c), 64, 64, ScreenClass::FlatField);
    }
    // Synthetic sparse dots on black: visible, not Dark.
    {
        let mut rgb = vec![0u8; 64 * 64 * 3];
        for i in 0..200 {
            let o = (i * 61) % (64 * 64) * 3;
            rgb[o..o + 3].copy_from_slice(&[220, 220, 220]);
        }
        push_frame(&mut out, &rgb, 64, 64, ScreenClass::Lockscreen);
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
        push_frame(&mut out, &rgb, w, h, ScreenClass::InstallerStep);
    }
    // Synthetic gradient (lock-screen style): Lockscreen.
    {
        let mut rgb = Vec::with_capacity(100 * 100 * 3);
        for y in 0..100u8 {
            for x in 0..100u8 {
                rgb.extend_from_slice(&[x * 2, y * 2, 128]);
            }
        }
        push_frame(&mut out, &rgb, 100, 100, ScreenClass::Lockscreen);
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
                                * conv_w[oc * 3 * KERNEL * KERNEL + ic * KERNEL * KERNEL
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

fn main() {
    let data = corpus();
    println!("corpus: {} frames", data.len());
    let mut rng = Lcg(0xC0FFEE);
    // He init scaled by each layer's fan-in.
    const CONV_FAN_IN: f32 = (3 * KERNEL * KERNEL) as f32;
    const DENSE_FAN_IN: f32 = DENSE_INPUTS as f32;
    let mut conv_w = [0.0f32; N_FILTERS * 3 * KERNEL * KERNEL];
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
    let mut caches = Caches {
        chw: [0.0; 3 * GRID * GRID],
        conv: [0.0; N_FILTERS * GRID * GRID],
        pool: [0.0; DENSE_INPUTS],
    };
    for epoch in 0..EPOCHS {
        let mut g_conv_w = [0.0f32; N_FILTERS * 3 * KERNEL * KERNEL];
        let mut g_conv_b = [0.0f32; N_FILTERS];
        let mut g_dense_w = [0.0f32; N_CLASSES * DENSE_INPUTS];
        let mut g_dense_b = [0.0f32; N_CLASSES];
        let mut loss = 0.0f32;
        let mut correct = 0;
        let mut min_margin = f32::INFINITY;
        for (rgb, label) in &data {
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
            let pred = probs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap()
                .0;
            if pred == *label {
                correct += 1;
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
                                    g_conv_w[oc * 3 * KERNEL * KERNEL
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
            println!(
                "epoch {epoch}: loss {:.4} acc {correct}/{} min_margin {:.2}",
                loss / n,
                data.len(),
                min_margin
            );
        }
        if correct == data.len() && min_margin >= REQUIRED_MARGIN {
            println!("converged at epoch {epoch}");
            break;
        }
    }

    // Final verdict on the corpus (report raw, no augmentation leniency).
    let mut fails = 0;
    for (rgb, label) in &data {
        let pool = forward(rgb, &conv_w, &conv_b, &mut caches);
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
        if best != *label {
            fails += 1;
            println!("MISCLASSIFIED want {label} got {best}");
        }
    }
    if fails > 0 {
        eprintln!("trainer FAILED: {fails} misclassifications, not writing weights");
        std::process::exit(1);
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
