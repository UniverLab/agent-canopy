//! System dashboard UI component for sidebar

use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::DIM;
use crate::domain::canopy_config::TemperatureUnit;
use crate::system::{PowerSource, SystemInfo};

// ── Alert colors ────────────────────────────────────────────────

const WARN: Color = Color::Rgb(255, 193, 7); // amber
const DANGER: Color = Color::Rgb(229, 57, 53); // red

/// Pick an alert color based on value thresholds.
fn alert_color(value: f32, yellow: f32, red: f32) -> Color {
    if value >= red {
        DANGER
    } else if value >= yellow {
        WARN
    } else {
        DIM
    }
}

/// Pick an alert color for temperatures (Celsius).
fn temp_alert_color(temp_c: f32) -> Color {
    alert_color(temp_c, 70.0, 85.0)
}

/// Pick an alert color for GPU temperatures (Celsius).
fn gpu_temp_alert_color(temp_c: f32) -> Color {
    alert_color(temp_c, 75.0, 90.0)
}

/// Format bytes smartly: show in MB if < 1 GB, otherwise in GB, with 2 decimals
fn format_bytes_smart(bytes: u64) -> String {
    let gb = bytes as f32 / 1_073_741_824.0;
    if gb < 1.0 {
        let mb = bytes as f32 / 1_048_576.0;
        format!("{:.2}MB", mb)
    } else {
        format!("{:.2}GB", gb)
    }
}

/// Format megabytes smartly: show in MB if < 1024 MB, otherwise in GB, with 2 decimals
fn format_megabytes_smart(mb: u64) -> String {
    let gb = mb as f32 / 1024.0;
    if gb < 1.0 {
        format!("{:.2}MB", mb)
    } else {
        format!("{:.2}GB", gb)
    }
}

/// Format CPU frequency: GHz if >= 1000 MHz, otherwise MHz
fn format_cpu_frequency(mhz: Option<u64>) -> Option<String> {
    mhz.map(|f| {
        if f >= 1000 {
            format!("{:.2}GHz", f as f32 / 1000.0)
        } else {
            format!("{f}MHz")
        }
    })
}

/// Render the system dashboard in the sidebar
pub fn render_system_dashboard(
    frame: &mut Frame,
    area: Rect,
    system_info: &SystemInfo,
    temperature_unit: TemperatureUnit,
) {
    // Only render if we have enough space (3 content lines + 2 borders)
    if area.height < 5 {
        return;
    }

    let max_lines = area.height.saturating_sub(2) as usize;
    let dashboard = create_system_dashboard_lines(system_info, temperature_unit, max_lines);

    frame.render_widget(
        Paragraph::new(dashboard)
            .block(
                Block::default()
                    .title(
                        Line::from(Span::styled(" sysInfo ", Style::default().fg(DIM)))
                            .alignment(ratatui::layout::Alignment::Right),
                    )
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(DIM)),
            )
            .style(Style::default().fg(DIM)),
        area,
    );
}

