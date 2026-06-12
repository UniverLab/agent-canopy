//! Auto-update module for canopy
//!
//! Checks for new stable releases on GitHub once per day and
//! atomically replaces the running binary when a newer version exists.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

const GITHUB_REPO: &str = "UniverLab/harness-canopy";
const CHECK_INTERVAL_SECS: u64 = 24 * 3600;
const LAST_CHECK_FILE: &str = "last_update_check.txt";

#[derive(Deserialize, Debug)]
struct GitHubRelease {
    tag_name: String,
    prerelease: bool,
}

// ── Public API ──────────────────────────────────────────────────

/// Entry point: check once per day, download + install if newer.
pub fn check_and_update_if_needed() -> Result<bool> {
    if !should_check() {
        return Ok(false);
    }
    println!("  \x1b[34mℹ\x1b[0m  Checking for updates...");
    perform_update()
}

// ── Throttle ────────────────────────────────────────────────────

fn should_check() -> bool {
    let Ok(data_dir) = crate::ensure_data_dir() else {
        return true;
    };
    let Ok(content) = std::fs::read_to_string(data_dir.join(LAST_CHECK_FILE)) else {
        return true;
    };
    let Ok(last) = content.trim().parse::<u64>() else {
        return true;
    };
    let Ok(now) = now_secs() else {
        return true;
    };
    now.saturating_sub(last) >= CHECK_INTERVAL_SECS
}

fn record_check() -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;
    let ts = now_secs()?;
    std::fs::write(data_dir.join(LAST_CHECK_FILE), ts.to_string())?;
    Ok(())
}

fn now_secs() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock error")?
        .as_secs())
}

// ── Version helpers ─────────────────────────────────────────────

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn is_stable_version(tag: &str) -> bool {
    let v = tag.trim_start_matches('v');
    !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u32> {
        s.trim_start_matches('v')
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    let len = pa.len().max(pb.len());
    for i in 0..len {
        let cmp = pa
            .get(i)
            .copied()
            .unwrap_or(0)
            .cmp(&pb.get(i).copied().unwrap_or(0));
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
    }
    std::cmp::Ordering::Equal
}

// ── Release check ───────────────────────────────────────────────

fn fetch_latest_stable() -> Result<Option<String>> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases");
    let resp = reqwest::blocking::Client::new()
        .get(&url)
        .header("User-Agent", "canopy-autoupdate")
        .send()
        .context("failed to fetch GitHub releases")?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let releases: Vec<GitHubRelease> = resp.json().context("failed to parse releases JSON")?;

    let latest = releases
        .into_iter()
        .filter(|r| !r.prerelease && is_stable_version(&r.tag_name))
        .max_by(|a, b| compare_versions(&a.tag_name, &b.tag_name));

    match latest {
        Some(r)
            if compare_versions(&r.tag_name, current_version()) == std::cmp::Ordering::Greater =>
        {
            Ok(Some(r.tag_name))
        }
        _ => Ok(None),
    }
}

// ── Download + install ──────────────────────────────────────────

fn perform_update() -> Result<bool> {
    let Some(latest) = fetch_latest_stable()? else {
        let _ = record_check();
        return Ok(false);
    };

    println!(
        "  \x1b[33m⚠\x1b[0m  New stable version available: {}",
        latest
    );

    let current_exe = std::env::current_exe()?;
    let tmp = tempfile::tempdir()?;
    let tmp_bin = tmp.path().join("canopy-new");

    if !download_and_extract(&latest, &tmp_bin)? {
        eprintln!("  \x1b[31m✗\x1b[0m  Binary not found in archive");
        return Ok(false);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_bin, std::fs::Permissions::from_mode(0o755))?;
    }

    // Try rename (fast, same-fs); fall back to copy (cross-device)
    if std::fs::rename(&tmp_bin, &current_exe).is_err() {
        std::fs::copy(&tmp_bin, &current_exe)
            .context("failed to replace binary (copy fallback)")?;
    }

    let _ = record_check();
    println!("  \x1b[32m✓\x1b[0m  Updated to version {}", latest);
    Ok(true)
}

fn download_and_extract(version: &str, output: &Path) -> Result<bool> {
    let (os_target, arch_target) = detect_platform()?;
    let archive_name = format!(
        "canopy-{}-{}-{}.tar.gz",
        version.trim_start_matches('v'),
        arch_target,
        os_target
    );
    let url =
        format!("https://github.com/{GITHUB_REPO}/releases/download/{version}/{archive_name}");

    println!("  \x1b[34m↓\x1b[0m  Downloading {}", url);

    let resp = reqwest::blocking::Client::new()
        .get(&url)
        .header("User-Agent", "canopy-autoupdate")
        .send()
        .context("failed to download update")?;

    if !resp.status().is_success() {
        eprintln!(
            "  \x1b[31m✗\x1b[0m  Download failed: HTTP {}",
            resp.status()
        );
        return Ok(false);
    }

    // Stream response directly into the gzip decoder (no intermediate file)
    let decoder = flate2::read::GzDecoder::new(resp);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.file_name().is_some_and(|n| n == "canopy") {
            entry.unpack(output)?;
            return Ok(true);
        }
    }

    Ok(false)
}

fn detect_platform() -> Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        other => anyhow::bail!("unsupported OS: {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => anyhow::bail!("unsupported architecture: {other}"),
    };
    Ok((os, arch))
}

#[cfg(test)]
mod tests;
