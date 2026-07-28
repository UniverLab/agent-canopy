//! System monitoring with host-aware fallbacks.
//!
//! `sysinfo` provides process/runtime metrics, but under WSL it only sees the Linux
//! guest. For hardware-facing values like installed RAM and GPU, query the Windows
//! host when possible and fall back to platform-local commands elsewhere.

mod gpu;
mod platform;
mod power;
mod windows;

use std::path::Path;

use sysinfo::{Components, System};

use gpu::{get_linux_gpu_info, get_macos_gpu_info, try_get_nvidia_gpu_info};
use platform::{
    detect_host_platform, is_cpu_temperature_label, is_gpu_temperature_label,
    normalize_temperature, HostPlatform,
};
use power::{get_linux_battery_watts, get_macos_battery_watts};
use windows::get_windows_host_metrics;

/// System information and metrics.
#[derive(Debug, Default, Clone)]
pub struct SystemInfo {
    pub cpu_usage: f32,
    pub cpu_cores: usize,
    pub cpu_temperature: Option<f32>,
    pub cpu_frequency_mhz: Option<u64>,
    pub memory_used: u64,
    pub memory_total: u64,
    pub system_uptime: u64,
    pub process_count: usize,
    pub swap_used: u64,
    pub swap_total: u64,
    pub load_average: Option<f64>,
    pub gpu_info: Option<GpuInfo>,
    pub power_watts: Option<f32>,
    pub power_limit_watts: Option<f32>,
    pub power_source: Option<PowerSource>,
}

/// Where `SystemInfo::power_watts` was sourced from. The GPU dashboard row
/// folds power in only when it's the GPU's own draw; battery discharge is
/// system-wide and gets its own `pwr:` row instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    Battery,
    Gpu,
}

/// GPU information.
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: String,
    pub usage: Option<f32>,
    pub temperature: Option<f32>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    pub power_watts: Option<f32>,
    pub power_limit_watts: Option<f32>,
}

/// Aggregated host-level metrics that override sysinfo values.
#[derive(Debug, Default)]
struct HostMetrics {
    cpu_usage: Option<f32>,
    cpu_temperature: Option<f32>,
    memory_used: Option<u64>,
    memory_total: Option<u64>,
    gpu_info: Option<GpuInfo>,
}

impl SystemInfo {
    pub fn new() -> Self {
        let mut this = Self::default();
        this.update();
        this
    }

    pub fn update(&mut self) {
        self.refresh_sysinfo_metrics();
        self.apply_component_metrics();
        self.apply_host_metrics();
        self.apply_power_metrics();
    }

    fn refresh_sysinfo_metrics(&mut self) {
        let mut system = System::new();
        system.refresh_cpu_usage();
        system.refresh_memory();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        system.refresh_cpu_frequency();

        self.cpu_usage = system.global_cpu_usage();
        self.cpu_cores = system.cpus().len();
        self.cpu_frequency_mhz = system.cpus().first().map(|c| c.frequency());
        self.memory_used = system.used_memory();
        self.memory_total = system.total_memory();
        self.system_uptime = System::uptime();
        self.process_count = system.processes().len();
        self.swap_used = system.used_swap();
        self.swap_total = system.total_swap();
        self.load_average = get_load_average();
        self.cpu_temperature = None;
        self.gpu_info = None;
    }

    fn apply_component_metrics(&mut self) {
        let components = Components::new_with_refreshed_list();
        let mut cpu_temperature = None;
        let mut gpu_temperature = None;

        for component in &components {
            let Some(temperature) = normalize_temperature(component.temperature()) else {
                continue;
            };
            let label = component.label().to_ascii_lowercase();
            if cpu_temperature.is_none() && is_cpu_temperature_label(&label) {
                cpu_temperature = Some(temperature);
            }
            if gpu_temperature.is_none() && is_gpu_temperature_label(&label) {
                gpu_temperature = Some(temperature);
            }
        }

        self.cpu_temperature = cpu_temperature;
        if let Some(temperature) = gpu_temperature {
            self.gpu_info = Some(GpuInfo {
                temperature: Some(temperature),
                ..GpuInfo::default()
            });
        }
    }

    fn apply_host_metrics(&mut self) {
        let host = self.collect_host_metrics();

        if let Some(v) = host.cpu_usage {
            self.cpu_usage = v;
        }
        if let Some(v) = host.cpu_temperature {
            self.cpu_temperature = Some(v);
        }
        if let Some(v) = host.memory_used {
            self.memory_used = v;
        }
        if let Some(v) = host.memory_total {
            self.memory_total = v;
        }
        if let Some(gpu) = host.gpu_info {
            self.gpu_info = Some(gpu);
        }
    }

