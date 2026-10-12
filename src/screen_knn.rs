//! k-NN screen classifier: normalized-pixel embeddings + cosine similarity.
//!
//! The classifier for the 9 [`ScreenClass`] labels. No weights, no epochs,
//! no training loop — the corpus is clustered per class and inference is
//! cosine distance against the cluster centers.
//!
//! Pipeline per frame (all deterministic, bit-for-bit):
//! 1. [`downsample_to_grid_rgb`] (brightest-pixel, so
//!    thin text/spinners survive the shrink).
//! 2. Luminance, then a 3x3 box blur: damps single-pixel jitter
//!    (spinner phase, glyph placement) that exact-pixel matching would
//!    otherwise memorize.
//! 3. Zero-mean + unit-norm: brightness/exposure cannot dominate the
//!    distance. A pure-black frame has norm zero and short-circuits to
//!    `Dark` — nothing captured is a cosine over zeros.
//!
//! Verdict shape: top-1 + gap + top-k, where the
//! gap is best-cosine minus best-cosine-of-any-other-class, and
//! [`KNN_UNCERTAIN_GAP`] hedges narration — wording only, never gating. Every verdict names the matched
//! exemplar, so a miss is a viewable cause ("matched corpus frame X at
//! 0.992, runner-up Y at 0.991"), not a weight matrix to interrogate.
//!
//! Known limit, shared with every pixel-space method: near-duplicate
//! frames with different labels (console twins, `-moved` windows) still
//! collide. The miss list names its causes; when the pixels cannot
//! separate two labels, that is a taxonomy problem, not a model problem.

/// Shared resampling front end: every frame is
/// resampled to this square before embedding, so any source resolution
/// feeds the same descriptor. Motion sensing ([`crate::vision`]) reuses
/// the same grid + downsampler — one resampling contract, not two.
pub const GRID: usize = 128;

/// Number of screen classes.
pub const N_CLASSES: usize = 9;

/// Screen classes, in index order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenClass {
    /// Nothing on screen (pure black).
    Dark,
    /// Bare color field, no UI.
    FlatField,
    /// UEFI shell prompt (no bootable device).
    ErrorShell,
    /// Windows Setup / installer / OOBE progress screens.
    InstallerStep,
    /// Windows Recovery screens (WinRE menus and failure text).
    ErrorRecovery,
    /// Error UI needing input: modal failure dialogs and bugcheck screens.
    /// One class on purpose — the operational response is identical, and
    /// neither has enough captures to stand alone.
    ErrorScreen,
    /// Anything else: lock screen, gradients. (No desktop ever appears in
    /// this flow, so the lock screen stands in as the class name for the
    /// remaining miscellaneous bucket.)
    Lockscreen,
    /// Firmware boot art: TianoCore splash, boot logos. Transient screens
    /// between firmware handoff and the OS, visually distinct from both
    /// text consoles and OS screens.
    FirmwareBoot,
    /// First-boot OOBE transitionals ("Just a moment", logo splash,
    /// "Welcome"): post-install, pre-logon. Progress, never errors.
    OobeStep,
}

/// Index to class. The single source of truth for index order: predictions
/// return through this table, and the roundtrip test below fails the build
/// if the enum order ever drifts from it again.
pub const BY_INDEX: [ScreenClass; N_CLASSES] = [
    ScreenClass::Dark,
    ScreenClass::FlatField,
    ScreenClass::ErrorShell,
    ScreenClass::InstallerStep,
    ScreenClass::ErrorRecovery,
    ScreenClass::ErrorScreen,
    ScreenClass::Lockscreen,
    ScreenClass::FirmwareBoot,
    ScreenClass::OobeStep,
];

