//! The golden test: a photo through this service must come out as it
//! comes out of the Python reference implementation (testdata/reference*.png,
//! produced by rembg from the same bytes). Runs only when MODEL_PATH is set.

use background_remover::config::Config;
use background_remover::imageops::cutout;
use background_remover::model::Model;

#[test]
fn matches_the_python_service() {
    let Ok(model_path) = std::env::var("MODEL_PATH") else {
        eprintln!("golden: MODEL_PATH is not set, skipping");
        return;
    };
    let cfg = Config {
        model_path,
        ..Config::from_env()
    };
    background_remover::model::verify_checksum(&cfg.model_path, &cfg.model_sha256)
        .expect("the model on disk is the expected one");
    let model = Model::new(cfg.clone());

    let fixture = std::env::var("GOLDEN").unwrap_or_else(|_| "png".into());
    let (sample_file, reference_file) = if fixture == "jpeg" {
        ("sample.jpg", "reference.png")
    } else {
        ("sample.png", "reference-png.png")
    };
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/");
    let sample = std::fs::read(format!("{dir}{sample_file}")).unwrap();
    let reference = std::fs::read(format!("{dir}{reference_file}")).unwrap();

    let started = std::time::Instant::now();
    let png = cutout(&sample, &model, false).expect("cutout runs");
    eprintln!(
        "golden: cutout took {:.2}s, {} bytes (reference {} bytes)",
        started.elapsed().as_secs_f32(),
        png.len(),
        reference.len()
    );

    let ours = image::load_from_memory(&png).unwrap().to_rgba8();
    let theirs = image::load_from_memory(&reference).unwrap().to_rgba8();
    assert_eq!(
        ours.dimensions(),
        theirs.dimensions(),
        "same size as the reference"
    );

    let mut max_alpha = 0u32;
    let mut sum_alpha = 0u64;
    let mut over_two = 0u64;
    let mut rgb_mismatch = 0u64;
    for (a, b) in ours.pixels().zip(theirs.pixels()) {
        if a[0] != b[0] || a[1] != b[1] || a[2] != b[2] {
            rgb_mismatch += 1;
        }
        let d = (a[3] as i32 - b[3] as i32).unsigned_abs();
        max_alpha = max_alpha.max(d);
        sum_alpha += d as u64;
        if d > 2 {
            over_two += 1;
        }
    }
    let pixels = (ours.width() * ours.height()) as u64;
    let mean_alpha = sum_alpha as f64 / pixels as f64;
    eprintln!("golden: rgb mismatches {rgb_mismatch}, alpha max {max_alpha}, mean {mean_alpha:.4}, over two {over_two} of {pixels}");

    assert_eq!(
        rgb_mismatch, 0,
        "the colour must be the original's, untouched"
    );
    assert!(
        mean_alpha < 0.1,
        "mean alpha difference {mean_alpha} is not under 0.1"
    );
    assert!(max_alpha <= 2, "max alpha difference {max_alpha} is over 2");
}

/// The same contract for BiRefNet at 768: testdata/sample.jpg through this
/// program must give the mask Python gives (Pillow's resize, ONNX Runtime,
/// numpy's sigmoid and truncation), testdata/reference-birefnet-lite-768-mask.png.
/// Runs only when BIREFNET_768_PATH points at birefnet-lite-768.onnx.
#[test]
fn birefnet_matches_the_python_reference() {
    let Ok(path) = std::env::var("BIREFNET_768_PATH") else {
        eprintln!("golden: BIREFNET_768_PATH is not set, skipping");
        return;
    };
    let known = background_remover::profile::known("birefnet-lite-768").unwrap();
    let cfg = Config {
        model_path: path,
        model_path_set: true,
        threads: 2,
        ..Config::from_env().with_model(known)
    };
    background_remover::model::verify_checksum(&cfg.model_path, &cfg.model_sha256)
        .expect("the model on disk is the published one");
    let model = Model::new(cfg);
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/");
    let sample = std::fs::read(format!("{dir}sample.jpg")).unwrap();
    let reference = std::fs::read(format!("{dir}reference-birefnet-lite-768-mask.png")).unwrap();

    let mut opts = background_remover::imageops::Options::default();
    opts.output = background_remover::imageops::Output::MaskPng;
    let started = std::time::Instant::now();
    let mask = background_remover::imageops::cutout_with(&sample, &model, &opts)
        .expect("cutout runs")
        .bytes;
    eprintln!(
        "golden: birefnet-lite-768 took {:.2}s",
        started.elapsed().as_secs_f32()
    );
    let ours = image::load_from_memory(&mask).unwrap().to_luma8();
    let theirs = image::load_from_memory(&reference).unwrap().to_luma8();
    assert_eq!(ours.dimensions(), theirs.dimensions());
    let (mut max, mut sum) = (0u32, 0u64);
    for (a, b) in ours.pixels().zip(theirs.pixels()) {
        let d = (a[0] as i32 - b[0] as i32).unsigned_abs();
        max = max.max(d);
        sum += d as u64;
    }
    let mean = sum as f64 / (ours.width() * ours.height()) as f64;
    eprintln!("golden: birefnet alpha max {max}, mean {mean:.4}");
    assert!(max <= 2, "max alpha difference {max} is over 2");
    assert!(mean < 0.1, "mean alpha difference {mean} is not under 0.1");
}