    /// Prefer total-system draw (battery discharge) and fall back to GPU draw.
    fn apply_power_metrics(&mut self) {
        let battery_watts = match detect_host_platform() {
            HostPlatform::Linux => get_linux_battery_watts(),
            HostPlatform::MacOs => get_macos_battery_watts(),
            HostPlatform::Wsl | HostPlatform::Windows => None,
        };

        match battery_watts {
            Some(watts) => {
                self.power_watts = Some(watts);
                self.power_limit_watts = None;
                self.power_source = Some(PowerSource::Battery);
            }
            None => {
                let gpu = self.gpu_info.as_ref();
                self.power_watts = gpu.and_then(|g| g.power_watts);
                self.power_limit_watts = gpu.and_then(|g| g.power_limit_watts);
                self.power_source = self.power_watts.map(|_| PowerSource::Gpu);
            }
        }
    }

    fn collect_host_metrics(&self) -> HostMetrics {
        match detect_host_platform() {
            HostPlatform::Wsl | HostPlatform::Windows => get_windows_host_metrics(),
            HostPlatform::Linux => HostMetrics {
                gpu_info: get_linux_gpu_info().or_else(try_get_nvidia_gpu_info),
                ..HostMetrics::default()
            },
            HostPlatform::MacOs => HostMetrics {
                gpu_info: get_macos_gpu_info().or_else(try_get_nvidia_gpu_info),
                ..HostMetrics::default()
            },
        }
    }

    pub fn cpu_usage_percent(&self) -> f32 {
        self.cpu_usage
    }

    pub fn cpu_temperature_celsius(&self) -> Option<f32> {
        self.cpu_temperature
    }

    #[allow(dead_code)]
    pub fn gpu_vram_used_mb(&self) -> Option<u64> {
        self.gpu_info.as_ref()?.vram_used
    }

    #[allow(dead_code)]
    pub fn gpu_vram_total_mb(&self) -> Option<u64> {
        self.gpu_info.as_ref()?.vram_total
    }

    #[allow(dead_code)]
    pub fn gpu_vram_usage_percent(&self) -> Option<f32> {
        let gpu = self.gpu_info.as_ref()?;
        let (used, total) = (gpu.vram_used?, gpu.vram_total?);
        if total == 0 {
            return None;
        }
        Some((used as f32 / total as f32) * 100.0)
    }
}

fn get_load_average() -> Option<f64> {
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        Some(System::load_average().one)
    } else {
        None
    }
}

/// Current machine boot id, stable for the lifetime of the running boot and
/// different after every reboot (including a WSL restart). Used to tell
/// whether a PID persisted in an earlier run could plausibly still refer to
/// the process that recorded it — after a reboot the OS recycles PIDs from
/// scratch, so a stored PID matching a live process is a coincidence, not
/// evidence the original process survived.
///
/// `None` if the id can't be read (non-Linux host, sandboxed environment, ...).
pub fn boot_id() -> Option<String> {
    read_boot_id_from(Path::new("/proc/sys/kernel/random/boot_id"))
}