/// Brightest-pixel downsample of a source region to GRID x GRID.
/// Full-frame [`downsample_to_grid_rgb`] is this over the whole frame.
/// Each output pixel takes the brightest source pixel in its cell (per
/// channel) rather than the nearest neighbor: thin white text and
/// spinner arcs on black survive the shrink instead of sampling out
/// of existence.
pub fn downsample_region_to_grid_rgb(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> [u8; 3 * GRID * GRID] {
    assert_eq!(src.len(), src_w * src_h * 3, "downsample size mismatch");
    assert!(
        x0 < x1 && x1 <= src_w && y0 < y1 && y1 <= src_h,
        "crop region out of bounds"
    );
    let (rw, rh) = (x1 - x0, y1 - y0);
    let mut out = [0u8; 3 * GRID * GRID];
    for y in 0..GRID {
        let sy0 = y0 + y * rh / GRID;
        let sy1 = (y0 + (y + 1) * rh / GRID).max(sy0 + 1).min(y1);
        for x in 0..GRID {
            let sx0 = x0 + x * rw / GRID;
            let sx1 = (x0 + (x + 1) * rw / GRID).max(sx0 + 1).min(x1);
            let mut best = [0u8; 3];
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let s = (sy * src_w + sx) * 3;
                    for c in 0..3 {
                        if src[s + c] > best[c] {
                            best[c] = src[s + c];
                        }
                    }
                }
            }
            let d = (y * GRID + x) * 3;
            out[d..d + 3].copy_from_slice(&best);
        }
    }
    out
}

/// Brightest-pixel downsample of full-resolution RGB to GRID x GRID.
/// `src` is row-major `src_w` x `src_h` RGB; resolution-independent, so any
/// screendump size feeds the same descriptor.
pub fn downsample_to_grid_rgb(src: &[u8], src_w: usize, src_h: usize) -> [u8; 3 * GRID * GRID] {
    downsample_region_to_grid_rgb(src, src_w, src_h, 0, 0, src_w, src_h)
}

/// Decode a PNG file to 8-bit RGB bytes. Panicking wrapper for fixtures
/// and tests (every test input is committed, so a decode failure is a
/// repo problem, not a runtime one); user-supplied paths go through
/// [`try_load_rgb_8bit`], which returns `Err` instead.
pub fn load_rgb_8bit(path: &str) -> (Vec<u8>, u32, u32) {
    try_load_rgb_8bit(path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

/// Fallible sibling of [`load_rgb_8bit`] for callers that face
/// user-supplied paths (CLI): missing files and corrupt headers come
/// back as `Err`, never a panic. `String` error keeps this module
/// std-only; the CLI maps it into `anyhow`.
pub fn try_load_rgb_8bit(path: &str) -> Result<(Vec<u8>, u32, u32), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("read header: {e}"))?;
    let size = reader
        .output_buffer_size()
        .ok_or("frame too large".to_string())?;
    let mut buf = vec![0; size];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("decode: {e}"))?;
    if frame.bit_depth != png::BitDepth::Eight {
        return Err("not 8-bit".to_string());
    }
    let (w, h) = (frame.width, frame.height);
    let px = &buf[..frame.buffer_size()];
    let rgb = match frame.color_type {
        png::ColorType::Rgb => px.to_vec(),
        png::ColorType::Rgba => px
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => px
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0]])
            .collect(),
        png::ColorType::Indexed => {
            let palette = reader
                .info()
                .palette
                .clone()
                .expect("indexed fixture has no palette");
            px.iter()
                .flat_map(|&i| {
                    let o = i as usize * 3;
                    [palette[o], palette[o + 1], palette[o + 2]]
                })
                .collect()
        }
    };
    Ok((rgb, w, h))
}

/// Embedding width: one luminance float per GRID cell, plus the
/// appended DC (spatial mean) dimension.
pub const EMBED_DIMS: usize = GRID * GRID + 1;

/// Provisional hedge threshold on the cosine gap (best minus best of any
/// other class). Cosine gaps on near-duplicates live in the thousandths —
/// tune this against the holdout gap distribution, never by gut feel.
pub const KNN_UNCERTAIN_GAP: f32 = 0.01;

/// How many ranked exemplars a verdict prints.
pub const KNN_TOP_K: usize = 3;