/// Create the lines for the system dashboard
fn create_system_dashboard_lines(
    system_info: &SystemInfo,
    temperature_unit: TemperatureUnit,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let cpu_usage = system_info.cpu_usage_percent();
    let cpu_color = alert_color(cpu_usage, 70.0, 90.0);

    // Build CPU line: usage (alert) + temp (alert) + freq (dim) + cores (dim)
    let mut cpu_spans = vec![
        Span::styled("cpu: ", Style::default().fg(Color::White)),
        Span::styled(format!("{cpu_usage:.0}%"), Style::default().fg(cpu_color)),
    ];
    if let Some(temp_c) = system_info.cpu_temperature_celsius() {
        let temp_str = format_temperature(temp_c, temperature_unit);
        cpu_spans.push(Span::styled(
            format!(" {temp_str}"),
            Style::default().fg(temp_alert_color(temp_c)),
        ));
    }
    if let Some(freq) = format_cpu_frequency(system_info.cpu_frequency_mhz) {
        cpu_spans.push(Span::styled(format!(" {freq}"), Style::default().fg(DIM)));
    }
    if system_info.cpu_cores > 0 {
        cpu_spans.push(Span::styled(
            format!(" {}core", system_info.cpu_cores),
            Style::default().fg(DIM),
        ));
    }
    let mut lines = vec![Line::from(cpu_spans)];

    // GPU line right after CPU if available. The power draw is folded
    // into the same row when both are present — splitting "gpu usage" and
    // "gpu watts" into two lines made the sysinfo taller than it needs to
    // be, and the two values are conceptually one (the energy the chip is
    // drawing right now).
    if let Some(gpu) = &system_info.gpu_info {
        let usage_pct = gpu.usage.unwrap_or(0.0);
        let gpu_usage_color = alert_color(usage_pct, 70.0, 90.0);

        // Show VRAM as a single size figure ("1.25GB"). The percentage
        // version (`16% 1.25GB`) is dropped because the same info is
        // already implicit in the size, and the percentage competes with
        // the colored GPU usage percentage for attention at a glance.
        let vram_size_text = match (gpu.vram_used, gpu.vram_total) {
            (Some(used), Some(total)) if total > 0 => Some(format_megabytes_smart(used)),
            _ => None,
        };

        // Power draw (watts), folded into this row only when the GPU
        // itself is the source. Battery discharge is a system-wide number,
        // not the GPU's, so it gets its own `pwr:` row below instead.
        let power_watts = match system_info.power_source {
            Some(PowerSource::Gpu) => system_info.power_watts,
            _ => None,
        };

        // Only show GPU line if we have at least one piece of information
        if gpu.usage.is_some()
            || gpu.temperature.is_some()
            || vram_size_text.is_some()
            || power_watts.is_some()
        {
            let mut spans = vec![Span::styled("gpu: ", Style::default().fg(Color::White))];

            if let Some(usage) = gpu.usage {
                spans.push(Span::styled(
                    format!("{usage:.0}%"),
                    Style::default().fg(gpu_usage_color),
                ));
            }
            if let Some(temp) = gpu.temperature {
                let sep = if gpu.usage.is_some() { " " } else { "" };
                spans.push(Span::styled(
                    format!("{sep}{}", format_temperature(temp, temperature_unit)),
                    Style::default().fg(gpu_temp_alert_color(temp)),
                ));
            }
            // Layout: <usage>% <temp> · <vram> · <watts>W
            // The " · " separator is only emitted when at least one of
            // usage/temp has already been printed, so an empty start
            // (rare, only watt data) doesn't show a leading separator.
            let has_left = gpu.usage.is_some() || gpu.temperature.is_some();
            if let Some(ref vram) = vram_size_text {
                if has_left {
                    spans.push(Span::styled(" · ", Style::default().fg(Color::White)));
                }
                spans.push(Span::styled(vram.clone(), Style::default().fg(DIM)));
            }
            if let Some(watts) = power_watts {
                let any_left = has_left || vram_size_text.is_some();
                if any_left {
                    spans.push(Span::styled(" · ", Style::default().fg(Color::White)));
                }
                spans.push(Span::styled(
                    format!("{watts:.0}W"),
                    Style::default().fg(DIM),
                ));
            }

            lines.push(Line::from(spans));
        }
    }

    // Memory line
    let mem_pct = if system_info.memory_total > 0 {
        (system_info.memory_used as f32 / system_info.memory_total as f32) * 100.0
    } else {
        0.0
    };
    lines.push(Line::from(vec![
        Span::styled("mem: ", Style::default().fg(Color::White)),
        Span::styled(
            format!("{mem_pct:.0}%"),
            Style::default().fg(alert_color(mem_pct, 70.0, 90.0)),
        ),
        Span::styled(
            format!(" {}", format_bytes_smart(system_info.memory_used)),
            Style::default().fg(DIM),
        ),
    ]));

    // Battery discharge gets its own `pwr:` line — it's a system-wide
    // number, not the GPU's, so it doesn't belong folded into the GPU row
    // above (that row only folds in GPU-sourced power).
    if system_info.power_source == Some(PowerSource::Battery) {
        if let Some(watts) = system_info.power_watts {
            let pct = system_info
                .power_limit_watts
                .filter(|l| *l > 0.0)
                .map(|l| (watts / l) * 100.0);
            let mut spans = vec![Span::styled("pwr: ", Style::default().fg(Color::White))];
            if let Some(p) = pct {
                spans.push(Span::styled(
                    format!("{p:.0}%"),
                    Style::default().fg(alert_color(p, 70.0, 90.0)),
                ));
                spans.push(Span::styled(
                    format!(" {watts:.0}W"),
                    Style::default().fg(DIM),
                ));
            } else {
                spans.push(Span::styled(
                    format!("{watts:.0}W"),
                    Style::default().fg(DIM),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    // Swap line only if actually being used — always yellow, no percentage
    if system_info.swap_used > 0 {
        lines.push(Line::from(vec![
            Span::styled("swap: ", Style::default().fg(Color::White)),
            Span::styled(
                format_bytes_smart(system_info.swap_used),
                Style::default().fg(WARN),
            ),
        ]));
    }

    // Load average + process count merged into one line
    // Color is based on load per core: <0.7 green, <1.0 yellow, >=1.0 red
    if let Some(load) = system_info.load_average {
        let cores = system_info.cpu_cores.max(1) as f64;
        let load_per_core = load / cores;
        let load_color = if load_per_core >= 1.0 {
            DANGER
        } else if load_per_core >= 0.7 {
            WARN
        } else {
            DIM
        };
        let load_pct = (load_per_core * 100.0) as u32;
        lines.push(Line::from(vec![
            Span::styled("load: ", Style::default().fg(Color::White)),
            Span::styled(format!("{load_pct}%"), Style::default().fg(load_color)),
            Span::styled(format!(" {load:.2}"), Style::default().fg(DIM)),
            Span::styled(" · ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{} procs", system_info.process_count),
                Style::default().fg(DIM),
            ),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("procs: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{}", system_info.process_count),
                Style::default().fg(DIM),
            ),
        ]));
    }

    lines.truncate(max_lines);
    lines
}

fn format_temperature(temp_celsius: f32, unit: TemperatureUnit) -> String {
    match unit {
        TemperatureUnit::Celsius => format!("{temp_celsius:.0}°C"),
        TemperatureUnit::Fahrenheit => {
            let temp_f = temp_celsius * 9.0 / 5.0 + 32.0;
            format!("{temp_f:.0}°F")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::SystemInfo;

    #[test]
    fn test_dashboard_creation() {
        let info = SystemInfo::new();
        let lines = create_system_dashboard_lines(&info, TemperatureUnit::Celsius, 10);

        // Should have at least the 2 base lines (cpu, mem)
        assert!(
            lines.len() >= 2,
            "Expected at least 2 lines, got {}",
            lines.len()
        );
        // Check key lines exist
        let all_text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("cpu:"), "Missing cpu line");
        assert!(all_text.contains("mem:"), "Missing mem line");
        assert!(!all_text.contains("disk:"), "Disk line should be removed");
        // Power draw used to live in its own `pwr:` line; that line is
        // gone — power is folded into the GPU row (or omitted when no GPU
        // is present, which is the default for `SystemInfo::new()`).
        assert!(!all_text.contains("pwr:"), "pwr: line should be removed");
    }

    #[test]
    fn gpu_line_folds_power_and_drops_vram_percent() {
        let mut info = SystemInfo::new();
        info.gpu_info = Some(crate::system::GpuInfo {
            name: "Test GPU".to_string(),
            vendor: "NVIDIA".to_string(),
            usage: Some(6.0),
            temperature: Some(41.0),
            // nvidia-smi's memory.used reports MiB, which is the unit
            // `format_megabytes_smart` expects. 1280 MiB = 1.25 GiB.
            vram_used: Some(1280),
            vram_total: Some(8192),
            power_watts: Some(12.0),
            power_limit_watts: Some(150.0),
        });
        info.power_watts = Some(12.0);
        info.power_source = Some(crate::system::PowerSource::Gpu);

        let lines = create_system_dashboard_lines(&info, TemperatureUnit::Celsius, 10);
        let gpu_line = lines
            .iter()
            .map(|l| l.to_string())
            .find(|s| s.starts_with("gpu:"))
            .expect("expected a gpu line");

        // Layout: <usage>% <temp> · <vram> · <watts>W
        // No percentage in front of the vram size, no separate pwr: line.
        assert!(gpu_line.contains("6%"), "got: {gpu_line}");
        assert!(gpu_line.contains("41°C"), "got: {gpu_line}");
        assert!(gpu_line.contains("1.25GB"), "got: {gpu_line}");
        assert!(gpu_line.contains("12W"), "got: {gpu_line}");
        assert!(
            !gpu_line.contains("16%"),
            "vram percentage should be gone, got: {gpu_line}"
        );

        // No standalone pwr: line.
        let all = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all.contains("pwr:"), "got:\n{all}");
    }

    #[test]
    fn battery_power_gets_its_own_pwr_line_not_folded_into_gpu() {
        let mut info = SystemInfo::new();
        info.gpu_info = Some(crate::system::GpuInfo {
            name: "Test GPU".to_string(),
            vendor: "NVIDIA".to_string(),
            usage: Some(6.0),
            temperature: Some(41.0),
            vram_used: Some(1280),
            vram_total: Some(8192),
            // GPU itself reports no power draw...
            power_watts: None,
            power_limit_watts: None,
        });
        // ...the system-wide number comes from battery discharge instead.
        info.power_watts = Some(18.0);
        info.power_limit_watts = None;
        info.power_source = Some(crate::system::PowerSource::Battery);

        let lines = create_system_dashboard_lines(&info, TemperatureUnit::Celsius, 10);
        let gpu_line = lines
            .iter()
            .map(|l| l.to_string())
            .find(|s| s.starts_with("gpu:"))
            .expect("expected a gpu line");
        assert!(
            !gpu_line.contains("W"),
            "battery watts must not fold into the gpu line, got: {gpu_line}"
        );

        let pwr_line = lines
            .iter()
            .map(|l| l.to_string())
            .find(|s| s.starts_with("pwr:"))
            .expect("expected a standalone pwr line for battery-sourced power");
        assert!(pwr_line.contains("18W"), "got: {pwr_line}");
    }

    #[test]
    fn gpu_line_omits_watts_when_no_power_data() {
        let mut info = SystemInfo::new();
        info.gpu_info = Some(crate::system::GpuInfo {
            name: "Test GPU".to_string(),
            vendor: "NVIDIA".to_string(),
            usage: Some(6.0),
            temperature: Some(41.0),
            vram_used: Some(1280),
            vram_total: Some(8192),
            power_watts: None,
            power_limit_watts: None,
        });
        // System-level power also missing.
        info.power_watts = None;

        let lines = create_system_dashboard_lines(&info, TemperatureUnit::Celsius, 10);
        let gpu_line = lines
            .iter()
            .map(|l| l.to_string())
            .find(|s| s.starts_with("gpu:"))
            .expect("expected a gpu line");

        assert!(gpu_line.contains("6%"));
        assert!(gpu_line.contains("41°C"));
        assert!(gpu_line.contains("1.25GB"));
        assert!(
            !gpu_line.contains("W"),
            "no watts when power is unknown, got: {gpu_line}"
        );
        // Trailing separator must be elided too.
        assert!(
            !gpu_line.trim_end().ends_with("·"),
            "trailing separator when no watts, got: {gpu_line}"
        );
    }
}
