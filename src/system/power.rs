//! System power draw (watts) from platform battery sources.
//!
//! Battery discharge rate is the only total-system reading available without
//! root or extra tooling, so it is only reported while discharging; callers
//! fall back to GPU power draw when no battery reading is available.

use std::process::Command;

/// Linux: instantaneous battery discharge from `/sys/class/power_supply`.
pub(super) fn get_linux_battery_watts() -> Option<f32> {
    let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
        if status.trim() != "Discharging" {
            continue;
        }
        // power_now is µW; some drivers only expose current_now (µA) + voltage_now (µV)
        if let Some(uw) = read_micro(&path.join("power_now")) {
            if uw > 0 {
                return Some(uw as f32 / 1_000_000.0);
            }
        }
        if let (Some(ua), Some(uv)) = (
            read_micro(&path.join("current_now")),
            read_micro(&path.join("voltage_now")),
        ) {
            if ua > 0 && uv > 0 {
                return Some((ua as f64 * uv as f64 / 1e12) as f32);
            }
        }
    }
    None
}

/// macOS: battery discharge via `ioreg -rn AppleSmartBattery`.
pub(super) fn get_macos_battery_watts() -> Option<f32> {
    let output = Command::new("ioreg")
        .args(["-rn", "AppleSmartBattery"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_battery_watts(&stdout)
}

/// Amperage (mA, negative while discharging) × Voltage (mV) → watts.
fn parse_battery_watts(ioreg_output: &str) -> Option<f32> {
    let amperage = parse_ioreg_i64(ioreg_output, "Amperage")?;
    let voltage = parse_ioreg_i64(ioreg_output, "Voltage")?;
    if amperage >= 0 || voltage <= 0 {
        return None;
    }
    Some(((-amperage) as f32 * voltage as f32) / 1_000_000.0)
}

/// ioreg prints negative values as two's-complement u64, so try both parses.
fn parse_ioreg_i64(text: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\" =");
    let value = text
        .lines()
        .find(|line| line.contains(&needle))?
        .split('=')
        .nth(1)?
        .trim();
    value
        .parse::<i64>()
        .ok()
        .or_else(|| value.parse::<u64>().ok().map(|v| v as i64))
}

fn read_micro(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_battery_watts_while_discharging() {
        // -1500 mA encoded as two's-complement u64, 12000 mV → 18 W
        let output = concat!(
            "      \"Amperage\" = 18446744073709550116\n",
            "      \"Voltage\" = 12000\n",
        );
        let watts = parse_battery_watts(output).expect("should parse discharge");
        assert!((watts - 18.0).abs() < 0.01, "got {watts}");
    }

    #[test]
    fn parse_battery_watts_hidden_when_charging() {
        let output = concat!("      \"Amperage\" = 850\n", "      \"Voltage\" = 12000\n",);
        assert!(parse_battery_watts(output).is_none());
    }
}