/// Corpus dir→label map. This table — not a filename list — is the
/// corpus definition: drop new captures in, rebuild the index, re-run.
/// The version directory itself carries no label: the classifier never
/// sees it, it only keeps captures attributable over time.
pub const CLASS_DIRS: [(&str, ScreenClass); 10] = [
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

/// Rec.601 luminance of a GRID RGB frame, in 0.0..=1.0.
pub fn luminance(full: &[u8; 3 * GRID * GRID]) -> [f32; GRID * GRID] {
    let mut out = [0.0f32; GRID * GRID];
    for i in 0..GRID * GRID {
        let r = f32::from(full[i * 3]);
        let g = f32::from(full[i * 3 + 1]);
        let b = f32::from(full[i * 3 + 2]);
        out[i] = (0.299 * r + 0.587 * g + 0.114 * b) / 255.0;
    }
    out
}

/// 3x3 box blur with clamped edges: single-pixel content (spinner arc
/// phase, glyph edges) contributes to its neighborhood instead of living
/// or dying on one cell. Constant frames pass through unchanged.
pub fn box_blur(lum: &[f32; GRID * GRID]) -> [f32; GRID * GRID] {
    let mut out = [0.0f32; GRID * GRID];
    for y in 0..GRID {
        for x in 0..GRID {
            let mut sum = 0.0f32;
            let mut n = 0.0f32;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let xx = (x as i32 + dx).clamp(0, GRID as i32 - 1) as usize;
                    let yy = (y as i32 + dy).clamp(0, GRID as i32 - 1) as usize;
                    sum += lum[yy * GRID + xx];
                    n += 1.0;
                }
            }
            out[y * GRID + x] = sum / n;
        }
    }
    out
}

/// Zero-mean + unit-norm, with the spatial mean appended as a final
/// dimension before scaling. The appended DC preserves absolute
/// brightness: a uniform frame's embedding is the pure DC direction
/// instead of the zero vector, so flat bright fields (setup blue) and
/// flat black (no signal) separate by construction rather than
/// collapsing onto each other. Only the exactly-zero vector (uniform
/// black: no variance AND no light) scores as nothing — that is the
/// sole case callers short-circuit to `Dark`.
/// Output length is [`EMBED_DIMS`] = GRID*GRID + 1.
pub fn normalize(lum: &[f32; GRID * GRID]) -> Vec<f32> {
    let mean = lum.iter().sum::<f32>() / (GRID * GRID) as f32;
    let mut centered: Vec<f32> = lum.iter().map(|v| v - mean).collect();
    centered.push(mean);
    let norm = centered.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 {
        return vec![0.0; EMBED_DIMS];
    }
    centered.iter().map(|v| v / norm).collect()
}

/// Full front end: GRID RGB → blurred unit-norm luminance embedding.
pub fn embed(full: &[u8; 3 * GRID * GRID]) -> Vec<f32> {
    normalize(&box_blur(&luminance(full)))
}

/// Cosine similarity. Both sides are unit-norm from [`normalize`], so
/// this is a dot product; the denominator is skipped, never assumed —
/// a zero vector scores 0.0 against everything instead of NaN.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), EMBED_DIMS);
    debug_assert_eq!(b.len(), EMBED_DIMS);
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// One stored corpus frame: label, display name, embedding.
#[derive(Clone)]
pub struct Exemplar {
    pub label: usize,
    pub name: String,
    pub vec: Vec<f32>,
}

/// Shipped exemplar artifact (vectors + labels + names, no pixels):
/// committed under `artifacts/`, baked into the binary via
/// `include_bytes!` so installed binaries classify with no fixture
/// checkout. Rebuilt by `src/bin/build-knn-index.rs` — corpus edits
/// re-run the builder, exactly like retrains re-ship weights.
pub const KNN_INDEX_REL_PATH: &str = "artifacts/knn_index.bin";

/// Baked exemplar bytes: present at compile time (the builder runs
/// before any dependents compile), absent from every runtime path —
/// installed binaries carry the index, never the fixtures.
static KNN_INDEX_BYTES: &[u8] = include_bytes!("../artifacts/knn_index.bin");

/// Decode the baked artifact, fail-hard on stale/corrupt bytes (same
/// contract as the weights load: never score against a misaligned
/// index).
pub fn bundled_index() -> KnnIndex {
    KnnIndex {
        exemplars: decode_index(KNN_INDEX_BYTES),
    }
}

/// Process-shared index for the live classify tick: decoded once
/// (7 MB of f32 at runtime from a 290 KB artifact), then borrowed by
/// every frame. A per-tick decode would re-pay the dequantization on
/// every 10s cadence for no reason.
pub(crate) fn shared_index() -> &'static KnnIndex {
    static INDEX: std::sync::OnceLock<KnnIndex> = std::sync::OnceLock::new();
    INDEX.get_or_init(bundled_index)
}

