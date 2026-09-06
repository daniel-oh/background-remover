//! The command-line mode, driven as a real process. Needs MODEL_PATH for the
//! cases that run the model; the usage cases run everywhere.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_background-remover");

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata")).join(name)
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("br-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn model() -> Option<String> {
    let p = std::env::var("MODEL_PATH").ok()?;
    Some(p)
}

#[test]
fn usage_errors_exit_2_and_explain() {
    let out = Command::new(BIN).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown option --bogus"), "{err}");
    assert!(err.contains("Usage:"), "{err}");
}

#[test]
fn version_and_help_print() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("background-remover "));
    let out = Command::new(BIN).arg("--help").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("--fetch-model"));
}

#[test]
fn a_missing_model_is_a_clear_error_when_downloads_are_off() {
    let dir = scratch("nomodel");
    let out = Command::new(BIN)
        .args(["--no-download", "--model"])
        .arg(dir.join("absent.onnx"))
        .arg(testdata("sample.jpg"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no model at"), "{err}");
    assert!(err.contains("--fetch-model"), "{err}");
}

#[test]
fn a_file_becomes_a_cutout_beside_it_or_where_asked() {
    let Some(model) = model() else {
        eprintln!("cli: MODEL_PATH is not set, skipping");
        return;
    };
    let dir = scratch("file");
    let input = dir.join("photo.jpg");
    std::fs::copy(testdata("sample.jpg"), &input).unwrap();

    // Default name beside the input.
    let out = Command::new(BIN)
        .env("MODEL_PATH", &model)
        .arg(&input)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let png = std::fs::read(dir.join("photo-cutout.png")).unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    let img = image::load_from_memory(&png).unwrap().to_rgba8();
    let reference = image::load_from_memory(&std::fs::read(testdata("reference.png")).unwrap())
        .unwrap()
        .to_rgba8();
    assert_eq!(img.dimensions(), reference.dimensions());
    let max_alpha = img
        .pixels()
        .zip(reference.pixels())
        .map(|(a, b)| (a[3] as i32 - b[3] as i32).unsigned_abs())
        .max()
        .unwrap();
    assert!(
        max_alpha <= 2,
        "alpha differs from the reference by {max_alpha}"
    );

    // Explicit output, format from the extension; and the mask; and a directory.
    let out = Command::new(BIN)
        .env("MODEL_PATH", &model)
        .args(["-q", "-o"])
        .arg(dir.join("x.webp"))
        .arg(&input)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty(), "quiet means quiet");
    assert!(std::fs::read(dir.join("x.webp"))
        .unwrap()
        .starts_with(b"RIFF"));

    let out = Command::new(BIN)
        .env("MODEL_PATH", &model)
        .args(["--mask", "-d"])
        .arg(dir.join("masks"))
        .arg(&input)
        .arg(testdata("sample.png"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mask =
        image::load_from_memory(&std::fs::read(dir.join("masks/photo-mask.png")).unwrap()).unwrap();
    assert_eq!(mask.color(), image::ColorType::L8);
    assert!(dir.join("masks/sample-mask.png").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stdin_to_stdout_and_a_bad_file_among_good_ones() {
    let Some(model) = model() else {
        return;
    };
    let mut child = Command::new(BIN)
        .env("MODEL_PATH", &model)
        .args(["-q", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&std::fs::read(testdata("sample.jpg")).unwrap())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.starts_with(b"\x89PNG"));

    let dir = scratch("mixed");
    std::fs::write(dir.join("junk.jpg"), b"\xFF\xD8\xFF\xE0not a jpeg at all").unwrap();
    let out = Command::new(BIN)
        .env("MODEL_PATH", &model)
        .args(["-q", "-d"])
        .arg(&dir)
        .arg(dir.join("junk.jpg"))
        .arg(dir.join("missing.jpg"))
        .arg(testdata("sample.jpg"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "one failure fails the run");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("junk.jpg"), "{err}");
    assert!(err.contains("missing.jpg"), "{err}");
    assert!(
        dir.join("sample-cutout.png").exists(),
        "the good one still ran"
    );
    let _ = std::fs::remove_dir_all(dir);
}
