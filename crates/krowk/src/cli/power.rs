//! Whether this machine has a battery: a laptop, by the rule the always-on
//! step uses for its default (R-EASE-3) — yes on a machine with none, no on
//! one with. A wrong guess only changes the default of a question.
//!
//! Each OS has a reader that is a pure function over what that OS says, so
//! it is tested on any machine: the entries of Linux's power-supply class,
//! macOS's `pmset` output, and Windows' system power status. `has_battery`
//! only fetches the input of the one this build runs on.

/// Linux: the names in `/sys/class/power_supply`. A laptop's battery is a
/// `BAT*` entry; a mouse's or a phone's shows as another name (`hidpp_…`,
/// `ucsi-…`) and a mains adapter as `AC`/`ADP*`.
pub(super) fn linux_has_battery(entries: &[String]) -> bool {
    entries.iter().any(|name| name.starts_with("BAT"))
}

/// macOS: the output of `pmset -g batt`. A laptop lists an `InternalBattery`
/// and draws from `Battery Power` or `AC Power`; a desktop says only
/// `AC Power`.
pub(super) fn macos_has_battery(pmset: &str) -> bool {
    pmset.contains("InternalBattery") || pmset.contains("Battery Power")
}

/// Windows: the fields of `SYSTEM_POWER_STATUS` the rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct WindowsPowerStatus {
    /// `BatteryFlag`: a bit set (high 1, low 2, critical 4, charging 8), 128
    /// for no system battery, 255 for unknown.
    pub battery_flag: u8,
}

/// Windows: a battery unless the status says there is none (128). Unknown
/// (255) is read as none: asked on a desktop that cannot say, "no battery"
/// is the likelier.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn windows_has_battery(status: &WindowsPowerStatus) -> bool {
    !matches!(status.battery_flag, 128 | 255)
}

/// What this machine says, read now. A reader that cannot read — no sysfs,
/// no `pmset` — says there is none.
pub(super) fn has_battery() -> bool {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("pmset").args(["-g", "batt"]).stdin(std::process::Stdio::null()).output();
        return out.is_ok_and(|o| macos_has_battery(&String::from_utf8_lossy(&o.stdout)));
    }
    let names: Vec<String> = std::fs::read_dir("/sys/class/power_supply").into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    linux_has_battery(&names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn linux_a_bat_entry_is_a_laptop_and_other_supplies_are_not() {
        assert!(linux_has_battery(&names(&["AC", "BAT0"])));
        assert!(linux_has_battery(&names(&["BAT1"])));
        assert!(!linux_has_battery(&names(&["AC", "ADP1"])));
        assert!(!linux_has_battery(&names(&["hidpp_battery_0", "ucsi-source-psy-USBC000:001"])));
        assert!(!linux_has_battery(&[]));
    }

    #[test]
    fn macos_an_internal_battery_is_a_laptop_and_ac_alone_is_not() {
        let laptop = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=4653155)\t87%; discharging; 5:12 remaining present: true\n";
        let plugged = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=4653155)\t100%; charged; 0:00 remaining present: true\n";
        let desktop = "Now drawing from 'AC Power'\n";
        assert!(macos_has_battery(laptop));
        assert!(macos_has_battery(plugged));
        assert!(!macos_has_battery(desktop));
        assert!(!macos_has_battery(""));
    }

    #[test]
    fn windows_no_system_battery_is_a_desktop() {
        assert!(windows_has_battery(&WindowsPowerStatus { battery_flag: 1 }));
        assert!(windows_has_battery(&WindowsPowerStatus { battery_flag: 8 }));
        assert!(windows_has_battery(&WindowsPowerStatus { battery_flag: 9 }));
        assert!(!windows_has_battery(&WindowsPowerStatus { battery_flag: 128 }));
        assert!(!windows_has_battery(&WindowsPowerStatus { battery_flag: 255 }));
    }
}
