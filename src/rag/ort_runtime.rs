//! On-demand ONNX Runtime for Linux local embeddings (`ort` load-dynamic).
//!
//! On Linux the `local-embeddings` build compiles `ort` in `load-dynamic`
//! mode, so the binary carries none of the ~40 MB ONNX Runtime and instead
//! `dlopen`s `libonnxruntime.so` at runtime via `ORT_DYLIB_PATH`. That
//! shared library is downloaded to `~/.canopy/ort/libonnxruntime.so` the
//! first time the user actually runs local RAG — never on a plain install.
//!
//! Version 1.24.2 is pinned to match what `ort-sys` 2.0.0-rc.12 links
//! against (its `build/download/dist.txt` resolves this target to
//! `ms@1.24.2`); a mismatched runtime would fail to load or misbehave at
//! inference time. We fetch Microsoft's official Linux x64 release rather
//! than the pyke CDN archive `ort-sys` itself uses, because the pyke
//! archive for this target ships only a static `libonnxruntime.a` and
//! load-dynamic needs the `.so`.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const ORT_VERSION: &str = "1.24.2";
const ORT_ARCHIVE_URL: &str =
    "https://github.com/microsoft/onnxruntime/releases/download/v1.24.2/onnxruntime-linux-x64-1.24.2.tgz";
const ORT_ARCHIVE_SHA256: &str = "43725474ba5663642e17684717946693850e2005efbd724ac72da278fead25e6";
/// The real shared object inside the archive — the other `libonnxruntime.so*`
/// entries are symlinks pointing at this one.
const ORT_SO_ENTRY: &str = "libonnxruntime.so.1.24.2";
const ORT_SO_SHA256: &str = "ffc84d48e845cf0b562ba4ea5ca32aaafc0d4069019fef4f63095b307d0270ad";

fn ort_dir_in(home: &Path) -> PathBuf {
    home.join(".canopy").join("ort")
}

fn ort_so_path_in(home: &Path) -> PathBuf {
    ort_dir_in(home).join("libonnxruntime.so")
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("Cannot determine home directory")
}

/// Where the runtime lives if it has already been downloaded, without
/// fetching it. `None` means "not present yet".
fn ort_runtime_path_in(home: &Path) -> Option<PathBuf> {
    let path = ort_so_path_in(home);
    path.exists().then_some(path)
}

/// Public presence check used by `canopy doctor` — reports whether the
/// runtime has been downloaded, never triggers a download.
pub fn ort_runtime_path() -> Option<PathBuf> {
    ort_runtime_path_in(&dirs::home_dir()?)
}

/// Ensure `~/.canopy/ort/libonnxruntime.so` exists, downloading it if
/// needed, and return its path. Idempotent: once the file is in place it is
/// returned immediately with no network access. A failed or interrupted
/// download leaves nothing behind — the `.so` is only moved into place
/// after it has been fully extracted and its SHA256 checked — so a later
/// call simply retries.
#[cfg(all(feature = "local-embeddings", target_os = "linux"))]
pub fn ensure_ort_runtime() -> Result<PathBuf> {
    ensure_ort_runtime_in(&home_dir()?)
}

#[cfg(not(all(feature = "local-embeddings", target_os = "linux")))]
pub fn ensure_ort_runtime() -> Result<PathBuf> {
    bail!("ONNX Runtime download is only supported on Linux with the local-embeddings feature")
}

fn ensure_ort_runtime_in(home: &Path) -> Result<PathBuf> {
    let so_path = ort_so_path_in(home);
    if so_path.exists() {
        return Ok(so_path);
    }

    let dir = ort_dir_in(home);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Cannot create ONNX Runtime dir: {}", dir.display()))?;

    let archive =
        download_archive().context("Failed to download ONNX Runtime for local embeddings")?;
    verify_sha256(&archive, ORT_ARCHIVE_SHA256)
        .context("ONNX Runtime archive failed its integrity check")?;

    let so_bytes = extract_so_from_archive(&archive)
        .context("Failed to extract libonnxruntime.so from the ONNX Runtime archive")?;
    verify_sha256(&so_bytes, ORT_SO_SHA256)
        .context("Extracted libonnxruntime.so failed its integrity check")?;

    // Write to a sibling temp file and rename into place so a crash
    // mid-write never leaves a truncated `.so` that later runs would trust.
    let tmp = dir.join(format!("libonnxruntime.so.download-{}", std::process::id()));
    let guard = TempFileGuard(&tmp);
    std::fs::write(&tmp, &so_bytes).with_context(|| format!("Cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &so_path).with_context(|| {
        format!(
            "Cannot move ONNX Runtime into place at {}",
            so_path.display()
        )
    })?;
    drop(guard);

    tracing::info!(
        "ONNX Runtime {ORT_VERSION} downloaded to {}",
        so_path.display()
    );
    Ok(so_path)
}