/// Magic + version of the index encoding below (`KNN1`, 3: int8
/// payloads with per-vector scale, DC-appended dims).
const KNN_INDEX_MAGIC: u32 = 0x4b_4e_4e_31;
const KNN_INDEX_VERSION: u32 = 3;

/// Per-vector scale for [`quantize`]: reconstructed component =
/// `byte as f32 * scale`. Stored beside each exemplar (4 bytes); keeps
/// int8 steps proportional to the vector's own range instead of a
/// global guess.
pub fn quantize(v: &[f32]) -> (Vec<i8>, f32) {
    debug_assert_eq!(v.len(), EMBED_DIMS);
    let peak = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if peak == 0.0 {
        return (vec![0; EMBED_DIMS], 1.0);
    }
    let scale = peak / 127.0;
    let q = v
        .iter()
        .map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8)
        .collect();
    (q, scale)
}

/// Inverse of [`quantize`]: int8 payload back to the f32 the query path
/// scores. Quantization noise is the only accuracy delta vs raw floats —
/// measured by the holdout test, never assumed.
pub fn dequantize(q: &[i8], scale: f32) -> Vec<f32> {
    debug_assert_eq!(q.len(), EMBED_DIMS);
    q.iter().map(|b| f32::from(*b) * scale).collect()
}

/// Cluster count for a class with `n` exemplars: one center per ~8
/// frames, min 1, max 6. The artifact stores neighborhoods, not frames
/// — kilobytes per class whether the class holds 2 captures or 200.
pub fn clusters_for(n: usize) -> usize {
    n.div_ceil(8).clamp(1, 6)
}

/// Deterministic k-means (fixed-seed LCG k-means++ init, 50 Lloyd
/// iterations, ties to the lowest index): same points, same centers,
/// every run. Squared-euclidean for assignment (standard; on unit-norm
/// vectors it orders like cosine for close neighbors), means as
/// centers. `k` clamps to `1..=points.len()` — a singleton class
/// centers on itself.
pub fn kmeans(points: &[Vec<f32>], k: usize) -> Vec<Vec<f32>> {
    assert!(!points.is_empty(), "kmeans on empty class");
    let k = k.clamp(1, points.len());
    let dim = points[0].len();
    // Fixed-seed LCG: deterministic k-means++ without a rng crate.
    let mut state: u64 = 0xC0FFEE;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as f64 / (u32::MAX as f64 + 1.0)
    };
    let dist2 = |a: &[f32], b: &[f32]| {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| {
                let d = f64::from(*x) - f64::from(*y);
                d * d
            })
            .sum::<f64>()
    };
    let mut centers: Vec<Vec<f32>> = vec![points[0].clone()];
    while centers.len() < k {
        let d2: Vec<f64> = points
            .iter()
            .map(|p| {
                centers
                    .iter()
                    .map(|c| dist2(p, c))
                    .fold(f64::INFINITY, f64::min)
            })
            .collect();
        let total: f64 = d2.iter().sum();
        if total == 0.0 {
            // Remaining points sit on existing centers: duplicate the
            // last one (harmless — it retrieves identically).
            while centers.len() < k {
                centers.push(centers.last().unwrap().clone());
            }
            break;
        }
        let mut pick = next() * total;
        let mut idx = 0;
        for (i, d) in d2.iter().enumerate() {
            pick -= d;
            if pick <= 0.0 {
                idx = i;
                break;
            }
            idx = i;
        }
        centers.push(points[idx].clone());
    }
    for _ in 0..50 {
        let mut sums = vec![vec![0.0f64; dim]; k];
        let mut counts = vec![0u64; k];
        for p in points {
            let mut best = 0;
            let mut best_d = f64::INFINITY;
            for (i, c) in centers.iter().enumerate() {
                let d = dist2(p, c);
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            for (s, v) in sums[best].iter_mut().zip(p.iter()) {
                *s += f64::from(*v);
            }
            counts[best] += 1;
        }
        for (i, c) in centers.iter_mut().enumerate() {
            if counts[i] == 0 {
                continue;
            }
            for (v, s) in c.iter_mut().zip(sums[i].iter()) {
                *v = (s / counts[i] as f64) as f32;
            }
        }
    }
    // Unit-norm restoration: the mean of unit vectors has length < 1 for
    // dispersed clusters, and the query path scores cosine as a plain dot
    // product — an unnormalized center would read as artificially distant.
    // A zero mean (members cancelling exactly) falls back to the first
    // member rather than scoring 0.0 against everything.
    for c in centers.iter_mut() {
        let norm = c.iter().map(|v| *v * *v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in c.iter_mut() {
                *v /= norm;
            }
        } else if let Some(first) = points.first() {
            *c = first.clone();
        }
    }
    centers
}

