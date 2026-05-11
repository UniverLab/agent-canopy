//! System monitoring with host-aware fallbacks.
//!
//! `sysinfo` provides process/runtime metrics, but under WSL it only sees the Linux
//! guest. For hardware-facing values like installed RAM and GPU, query the Windows
//! host when possible and fall back to platform-local commands elsewhere.

mod gpu;
mod platform;
mod windows;

use sysinfo::{Components, Disks, System};

use gpu::{get_linux_gpu_info, get_macos_gpu_info, try_get_nvidia_gpu_info};
use platform::{
    detect_host_platform, is_cpu_temperature_label, is_gpu_temperature_label,
    normalize_temperature, HostPlatform,
};
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
    pub disk_used: u64,
    pub disk_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub load_average: Option<f64>,
    pub gpu_info: Option<GpuInfo>,
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

        let disks = Disks::new_with_refreshed_list();
        let (disk_total, disk_used) = get_main_disk(&disks);
        self.disk_total = disk_total;
        self.disk_used = disk_used;
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

fn get_main_disk(disks: &Disks) -> (u64, u64) {
    let target = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "/".to_string());

    let mut best: Option<&sysinfo::Disk> = None;
    let mut best_len = 0;
    for disk in disks.iter() {
        let mp = disk.mount_point().to_str().unwrap_or("");
        if target.starts_with(mp) && mp.len() > best_len {
            best = Some(disk);
            best_len = mp.len();
        }
    }

    best.map(|disk| {
        let total = disk.total_space();
        (total, total - disk.available_space())
    })
    .unwrap_or((0, 0))
}

fn get_load_average() -> Option<f64> {
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        Some(System::load_average().one)
    } else {
        None
    }
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
}
