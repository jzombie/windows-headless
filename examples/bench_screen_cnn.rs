//! Dev-time inference benchmark: per-frame cost of the full classify path
//! (downsample + forward pass) on a real screendump, release vs debug.
//! Usage: `cargo run --release --example bench_screen_cnn [png-path]`.

use windows_headless::screen_cnn::{load_rgb_8bit, predict_full};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        format!(
            "{}/tests/fixtures/win11-26300-en-us-retail/installer-step/installer-progress.png",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    let (rgb, w, _) = load_rgb_8bit(&path);
    // Warmup (weights decode + allocator paths outside the timed region).
    let _ = predict_full(&rgb, w);
    let iters = 50;
    let start = std::time::Instant::now();
    for _ in 0..iters {
        std::hint::black_box(predict_full(&rgb, w));
    }
    let per = start.elapsed() / iters;
    println!("predict_full ({}x{}): {per:?} per frame", w, rgb.len() / 3 / w as usize);
}