/// Encode exemplars to little-endian bytes: 16-byte header (magic,
/// version, dims, count), then per exemplar label u32, scale f32,
/// name-len u32, name bytes, dims int8 payload. int8 + scale is one
/// byte per dim (4x under raw f32): artifact growth tracks dimensions
/// and centers, never raw frame count. The single writer behind every
/// index producer; [`decode_index`] is the single reader.
pub fn encode_index(exemplars: &[Exemplar]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&KNN_INDEX_MAGIC.to_le_bytes());
    out.extend_from_slice(&KNN_INDEX_VERSION.to_le_bytes());
    out.extend_from_slice(&(EMBED_DIMS as u32).to_le_bytes());
    out.extend_from_slice(&(exemplars.len() as u32).to_le_bytes());
    for e in exemplars {
        assert_eq!(e.vec.len(), EMBED_DIMS, "exemplar {} dims drift", e.name);
        let (q, scale) = quantize(&e.vec);
        out.extend_from_slice(&(e.label as u32).to_le_bytes());
        out.extend_from_slice(&scale.to_le_bytes());
        out.extend_from_slice(&(e.name.len() as u32).to_le_bytes());
        out.extend_from_slice(e.name.as_bytes());
        // i8→u8 `as` is a bit-preserving reinterpret, never a conversion.
        out.extend(q.iter().map(|b| *b as u8));
    }
    out
}

/// Decode [`encode_index`] output, fail-hard like `decode_weights_le`:
/// wrong magic/version/dims or trailing/truncated bytes panic — a stale
/// or corrupt artifact must never silently score.
pub fn decode_index(bytes: &[u8]) -> Vec<Exemplar> {
    fn take<'a>(bytes: &'a [u8], off: &mut usize, n: usize) -> &'a [u8] {
        let s = *off;
        // Overflow saturates to a range `get` shrinks to `None`: any
        // truncation or wraparound lands in the same loud panic.
        let e = s.saturating_add(n);
        let slice = bytes
            .get(s..e)
            .filter(|sl| sl.len() == n)
            .unwrap_or_else(|| panic!("{KNN_INDEX_REL_PATH} corrupt: truncated at byte {s}"));
        *off = s + slice.len();
        slice
    }
    let mut off = 0usize;
    let u32le = |s: &[u8]| u32::from_le_bytes(s.try_into().unwrap());
    assert!(
        bytes.len() >= 16,
        "{KNN_INDEX_REL_PATH} corrupt: truncated header"
    );
    let magic = u32le(take(bytes, &mut off, 4));
    let version = u32le(take(bytes, &mut off, 4));
    let dims = u32le(take(bytes, &mut off, 4));
    let count = u32le(take(bytes, &mut off, 4));
    assert_eq!(
        magic, KNN_INDEX_MAGIC,
        "{KNN_INDEX_REL_PATH} corrupt: bad magic"
    );
    assert_eq!(
        version, KNN_INDEX_VERSION,
        "{KNN_INDEX_REL_PATH} stale: want version {KNN_INDEX_VERSION}, have {version}"
    );
    assert_eq!(
        dims as usize, EMBED_DIMS,
        "{KNN_INDEX_REL_PATH} stale: want dims {EMBED_DIMS}, have {dims}"
    );
    let mut exemplars = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let label = u32le(take(bytes, &mut off, 4)) as usize;
        let scale = f32::from_le_bytes(take(bytes, &mut off, 4).try_into().unwrap());
        let name_len = u32le(take(bytes, &mut off, 4)) as usize;
        let name = take(bytes, &mut off, name_len);
        let name = std::str::from_utf8(name)
            .unwrap_or_else(|_| panic!("{KNN_INDEX_REL_PATH} corrupt: bad name bytes"))
            .to_string();
        let raw = take(bytes, &mut off, EMBED_DIMS);
        let q: Vec<i8> = raw.iter().map(|b| *b as i8).collect();
        exemplars.push(Exemplar {
            label,
            name,
            vec: dequantize(&q, scale),
        });
    }
    assert_eq!(
        off,
        bytes.len(),
        "{KNN_INDEX_REL_PATH} corrupt: {} trailing bytes",
        bytes.len() - off
    );
    exemplars
}

