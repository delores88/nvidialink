//! Read-only local Windows form factor. A battery API failure or an unknown
//! chassis never becomes a claim that this machine is a desktop.

#[cfg(any(windows, test))]
fn no_system_battery(flag: Option<u8>) -> bool {
    // 255 means unknown, despite also containing the no-battery bit.
    flag.is_some_and(|value| value != 255 && value & 128 != 0)
}

#[cfg(any(windows, test))]
fn classify(flag: Option<u8>, chassis: &[u16]) -> &'static str {
    // Desktop/tower, all-in-one, space-saving and other nonportable PC chassis.
    // Explicit chassis evidence also avoids calling a desktop with a UPS a laptop.
    if no_system_battery(flag)
        || chassis
            .iter()
            .any(|value| matches!(value, 3..=7 | 13 | 15..=17 | 23 | 24))
    {
        return "desktop";
    }
    if chassis
        .iter()
        .any(|value| matches!(value, 8..=11 | 14 | 30..=32))
        || flag.is_some_and(|value| value & !15 == 0)
    {
        // BatteryFlag=0 is a known battery between low/high charge, not missing.
        return "laptop";
    }
    "unknown"
}

#[cfg(any(windows, test))]
fn chassis_from_output(raw: &[u8]) -> Option<Vec<u16>> {
    if raw.len() > 1024 {
        return None;
    }
    let text = std::str::from_utf8(raw)
        .ok()?
        .trim_start_matches('\u{feff}');
    let values: Vec<u16> = serde_json::from_str(text).ok()?;
    (values.len() <= 32).then_some(values)
}

#[cfg(windows)]
fn battery_flag() -> Option<u8> {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    // All members are integer fields. A failed API call contributes no evidence.
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    (unsafe { GetSystemPowerStatus(&mut status) } != 0).then_some(status.BatteryFlag)
}

#[cfg(windows)]
fn chassis_types() -> Option<Vec<u16>> {
    use crate::transport::{self, Ran};
    use std::{process::Command, time::Duration};
    // Fixed, local-only CIM query; no network session or owner configuration.
    // The outer process budget also covers PowerShell startup/provider delays.
    let mut command = Command::new("powershell");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "$ErrorActionPreference='Stop'; $types=@(Get-CimInstance -ClassName Win32_SystemEnclosure -Property ChassisTypes -OperationTimeoutSec 1 | ForEach-Object { $_.ChassisTypes } | Select-Object -First 32); ConvertTo-Json -InputObject $types -Compress",
    ]);
    match transport::run_process(command, &[], Duration::from_secs(2)).ok()? {
        Ran::Exited((Some(0), stdout, _)) => chassis_from_output(&stdout),
        _ => None,
    }
}

/// The local machine only. Observation does not depend on a configured Spark,
/// and never changes battery, firewall, network or device settings.
#[cfg(windows)]
pub fn observe() -> &'static str {
    let flag = battery_flag();
    if no_system_battery(flag) {
        return "desktop";
    }
    classify(flag, chassis_types().as_deref().unwrap_or_default())
}

#[cfg(not(windows))]
pub fn observe() -> &'static str {
    "unknown"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_battery_is_not_a_no_battery_flag() {
        assert!(!no_system_battery(Some(255)));
        assert!(!no_system_battery(None));
        assert_eq!(classify(Some(255), &[]), "unknown");
        assert_eq!(classify(None, &[1, 2]), "unknown");
        assert_eq!(classify(Some(16), &[]), "unknown");
    }

    #[test]
    fn no_battery_or_desktop_chassis_selects_desktop() {
        assert!(no_system_battery(Some(128)));
        assert_eq!(classify(Some(128), &[]), "desktop");
        assert_eq!(classify(Some(128), &[9]), "desktop");
        for chassis in [3, 4, 5, 6, 7, 13, 15, 16, 17, 23, 24] {
            // Battery evidence alone must not mislabel a tower with a UPS.
            assert_eq!(classify(Some(1), &[chassis]), "desktop");
            assert_eq!(classify(None, &[chassis]), "desktop");
        }
    }

    #[test]
    fn portable_chassis_and_known_battery_keep_laptop() {
        for flag in [0, 1, 2, 4, 8, 9] {
            assert_eq!(classify(Some(flag), &[]), "laptop");
        }
        for chassis in [8, 9, 10, 11, 14, 30, 31, 32] {
            assert_eq!(classify(Some(255), &[chassis]), "laptop");
        }
        assert_eq!(classify(None, &[9, 7]), "desktop");
    }

    #[test]
    fn chassis_parser_accepts_only_bounded_integer_arrays() {
        assert_eq!(chassis_from_output(b"[7]"), Some(vec![7]));
        assert_eq!(chassis_from_output(b"[9,31]\r\n"), Some(vec![9, 31]));
        assert_eq!(chassis_from_output("\u{feff}[3]".as_bytes()), Some(vec![3]));
        for raw in [
            b"7".as_slice(),
            b"{\"ChassisTypes\":[7]}",
            b"[null]",
            b"[\"7\"]",
            b"[65536]",
        ] {
            assert!(chassis_from_output(raw).is_none());
        }
        assert!(chassis_from_output(&vec![b' '; 1025]).is_none());
        assert!(chassis_from_output(format!("[{}]", vec!["7"; 33].join(",")).as_bytes()).is_none());
    }
}