fn read_boot_id_from(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_info_creation_returns_valid_ranges() {
        let info = SystemInfo::new();
        assert!((0.0..=100.0).contains(&info.cpu_usage));
        assert!(info.memory_total >= info.memory_used);
        assert!(info.system_uptime > 0);
    }

    #[test]
    fn memory_used_never_exceeds_total() {
        let info = SystemInfo::new();
        assert!(info.memory_total >= info.memory_used);
    }

    #[test]
    fn gpu_vram_usage_percent_is_bounded() {
        let info = SystemInfo::new();
        if let Some(pct) = info.gpu_vram_usage_percent() {
            assert!((0.0..=100.0).contains(&pct));
        }
    }

    #[test]
    fn read_boot_id_from_trims_trailing_newline() {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        std::fs::write(tmp.path(), "abcd-1234\n").expect("write boot id");
        assert_eq!(read_boot_id_from(tmp.path()), Some("abcd-1234".to_string()));
    }

    #[test]
    fn read_boot_id_from_missing_file_returns_none() {
        assert_eq!(
            read_boot_id_from(Path::new("/nonexistent/boot_id_path")),
            None
        );
    }

    #[test]
    fn boot_id_is_available_on_linux() {
        if cfg!(target_os = "linux") {
            assert!(boot_id().is_some());
        }
    }

    #[test]
    fn system_info_default_has_zeroed_values() {
        let info = SystemInfo::default();
        assert_eq!(info.cpu_usage, 0.0);
        assert_eq!(info.cpu_cores, 0);
        assert!(info.cpu_temperature.is_none());
        assert_eq!(info.memory_used, 0);
        assert_eq!(info.memory_total, 0);
        assert!(info.gpu_info.is_none());
        assert!(info.power_watts.is_none());
        assert!(info.power_limit_watts.is_none());
        assert!(info.power_source.is_none());
    }

    #[test]
    fn cpu_usage_percent_returns_inner() {
        let info = SystemInfo {
            cpu_usage: 42.5,
            ..SystemInfo::default()
        };
        assert_eq!(info.cpu_usage_percent(), 42.5);
    }

    #[test]
    fn cpu_temperature_celsius_returns_inner() {
        let info = SystemInfo::default();
        assert!(info.cpu_temperature_celsius().is_none());
        let info = SystemInfo {
            cpu_temperature: Some(65.0),
            ..SystemInfo::default()
        };
        assert_eq!(info.cpu_temperature_celsius(), Some(65.0));
    }

    #[test]
    fn gpu_vram_used_mb_returns_none_when_no_gpu() {
        let info = SystemInfo::default();
        assert!(info.gpu_vram_used_mb().is_none());
    }

    #[test]
    fn gpu_vram_used_mb_returns_vram_used() {
        let info = SystemInfo {
            gpu_info: Some(GpuInfo {
                vram_used: Some(4096),
                ..GpuInfo::default()
            }),
            ..SystemInfo::default()
        };
        assert_eq!(info.gpu_vram_used_mb(), Some(4096));
    }

    #[test]
    fn gpu_vram_total_mb_returns_none_when_no_gpu() {
        let info = SystemInfo::default();
        assert!(info.gpu_vram_total_mb().is_none());
    }

    #[test]
    fn gpu_vram_total_mb_returns_vram_total() {
        let info = SystemInfo {
            gpu_info: Some(GpuInfo {
                vram_total: Some(8192),
                ..GpuInfo::default()
            }),
            ..SystemInfo::default()
        };
        assert_eq!(info.gpu_vram_total_mb(), Some(8192));
    }

    #[test]
    fn gpu_vram_usage_percent_none_when_no_gpu() {
        let info = SystemInfo::default();
        assert!(info.gpu_vram_usage_percent().is_none());
    }

    #[test]
    fn gpu_vram_usage_percent_none_when_no_used() {
        let info = SystemInfo {
            gpu_info: Some(GpuInfo {
                vram_used: None,
                vram_total: Some(8192),
                ..GpuInfo::default()
            }),
            ..SystemInfo::default()
        };
        assert!(info.gpu_vram_usage_percent().is_none());
    }

    #[test]
    fn gpu_vram_usage_percent_none_when_no_total() {
        let info = SystemInfo {
            gpu_info: Some(GpuInfo {
                vram_used: Some(4096),
                vram_total: None,
                ..GpuInfo::default()
            }),
            ..SystemInfo::default()
        };
        assert!(info.gpu_vram_usage_percent().is_none());
    }

    #[test]
    fn gpu_vram_usage_percent_none_when_total_zero() {
        let info = SystemInfo {
            gpu_info: Some(GpuInfo {
                vram_used: Some(100),
                vram_total: Some(0),
                ..GpuInfo::default()
            }),
            ..SystemInfo::default()
        };
        assert!(info.gpu_vram_usage_percent().is_none());
    }

    #[test]
    fn gpu_vram_usage_percent_calculates_correctly() {
        let info = SystemInfo {
            gpu_info: Some(GpuInfo {
                vram_used: Some(3072),
                vram_total: Some(8192),
                ..GpuInfo::default()
            }),
            ..SystemInfo::default()
        };
        let pct = info.gpu_vram_usage_percent().unwrap();
        let expected = (3072.0 / 8192.0) * 100.0;
        assert!((pct - expected).abs() < 0.01);
    }

    #[test]
    fn gpu_vram_usage_percent_zero_used() {
        let info = SystemInfo {
            gpu_info: Some(GpuInfo {
                vram_used: Some(0),
                vram_total: Some(8192),
                ..GpuInfo::default()
            }),
            ..SystemInfo::default()
        };
        assert_eq!(info.gpu_vram_usage_percent(), Some(0.0));
    }

    #[test]
    fn gpu_vram_usage_percent_full_used() {
        let info = SystemInfo {
            gpu_info: Some(GpuInfo {
                vram_used: Some(8192),
                vram_total: Some(8192),
                ..GpuInfo::default()
            }),
            ..SystemInfo::default()
        };
        assert_eq!(info.gpu_vram_usage_percent(), Some(100.0));
    }

    #[test]
    fn power_source_equality() {
        assert_eq!(PowerSource::Battery, PowerSource::Battery);
        assert_eq!(PowerSource::Gpu, PowerSource::Gpu);
        assert_ne!(PowerSource::Battery, PowerSource::Gpu);
    }

    #[test]
    fn gpu_info_default() {
        let gpu = GpuInfo::default();
        assert!(gpu.name.is_empty());
        assert!(gpu.vendor.is_empty());
        assert!(gpu.usage.is_none());
        assert!(gpu.temperature.is_none());
        assert!(gpu.vram_used.is_none());
        assert!(gpu.vram_total.is_none());
        assert!(gpu.power_watts.is_none());
        assert!(gpu.power_limit_watts.is_none());
    }
}