/// The clustered corpus in memory: one center per class neighborhood
/// (see [`clusters_for`]), each EMBED_DIMS floats. Built once
/// per process (tests) or per CLI invocation — no cache file, no format
/// to version, re-derivable from the fixtures in seconds.
pub struct KnnIndex {
    pub exemplars: Vec<Exemplar>,
}

impl KnnIndex {
    /// Downsample + embed one decoded frame. Shared by the index build
    /// and by queries so both sides see identical pixels.
    pub fn embed_frame(rgb: &[u8], w: u32, h: u32) -> Vec<f32> {
        let full = downsample_to_grid_rgb(rgb, w as usize, h as usize);
        embed(&full)
    }

    /// Load every `*.png` under `<root>/<version>/<class-dir>/`.
    /// Deterministic order: versions, names, and dirs all sorted — miss
    /// lists diff cleanly across runs.
    pub fn build(corpus_root: &str) -> Self {
        let mut exemplars = Vec::new();
        let mut versions: Vec<_> = std::fs::read_dir(corpus_root)
            .unwrap_or_else(|e| panic!("read {corpus_root}: {e}"))
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        versions.sort();
        for version in versions {
            if !std::path::Path::new(&format!("{corpus_root}/{version}")).is_dir() {
                continue;
            }
            for (dir, class) in CLASS_DIRS {
                let dir_path = format!("{corpus_root}/{version}/{dir}");
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
                    exemplars.push(Exemplar {
                        label: class as usize,
                        name: format!("{version}/{dir}/{name}"),
                        vec: Self::embed_frame(&rgb, w, h),
                    });
                }
            }
        }
        Self { exemplars }
    }

    /// Per-class clustered index: one [`kmeans`] run per label
    /// ([`clusters_for`] centers), each center named after its nearest
    /// member so verdicts still name a viewable frame (`<frame>
    /// ~centroid`). The shipped shape: ~18 vectors for today's corpus
    /// instead of 118, and new captures mostly land inside existing
    /// neighborhoods — the artifact grows with variety, not volume.
    /// Deterministic: labels in [`BY_INDEX`] order, members in build
    /// order, fixed-seed clustering — the same corpus rebuilds the same
    /// bytes, which `bundled_index_matches_fixtures` enforces.
    pub fn clustered(corpus_root: &str) -> Self {
        let full = Self::build(corpus_root);
        let mut exemplars = Vec::new();
        for label in 0..BY_INDEX.len() {
            let members: Vec<&Exemplar> =
                full.exemplars.iter().filter(|e| e.label == label).collect();
            if members.is_empty() {
                continue;
            }
            let points: Vec<Vec<f32>> = members.iter().map(|e| e.vec.clone()).collect();
            for center in kmeans(&points, clusters_for(members.len())) {
                let nearest = members
                    .iter()
                    .min_by(|a, b| {
                        let da: f32 = a
                            .vec
                            .iter()
                            .zip(center.iter())
                            .map(|(x, y)| (x - y) * (x - y))
                            .sum();
                        let db: f32 = b
                            .vec
                            .iter()
                            .zip(center.iter())
                            .map(|(x, y)| (x - y) * (x - y))
                            .sum();
                        da.total_cmp(&db)
                    })
                    .unwrap()
                    .name
                    .clone();
                exemplars.push(Exemplar {
                    label,
                    name: format!("{nearest} ~centroid"),
                    vec: center,
                });
            }
        }
        Self { exemplars }
    }

    /// Ranked (label, cosine, exemplar name), best first. `skip` excludes
    /// one exemplar by position (leave-one-out without rebuilding).
    pub fn query(&self, q: &[f32], skip: Option<usize>, k: usize) -> Vec<(usize, f32, &str)> {
        let mut ranked: Vec<(usize, f32, &str)> = self
            .exemplars
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != skip)
            .map(|(_, e)| (e.label, cosine(q, &e.vec), e.name.as_str()))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(k);
        ranked
    }

    /// Top-1 class + gap + matched exemplar name. The gap is best cosine
    /// minus the best cosine of any *other* class (single-class index:
    /// the gap is the top cosine itself). Zero-vector queries (pure
    /// black) return `Dark` committed — the only hardcoded verdict.
    /// `None` only when the index (minus `skip`) is empty.
    pub fn predict(&self, q: &[f32], skip: Option<usize>) -> Option<(ScreenClass, f32, String)> {
        if q.iter().all(|v| *v == 0.0) {
            return Some((ScreenClass::Dark, 1.0, "zero-frame".to_string()));
        }
        let ranked = self.query(q, skip, self.exemplars.len());
        let (label, best, name) = ranked.first()?;
        let runner_up = ranked
            .iter()
            .find(|(l, _, _)| l != label)
            .map(|(_, c, _)| *c)
            .unwrap_or(0.0);
        let class = BY_INDEX.iter().find(|c| **c as usize == *label).copied()?;
        Some((class, best - runner_up, (*name).to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_frame() -> [u8; 3 * GRID * GRID] {
        let mut full = [0u8; 3 * GRID * GRID];
        for y in 0..GRID {
            for x in 0..GRID {
                let v = ((x + y) % 256) as u8;
                let i = (y * GRID + x) * 3;
                full[i..i + 3].copy_from_slice(&[v, v, v]);
            }
        }
        full
    }

    #[test]
    fn embed_is_unit_norm() {
        let e = embed(&gradient_frame());
        let norm = e.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm {norm}");
    }

    #[test]
    fn cosine_self_is_one() {
        let e = embed(&gradient_frame());
        assert!((cosine(&e, &e) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn blur_holds_constant() {
        // A flat field blurs to itself: no phantom structure invented.
        let flat = [0.4f32; GRID * GRID];
        let out = box_blur(&flat);
        assert!(out.iter().all(|v| (v - 0.4).abs() < 1e-6));
    }

    #[test]
    fn index_codec_roundtrips() {
        // encode→decode preserves labels/names exactly and floats to
        // within one quantization step: the shipped artifact scores what
        // the fixtures built, up to int8 noise.
        let exemplars = vec![
            Exemplar {
                label: 3,
                name: "v/dir/a.png".to_string(),
                vec: embed(&gradient_frame()),
            },
            Exemplar {
                label: 7,
                name: "v/dir/b.png".to_string(),
                vec: vec![0.0; EMBED_DIMS],
            },
        ];
        let back = decode_index(&encode_index(&exemplars));
        assert_eq!(back.len(), 2);
        for (a, b) in exemplars.iter().zip(back.iter()) {
            assert_eq!(a.label, b.label);
            assert_eq!(a.name, b.name);
            assert_eq!(a.vec.len(), b.vec.len());
            let peak = a.vec.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            let step = peak / 127.0;
            assert!(
                a.vec
                    .iter()
                    .zip(b.vec.iter())
                    .all(|(x, y)| (x - y).abs() <= step + 1e-6)
            );
        }
    }

    #[test]
    #[should_panic(expected = "bad magic")]
    fn index_decode_rejects_bad_magic() {
        let mut bytes = encode_index(&[]);
        bytes[0] ^= 0xff;
        let _ = decode_index(&bytes);
    }

    #[test]
    fn bundled_index_matches_fixtures() {
        // The baked artifact agrees with a fresh clustered build: same
        // count, same names, vectors within one quantization step —
        // commit drift between the artifact and the corpus fails here,
        // not in production.
        let root = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let fresh = KnnIndex::clustered(&root);
        let bundled = bundled_index();
        assert_eq!(bundled.exemplars.len(), fresh.exemplars.len());
        for (a, b) in bundled.exemplars.iter().zip(fresh.exemplars.iter()) {
            assert_eq!(a.label, b.label);
            assert_eq!(a.name, b.name, "corpus order drift vs artifact");
            let peak = b.vec.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            let step = peak / 127.0;
            assert!(
                a.vec
                    .iter()
                    .zip(b.vec.iter())
                    .all(|(x, y)| (x - y).abs() <= step + 1e-6)
            );
        }
    }

    #[test]
    fn all_black_predicts_dark() {
        let black = [0u8; 3 * GRID * GRID];
        let e = embed(&black);
        assert!(e.iter().all(|v| *v == 0.0));
        let index = KnnIndex { exemplars: vec![] };
        assert!(matches!(
            index.predict(&e, None),
            Some((ScreenClass::Dark, _, _))
        ));
    }

    #[test]
    fn knn_corpus_leave_one_out() {
        // Every corpus frame queries all *other* frames: the scoreboard
        // for a pure memorizer. Prints one line per miss (matched
        // exemplar named — the legibility win).
        // No accuracy gate: this test reports, the promotion decision
        // reads the report.
        let root = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let index = KnnIndex::build(&root);
        assert!(!index.exemplars.is_empty(), "no fixtures loaded");
        let mut hits = 0usize;
        for (i, e) in index.exemplars.iter().enumerate() {
            let (got, gap, matched) = index.predict(&e.vec, Some(i)).unwrap();
            let want = BY_INDEX
                .iter()
                .find(|c| **c as usize == e.label)
                .copied()
                .unwrap();
            if got == want {
                hits += 1;
            } else {
                println!(
                    "KNN LOO MISS {}: want {want:?} got {got:?} gap {gap:.4} via {matched}",
                    e.name
                );
            }
        }
        let n = index.exemplars.len();
        println!("KNN LOO {hits}/{n}");
    }

    #[test]
    fn knn_holdout_scores() {
        // Corpus-built index vs tests/fixtures-holdout/<class>/*.png:
        // unseen layouts — the class directory supplies the expected
        // label, no lists. Misses print the matched exemplar + gap, so
        // each one is triageable on sight.
        let root = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let index = KnnIndex::build(&root);
        let hold_root = format!("{}/tests/fixtures-holdout", env!("CARGO_MANIFEST_DIR"));
        let mut hits = 0usize;
        let mut total = 0usize;
        for (dir, want) in CLASS_DIRS {
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
                let q = KnnIndex::embed_frame(&rgb, w, h);
                let (got, gap, matched) = index.predict(&q, None).unwrap();
                total += 1;
                if got == want {
                    hits += 1;
                } else {
                    println!(
                        "KNN HOLDOUT MISS {dir}/{name}: want {want:?} got {got:?} gap {gap:.4} via {matched}"
                    );
                }
            }
        }
        println!("KNN HOLDOUT {hits}/{total}");
    }

    #[test]
    fn knn_holdout_quantized() {
        // The shipped path end to end: holdout frames vs the dequantized
        // baked artifact (int8 noise included). Any delta vs KNN HOLDOUT
        // above is the measured cost of 4x smaller shipping — report, no
        // gate.
        let index = bundled_index();
        let hold_root = format!("{}/tests/fixtures-holdout", env!("CARGO_MANIFEST_DIR"));
        let mut hits = 0usize;
        let mut total = 0usize;
        for (dir, want) in CLASS_DIRS {
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
                let q = KnnIndex::embed_frame(&rgb, w, h);
                let (got, gap, matched) = index.predict(&q, None).unwrap();
                total += 1;
                if got == want {
                    hits += 1;
                } else {
                    println!(
                        "KNN-Q HOLDOUT MISS {dir}/{name}: want {want:?} got {got:?} gap {gap:.4} via {matched}"
                    );
                }
            }
        }
        println!("KNN-Q HOLDOUT {hits}/{total}");
    }

    #[test]
    fn knn_holdout_clustered_float() {
        // Float-centroid reference: same clustered index as the artifact
        // but unquantized — separates clustering cost from quantization
        // cost. Compare against KNN HOLDOUT (full index) and KNN-Q
        // HOLDOUT (quantized, whatever ships).
        let root = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let index = KnnIndex::clustered(&root);
        println!("clustered index: {} centers", index.exemplars.len());
        let hold_root = format!("{}/tests/fixtures-holdout", env!("CARGO_MANIFEST_DIR"));
        let mut hits = 0usize;
        let mut total = 0usize;
        for (dir, want) in CLASS_DIRS {
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
                let q = KnnIndex::embed_frame(&rgb, w, h);
                let (got, gap, matched) = index.predict(&q, None).unwrap();
                total += 1;
                if got == want {
                    hits += 1;
                } else {
                    println!(
                        "KNN-C HOLDOUT MISS {dir}/{name}: want {want:?} got {got:?} gap {gap:.4} via {matched}"
                    );
                }
            }
        }
        println!("KNN-C HOLDOUT {hits}/{total}");
    }
}
