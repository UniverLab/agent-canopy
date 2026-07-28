//! Windows and WSL host metrics via PowerShell and WMI.

use serde::Deserialize;

use super::platform::{bytes_to_megabytes, infer_gpu_vendor, run_powershell_json};
use super::{GpuInfo, HostMetrics};
use crate::system::gpu::try_get_nvidia_gpu_info;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct WindowsHostSnapshot {
    pub cpu_temperature_c: Option<f32>,
    pub cpu_usage_percent: Option<f32>,
    pub installed_memory_bytes: Option<u64>,
    pub visible_memory_bytes: Option<u64>,
    pub free_memory_bytes: Option<u64>,
    pub gpu_usage_percent: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsVideoController {
    name: Option<String>,
    adapter_compatibility: Option<String>,
    adapter_ram: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            Self::One(item) => vec![item],
            Self::Many(items) => items,
        }
    }
}

/// Collect CPU, memory, and GPU metrics from the Windows host.
pub(super) fn get_windows_host_metrics() -> HostMetrics {
    let snapshot = get_windows_host_snapshot();
    let mut gpu_info = try_get_nvidia_gpu_info().or_else(get_windows_gpu_info);

    if let Some(usage) = snapshot.gpu_usage_percent {
        if let Some(gpu) = gpu_info.as_mut() {
            gpu.usage = Some(usage);
        }
    }

    let memory_total = snapshot
        .installed_memory_bytes
        .or(snapshot.visible_memory_bytes)
        .filter(|total| *total > 0);

    let memory_used = match (snapshot.visible_memory_bytes, snapshot.free_memory_bytes) {
        (Some(visible), Some(free)) if visible >= free => Some(visible - free),
        _ => None,
    }
    .map(|used| match memory_total {
        Some(total) => used.min(total),
        None => used,
    });

    HostMetrics {
        cpu_usage: snapshot.cpu_usage_percent,
        cpu_temperature: snapshot.cpu_temperature_c,
        memory_used,
        memory_total,
        gpu_info,
    }
}

fn get_windows_host_snapshot() -> WindowsHostSnapshot {
    let script = concat!(
        "$os = Get-CimInstance Win32_OperatingSystem; ",
        "$dimms = Get-CimInstance Win32_PhysicalMemory | Measure-Object -Property Capacity -Sum; ",
        "$cpuCounter = (Get-Counter '\\Processor(_Total)\\% Processor Time' -ErrorAction SilentlyContinue).CounterSamples | Select-Object -First 1 -ExpandProperty CookedValue; ",
        "$thermal = Get-CimInstance -Namespace root/wmi -Class MSAcpi_ThermalZoneTemperature -ErrorAction SilentlyContinue; ",
        "$cpuTemp = $null; ",
        "if ($thermal) { ",
        "  $temps = @($thermal | Where-Object { $_.CurrentTemperature -gt 0 } | ForEach-Object { ($_.CurrentTemperature / 10) - 273.15 }); ",
        "  if ($temps.Count -gt 0) { $cpuTemp = [math]::Round((($temps | Measure-Object -Average).Average), 1) } ",
        "} ",
        "$gpuUsage = $null; ",
        "try { ",
        "  $gpuCounters = (Get-Counter '\\GPU Engine(*)\\Utilization Percentage' -ErrorAction Stop).CounterSamples; ",
        "  $gpu3d = @($gpuCounters | Where-Object { $_.InstanceName -match 'engtype_3D' }); ",
        "  $samples = if ($gpu3d.Count -gt 0) { $gpu3d } else { $gpuCounters }; ",
        "  if ($samples.Count -gt 0) { ",
        "    $sum = ($samples | Measure-Object -Property CookedValue -Sum).Sum; ",
        "    if ($null -ne $sum) { $gpuUsage = [math]::Min([math]::Round([double]$sum, 1), 100) } ",
        "  } ",
        "} catch {} ",
        "[pscustomobject]@{",
        "CpuTemperatureC = $cpuTemp; ",
        "CpuUsagePercent = if ($null -ne $cpuCounter) { [math]::Round([double]$cpuCounter, 1) } else { $null }; ",
        "InstalledMemoryBytes = [uint64]($dimms.Sum); ",
        "VisibleMemoryBytes = [uint64]($os.TotalVisibleMemorySize * 1KB); ",
        "FreeMemoryBytes = [uint64]($os.FreePhysicalMemory * 1KB); ",
        "GpuUsagePercent = $gpuUsage",
        "} | ConvertTo-Json -Compress"
    );

    run_powershell_json::<WindowsHostSnapshot>(script).unwrap_or_default()
}

fn get_windows_gpu_info() -> Option<GpuInfo> {
    let script = concat!(
        "Get-CimInstance Win32_VideoController ",
        "| Where-Object { $_.Name -and $_.Name -notmatch 'Microsoft Basic|Remote Display|Hyper-V' } ",
        "| Select-Object Name,AdapterCompatibility,AdapterRAM ",
        "| ConvertTo-Json -Compress"
    );

    let controllers = run_powershell_json::<OneOrMany<WindowsVideoController>>(script)?
        .into_vec()
        .into_iter()
        .filter(|c| c.name.as_ref().is_some_and(|n| !n.trim().is_empty()))
        .collect::<Vec<_>>();

    let controller = controllers.into_iter().next()?;
    let name = controller.name?.trim().to_string();
    let vendor = controller
        .adapter_compatibility
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| infer_gpu_vendor(&name));

    Some(GpuInfo {
        name,
        vendor,
        vram_total: controller.adapter_ram.map(bytes_to_megabytes),
        ..GpuInfo::default()
    })
}
