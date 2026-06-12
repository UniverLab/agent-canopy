//! Host platform detection and shared parsing helpers.

use serde::Deserialize;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostPlatform {
    Linux,
    MacOs,
    Windows,
    Wsl,
}

pub(super) fn detect_host_platform() -> HostPlatform {
    if cfg!(target_os = "windows") {
        return HostPlatform::Windows;
    }
    if cfg!(target_os = "macos") {
        return HostPlatform::MacOs;
    }
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        if version.to_lowercase().contains("microsoft") {
            return HostPlatform::Wsl;
        }
    }
    HostPlatform::Linux
}

pub(super) fn run_powershell_json<T>(script: &str) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    let output = Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(script)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    serde_json::from_str(trimmed).ok()
}

pub(super) fn infer_gpu_vendor(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.contains("nvidia") || lower.contains("geforce") || lower.contains("quadro") {
        "NVIDIA".to_string()
    } else if lower.contains("amd") || lower.contains("radeon") || lower.contains("ati") {
        "AMD".to_string()
    } else if lower.contains("intel") || lower.contains("arc") || lower.contains("uhd") {
        "Intel".to_string()
    } else if lower.contains("apple") {
        "Apple".to_string()
    } else {
        "GPU".to_string()
    }
}

pub(super) fn parse_optional_f32(value: Option<&str>) -> Option<f32> {
    value
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<f32>().ok())
}

pub(super) fn parse_optional_u64(value: Option<&str>) -> Option<u64> {
    value
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<u64>().ok())
}

pub(super) fn normalize_temperature(value: Option<f32>) -> Option<f32> {
    value.filter(|t| t.is_finite() && *t > 0.0)
}

pub(super) fn is_cpu_temperature_label(label: &str) -> bool {
    label.contains("cpu")
        || label.contains("package")
        || label.contains("tctl")
        || label.contains("tdie")
        || label.contains("coretemp")
}

pub(super) fn is_gpu_temperature_label(label: &str) -> bool {
    label.contains("gpu") || label.contains("graphics") || label.contains("junction")
}

pub(super) fn bytes_to_megabytes(bytes: u64) -> u64 {
    bytes / 1024 / 1024
}
