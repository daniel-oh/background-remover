//! Where the model lives when nobody said, and how it gets there.
//!
//! The command-line mode fetches the weights on first use into the user's
//! cache directory, verifies them, and keeps them. The server never
//! downloads anything: production weights are mounted and checksummed.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Where the isnet-general-use weights are published (the rembg release
/// that redistributes them). `MODEL_URL` overrides it.
pub const MODEL_URL: &str =
    "https://github.com/danielgatis/rembg/releases/download/v0.0.0/isnet-general-use.onnx";

/// File name of the model inside the cache directory.
pub const MODEL_FILE: &str = "isnet-general-use.onnx";

/// The per-user cache directory for this program: `~/Library/Caches` on
/// macOS, `%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` or `~/.cache`
/// elsewhere, each with a `background-remover` subdirectory.
pub fn cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library").join("Caches"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".cache")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("background-remover")
}

/// The model path used when `MODEL_PATH` is not set.
pub fn default_model_path() -> PathBuf {
    cache_dir().join(MODEL_FILE)
}

/// Download `url` to `dest`, verify it hashes to `sha256`, and only then put
/// it in place. Progress goes to `progress` as a fraction, when known.
pub fn fetch_model(
    url: &str,
    dest: &Path,
    sha256: &str,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let part = dest.with_extension("onnx.part");
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("cannot fetch {url}: {e}"))?;
    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut body = response.into_body().into_reader();
    let mut file =
        fs::File::create(&part).map_err(|e| format!("cannot write {}: {e}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        let n = body
            .read(&mut buf)
            .map_err(|e| format!("download interrupted after {done} bytes: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("cannot write {}: {e}", part.display()))?;
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done, total);
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    let got = hex::encode(hasher.finalize());
    if got != sha256 {
        let _ = fs::remove_file(&part);
        return Err(format!(
            "the download does not match the expected checksum\n  expected {sha256}\n  got      {got}"
        ));
    }
    fs::rename(&part, dest).map_err(|e| format!("cannot move into {}: {e}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    /// One-shot HTTP server on a random port that answers any GET with `body`.
    fn serve_once(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.onnx", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                line.clear();
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        url
    }

    #[test]
    fn a_good_download_lands_in_place_with_progress() {
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let sha = hex::encode(Sha256::digest(&body));
        let dir = std::env::temp_dir().join(format!("br-fetch-{}", std::process::id()));
        let dest = dir.join("nested").join("model.onnx");
        let url = serve_once(body.clone());
        let mut last = (0, None);
        fetch_model(&url, &dest, &sha, |d, t| last = (d, t)).unwrap();
        assert_eq!(last, (body.len() as u64, Some(body.len() as u64)));
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(!dest.with_extension("onnx.part").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_wrong_checksum_leaves_nothing_behind() {
        let body = b"not the model".to_vec();
        let dir = std::env::temp_dir().join(format!("br-fetch-bad-{}", std::process::id()));
        let dest = dir.join("model.onnx");
        let url = serve_once(body);
        let err = fetch_model(&url, &dest, &"0".repeat(64), |_, _| {}).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert!(!dest.exists());
        assert!(!dest.with_extension("onnx.part").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_cache_directory_is_under_the_home() {
        let dir = cache_dir();
        assert!(dir.ends_with("background-remover"), "{}", dir.display());
        assert_eq!(default_model_path().file_name().unwrap(), MODEL_FILE);
    }
}
