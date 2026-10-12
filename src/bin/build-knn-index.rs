//! Build the shipped k-NN exemplar artifact: cluster every corpus frame
//! per class and write `artifacts/knn_index.bin` (centroid vectors +
//! labels + names, no pixels). Re-run after corpus edits; the file is
//! committed and baked into the binary via `include_bytes!`, so
//! installed binaries need no fixture checkout.
//!
//! Run: `cargo run --release --bin build-knn-index [--full]`
//! (`--full` stores every frame's vector instead of cluster centers:
//! bigger artifact, exact retrieval — the escape hatch if clustering
//! ever costs accuracy that matters.)

use windows_headless::screen_knn::{KNN_INDEX_REL_PATH, KnnIndex, encode_index};

fn main() {
    let root = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let full = std::env::args().any(|a| a == "--full");
    let index = if full {
        KnnIndex::build(&root)
    } else {
        KnnIndex::clustered(&root)
    };
    assert!(
        !index.exemplars.is_empty(),
        "no fixtures under tests/fixtures"
    );
    let bytes = encode_index(&index.exemplars);
    let out_path = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), KNN_INDEX_REL_PATH);
    if let Some(parent) = std::path::Path::new(&out_path).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&out_path, &bytes).unwrap();
    println!(
        "knn index: {} exemplars -> {out_path} ({} bytes)",
        index.exemplars.len(),
        bytes.len()
    );
}
