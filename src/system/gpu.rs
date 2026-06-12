//! GPU detection: nvidia-smi, lspci (Linux), system_profiler (macOS).

use std::process::Command;

use super::platform::{infer_gpu_vendor, parse_optional_f32, parse_optional_u64};
use super::GpuInfo;

/// Try to read GPU info via `nvidia-smi`. Works on Linux, macOS, and Windows.
pub(super) fn try_get_nvidia_gpu_info() -> Option<GpuInfo> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,temperature.gpu,memory.used,memory.total,power.draw,power.limit",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let line = stdout.lines().find(|l| !l.trim().is_empty())?.to_string();
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    if parts.is_empty() {
        return None;
    }

    Some(GpuInfo {
        name: parts.first()?.to_string(),
        vendor: "NVIDIA".to_string(),
        usage: parse_optional_f32(parts.get(1).copied()),
        temperature: parse_optional_f32(parts.get(2).copied()),
        vram_used: parse_optional_u64(parts.get(3).copied()),
        vram_total: parse_optional_u64(parts.get(4).copied()),
        power_watts: parse_optional_f32(parts.get(5).copied()),
        power_limit_watts: parse_optional_f32(parts.get(6).copied()),
    })
}

/// Try to read GPU info via `lspci` (Linux).
pub(super) fn get_linux_gpu_info() -> Option<GpuInfo> {
    let output = Command::new("lspci").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout
        .lines()
        .find(|line| {
            line.contains(" VGA ")
                || line.contains("3D controller")
                || line.contains("Display controller")
        })
        .map(parse_lspci_gpu_line)
}

/// Try to read GPU info via `system_profiler` (macOS).
pub(super) fn get_macos_gpu_info() -> Option<GpuInfo> {
    let output = Command::new("system_profiler")
        .arg("SPDisplaysDataType")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("Chipset Model:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_string())
        .filter(|name| !name.is_empty())
        .map(|name| GpuInfo {
            vendor: infer_gpu_vendor(&name),
            name,
            ..GpuInfo::default()
        })
}

fn parse_lspci_gpu_line(line: &str) -> GpuInfo {
    let name = line
        .split_once(": ")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| line.trim().to_string());

    GpuInfo {
        vendor: infer_gpu_vendor(&name),
        name,
        ..GpuInfo::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lspci_gpu_line_preserves_device_name() {
        let gpu = parse_lspci_gpu_line(
            "01:00.0 VGA compatible controller: NVIDIA Corporation AD106M [GeForce RTX 4070 Max-Q / Mobile]",
        );
        assert!(gpu.name.contains("GeForce RTX 4070"));
        assert_eq!(gpu.vendor, "NVIDIA");
    }
}
