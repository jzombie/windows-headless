//! Temporary probe: print predicted class per fixture file. Delete after use.
use windows_headless::screen_cnn::{load_rgb_8bit, predict_full};

fn main() {
    let root = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let mut versions: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    versions.sort();
    for version in versions {
        let mut classes: Vec<_> = std::fs::read_dir(format!("{root}/{version}"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        classes.sort();
        for class in classes {
            let dir = format!("{root}/{version}/{class}");
            if !std::path::Path::new(&dir).is_dir() {
                continue;
            }
            let mut names: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".png"))
                .collect();
            names.sort();
            for name in names {
                let (rgb, w, _) = load_rgb_8bit(&format!("{dir}/{name}"));
                println!("{class}/{name} -> {:?}", predict_full(&rgb, w));
            }
        }
    }
}
