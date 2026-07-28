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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_host_platform_returns_valid_platform() {
        let platform = detect_host_platform();
        // Should return one of the valid platforms
        assert!(
            matches!(
                platform,
                HostPlatform::Linux
                    | HostPlatform::MacOs
                    | HostPlatform::Windows
                    | HostPlatform::Wsl
            ),
            "should return a valid platform"
        );
    }

    #[test]
    fn normalize_temperature_accepts_positive_finite() {
        assert_eq!(normalize_temperature(Some(45.0)), Some(45.0));
        assert_eq!(normalize_temperature(Some(0.1)), Some(0.1));
        assert_eq!(normalize_temperature(Some(100.0)), Some(100.0));
    }

    #[test]
    fn normalize_temperature_rejects_zero_and_negative() {
        assert_eq!(normalize_temperature(Some(0.0)), None);
        assert_eq!(normalize_temperature(Some(-10.0)), None);
    }

    #[test]
    fn normalize_temperature_rejects_non_finite() {
        assert_eq!(normalize_temperature(Some(f32::INFINITY)), None);
        assert_eq!(normalize_temperature(Some(f32::NAN)), None);
        assert_eq!(normalize_temperature(Some(f32::NEG_INFINITY)), None);
    }

    #[test]
    fn normalize_temperature_handles_none() {
        assert_eq!(normalize_temperature(None), None);
    }

    #[test]
    fn is_cpu_temperature_label_matches_cpu_keywords() {
        assert!(is_cpu_temperature_label("cpu_temp"));
        assert!(is_cpu_temperature_label("package id 0"));
        assert!(is_cpu_temperature_label("tctl"));
        assert!(is_cpu_temperature_label("tdie"));
        assert!(is_cpu_temperature_label("coretemp-isa-0000"));
    }

    #[test]
    fn is_cpu_temperature_label_rejects_non_cpu() {
        assert!(!is_cpu_temperature_label("gpu_temp"));
        assert!(!is_cpu_temperature_label("fan_speed"));
        assert!(!is_cpu_temperature_label("voltage"));
    }

    #[test]
    fn is_gpu_temperature_label_matches_gpu_keywords() {
        assert!(is_gpu_temperature_label("gpu_temp"));
        assert!(is_gpu_temperature_label("graphics temperature"));
        assert!(is_gpu_temperature_label("junction_temp"));
    }

    #[test]
    fn is_gpu_temperature_label_rejects_non_gpu() {
        assert!(!is_gpu_temperature_label("cpu_temp"));
        assert!(!is_gpu_temperature_label("fan_speed"));
        assert!(!is_gpu_temperature_label("voltage"));
    }

    #[test]
    fn bytes_to_megabytes_converts_correctly() {
        assert_eq!(bytes_to_megabytes(0), 0);
        assert_eq!(bytes_to_megabytes(1024 * 1024), 1);
        assert_eq!(bytes_to_megabytes(10 * 1024 * 1024), 10);
        assert_eq!(bytes_to_megabytes(1024), 0); // Less than 1 MB rounds down
    }
}