/// Best-effort cleanup of the partially written temp file if extraction or
/// the rename fails partway through.
struct TempFileGuard<'a>(&'a Path);

impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

fn download_archive() -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .context("Failed to build HTTP client")?;
    let resp = client
        .get(ORT_ARCHIVE_URL)
        .send()
        .with_context(|| format!("Request to {ORT_ARCHIVE_URL} failed"))?
        .error_for_status()
        .with_context(|| format!("{ORT_ARCHIVE_URL} returned an error status"))?;
    let bytes = resp
        .bytes()
        .context("Failed to read the ONNX Runtime archive body")?;
    Ok(bytes.to_vec())
}

fn verify_sha256(data: &[u8], expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if hex != expected {
        bail!("SHA256 mismatch: expected {expected}, got {hex}");
    }
    Ok(())
}

/// Pull the real `libonnxruntime.so` payload out of Microsoft's gzip
/// tarball. Returns its bytes; errors if the expected entry is absent.
fn extract_so_from_archive(archive_bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;

    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(archive_bytes));
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries().context("Failed to read tar entries")? {
        let mut entry = entry.context("Failed to read a tar entry")?;
        let is_target = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_os_string()))
            .is_some_and(|name| name == ORT_SO_ENTRY);

        if is_target {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .context("Failed to read libonnxruntime.so from the archive")?;
            return Ok(buf);
        }
    }

    bail!("{ORT_SO_ENTRY} not found in downloaded archive")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ort_dir_is_under_canopy_home() {
        let dir = ort_dir_in(Path::new("/home/someone"));
        assert_eq!(dir, Path::new("/home/someone/.canopy/ort"));
    }

    #[test]
    fn ort_so_path_ends_with_the_library() {
        let path = ort_so_path_in(Path::new("/home/someone"));
        assert_eq!(
            path,
            Path::new("/home/someone/.canopy/ort/libonnxruntime.so")
        );
    }

    #[test]
    fn ort_runtime_path_is_none_when_the_library_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ort_runtime_path_in(tmp.path()).is_none());
    }

    #[test]
    fn ort_runtime_path_is_some_when_the_library_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ort_dir_in(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        let so = dir.join("libonnxruntime.so");
        std::fs::write(&so, b"fake").unwrap();
        assert_eq!(ort_runtime_path_in(tmp.path()), Some(so));
    }

    fn make_targz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extract_pulls_the_real_so_entry_past_symlinks_and_docs() {
        let tgz = make_targz(&[
            ("onnxruntime-linux-x64-1.24.2/README.md", b"docs"),
            (
                "onnxruntime-linux-x64-1.24.2/lib/libonnxruntime.so.1.24.2",
                b"real shared object payload",
            ),
        ]);
        let got = extract_so_from_archive(&tgz).unwrap();
        assert_eq!(got, b"real shared object payload");
    }

    #[test]
    fn extract_errors_when_the_so_entry_is_missing() {
        let tgz = make_targz(&[("onnxruntime-linux-x64-1.24.2/README.md", b"docs")]);
        assert!(extract_so_from_archive(&tgz).is_err());
    }

    #[test]
    fn verify_sha256_accepts_matching_and_rejects_mismatched() {
        // echo -n "abc" | sha256sum
        let want = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256(b"abc", want).is_ok());
        assert!(verify_sha256(b"abd", want).is_err());
    }

    #[test]
    fn ensure_returns_the_existing_library_without_downloading() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ort_dir_in(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        let so = dir.join("libonnxruntime.so");
        std::fs::write(&so, b"cached runtime").unwrap();
        // No network configured in tests; this only passes via the
        // early-return path.
        assert_eq!(ensure_ort_runtime_in(tmp.path()).unwrap(), so);
    }
}
