//! JSON config (the same schema on Linux and Windows) and the "Start with login" autostart entry.
//!
//! A config holds one or more monitors, each with its own inputs and "Next input" shortcut. Older
//! files had a single monitor at the top level; `ConfigWire` migrates them on load.

use crate::ddc::{self, Target};
use crate::hotkey::Hotkey;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const APP_ID: &str = "monitor-input-switcher";
pub const APP_NAME: &str = "Monitor Input Switcher";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InputEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: u32,
    #[serde(default)]
    pub hotkey: Option<String>,
    /// Whether "Next input" cycles through this input (untick inputs with nothing plugged in).
    /// Missing in older configs, which means included.
    #[serde(default = "included")]
    pub include_in_next: bool,
}

fn included() -> bool {
    true
}

impl InputEntry {
    pub fn new(name: impl Into<String>, value: u32, hotkey: Option<String>) -> Self {
        InputEntry {
            name: name.into(),
            value,
            hotkey,
            include_in_next: true,
        }
    }
}

/// The input "Next input" goes to: the first included input after the current one, in list order
/// and wrapping around. Inputs left out of the cycle are skipped, but switching away from one works.
pub fn next_input(inputs: &[InputEntry], current: Option<u32>) -> Result<&InputEntry, String> {
    if inputs.is_empty() {
        return Err("No inputs configured".into());
    }
    let start = current.and_then(|c| inputs.iter().position(|e| crate::ddc::matches(c, e.value)));
    let n = inputs.len();
    let index = match start {
        Some(s) => (1..=n)
            .map(|k| (s + k) % n)
            .find(|&i| inputs[i].include_in_next),
        None => (0..n).find(|&i| inputs[i].include_in_next),
    };
    index
        .map(|i| &inputs[i])
        .ok_or_else(|| "No input is included in \"Next input\" (see Settings)".into())
}

/// One configured monitor: what to match it by, its inputs and its own "Next input" shortcut.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MonitorConfig {
    pub name: Option<String>,
    pub device: Option<String>,
    pub inputs: Vec<InputEntry>,
    pub toggle_hotkey: Option<String>,
}

impl MonitorConfig {
    pub fn new(
        name: Option<String>,
        device: Option<String>,
        inputs: Vec<InputEntry>,
        toggle_hotkey: Option<String>,
    ) -> Self {
        MonitorConfig {
            name,
            device,
            inputs,
            toggle_hotkey,
        }
    }

    pub fn target(&self) -> Target {
        (self.name.clone(), self.device.clone())
    }

    /// The name shown in the tray and CLI, before duplicate names are disambiguated.
    pub fn label(&self, index: usize) -> String {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.device
                    .as_deref()
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("Monitor {}", index + 1))
    }
}

/// Whether two targets refer to the same physical monitor: same device when both have one,
/// otherwise the same (case-insensitive) name.
pub fn targets_conflict(a: &Target, b: &Target) -> bool {
    let device = |t: &Target| {
        t.1.as_deref()
            .filter(|d| ddc::is_device(d))
            .map(str::to_string)
    };
    match (device(a), device(b)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
        (None, None) => match (&a.0, &b.0) {
            (Some(a), Some(b)) => a.trim().eq_ignore_ascii_case(b.trim()),
            (None, None) => true,
            _ => false,
        },
        // One entry has a device and the other only a name. If the names agree, the name-only
        // entry can resolve to the same display, so treat it as a duplicate and ask the user to
        // disambiguate with a device.
        _ => match (&a.0, &b.0) {
            (Some(a), Some(b)) => {
                !a.trim().is_empty() && a.trim().eq_ignore_ascii_case(b.trim())
            }
            _ => false,
        },
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", from = "ConfigWire")]
pub struct AppConfig {
    pub monitors: Vec<MonitorConfig>,
    /// A notification after each switch.
    pub show_notifications: bool,
    /// A notification when the app starts, so it's clear it is running in the tray.
    pub show_startup_notification: bool,
}

/// Monitor-agnostic defaults: no monitors and no inputs yet. The app fills them in from the
/// connected monitors on first run (see autoconfig.rs).
impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            monitors: Vec::new(),
            show_notifications: true,
            show_startup_notification: true,
        }
    }
}

/// The on-disk shape, accepting both the current `Monitors` list and the old single-monitor fields.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ConfigWire {
    monitors: Vec<MonitorConfig>,
    monitor_name: Option<String>,
    monitor_device: Option<String>,
    inputs: Vec<InputEntry>,
    toggle_hotkey: Option<String>,
    show_notifications: bool,
    show_startup_notification: bool,
}

impl Default for ConfigWire {
    fn default() -> Self {
        ConfigWire {
            monitors: Vec::new(),
            monitor_name: None,
            monitor_device: None,
            inputs: Vec::new(),
            toggle_hotkey: None,
            show_notifications: true,
            show_startup_notification: true,
        }
    }
}

impl From<ConfigWire> for AppConfig {
    fn from(wire: ConfigWire) -> Self {
        let mut monitors = wire.monitors;
        // Before multi-monitor support the settings lived at the top level. Migrate them into a
        // single monitor entry; the file is rewritten in the new shape on the next save.
        if monitors.is_empty()
            && (!wire.inputs.is_empty()
                || wire.monitor_name.is_some()
                || wire.monitor_device.is_some())
        {
            monitors.push(MonitorConfig::new(
                wire.monitor_name,
                wire.monitor_device,
                wire.inputs,
                wire.toggle_hotkey,
            ));
        }
        AppConfig {
            monitors,
            show_notifications: wire.show_notifications,
            show_startup_notification: wire.show_startup_notification,
        }
    }
}

/// This executable. After an upgrade replaced the file, /proc/self/exe reads "<path> (deleted)":
/// the new binary at the same path is what we want then.
pub fn self_exe() -> PathBuf {
    match std::env::current_exe() {
        Ok(p) => {
            let s = p.display().to_string();
            s.strip_suffix(" (deleted)").map(PathBuf::from).unwrap_or(p)
        }
        Err(_) => PathBuf::from(APP_ID),
    }
}

/// `~/.config/monitor-input-switcher/config.json` on Linux,
/// `%APPDATA%\MonitorInputSwitcher\config.json` on Windows.
pub fn config_path() -> PathBuf {
    crate::platform::config_dir().join("config.json")
}

pub use crate::platform::autostart;

pub enum Loaded {
    Ok(AppConfig),
    FirstRun(AppConfig),
    /// The file exists but could not be read; defaults are used.
    Error(AppConfig, String),
}

impl AppConfig {
    pub fn load() -> Loaded {
        let path = config_path();
        match fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Loaded::FirstRun(AppConfig::default()),
            Err(e) => Loaded::Error(
                AppConfig::default(),
                format!("Could not read {}: {e}", path.display()),
            ),
            Ok(text) => match serde_json::from_str(&text) {
                Ok(cfg) => Loaded::Ok(cfg),
                Err(e) => Loaded::Error(
                    AppConfig::default(),
                    format!("Could not read {}: {e}. Defaults are used.", path.display()),
                ),
            },
        }
    }

    /// The config, or defaults if it is missing or unreadable.
    pub fn load_or_default() -> AppConfig {
        match Self::load() {
            Loaded::Ok(c) | Loaded::FirstRun(c) | Loaded::Error(c, _) => c,
        }
    }

    fn to_json(&self) -> io::Result<String> {
        let mut text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        text.push('\n');
        Ok(text)
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_path();
        fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, self.to_json()?)?;
        fs::rename(&tmp, &path)
    }

    /// Writes the settings to a file another machine can import (same format as config.json).
    pub fn export(&self, path: &Path) -> Result<(), String> {
        self.to_json()
            .and_then(|text| fs::write(path, text))
            .map_err(|e| format!("Could not write {}: {e}", path.display()))
    }

    /// Reads settings exported by `export` (or a copy of config.json), and checks them.
    pub fn import(path: &Path) -> Result<AppConfig, String> {
        let invalid = |e: String| format!("{} is not a valid settings file: {e}", path.display());
        let read = |e: io::Error| format!("Could not read {}: {e}", path.display());
        // A settings file is a few hundred bytes; don't load a big file picked by mistake.
        if fs::metadata(path).map_err(read)?.len() > 1024 * 1024 {
            return Err(invalid("it is larger than 1 MB".into()));
        }
        let text = fs::read_to_string(path).map_err(read)?;
        Self::from_export(&text).map_err(invalid)
    }

    fn from_export(text: &str) -> Result<AppConfig, String> {
        // Windows PowerShell and older Notepad start UTF-8 files with a byte order mark.
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        // Every field has a default, so any JSON object would parse: require the fields that
        // carry the settings rather than silently filling them in.
        match value
            .get("Monitors")
            .and_then(|v| v.as_array())
            .filter(|m| !m.is_empty())
        {
            Some(monitors) => {
                for (i, monitor) in monitors.iter().enumerate() {
                    let inputs = monitor
                        .get("Inputs")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| format!("monitor {} has no \"Inputs\" list", i + 1))?;
                    for (j, input) in inputs.iter().enumerate() {
                        for field in ["Name", "Value"] {
                            if input.get(field).is_none() {
                                return Err(format!(
                                    "monitor {} input {} has no \"{field}\"",
                                    i + 1,
                                    j + 1
                                ));
                            }
                        }
                    }
                }
            }
            None => {
                // Old single-monitor export (or a config.json from before multi-monitor support).
                let inputs = value
                    .get("Inputs")
                    .and_then(|v| v.as_array())
                    .ok_or("no \"Monitors\" or \"Inputs\" list")?;
                for (i, input) in inputs.iter().enumerate() {
                    for field in ["Name", "Value"] {
                        if input.get(field).is_none() {
                            return Err(format!("input {} has no \"{field}\"", i + 1));
                        }
                    }
                }
            }
        }
        let mut config: AppConfig = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let blank = |h: &mut Option<String>| {
            if h.as_deref().is_some_and(|t| t.trim().is_empty()) {
                *h = None;
            }
        };
        for monitor in &mut config.monitors {
            monitor.inputs.iter_mut().for_each(|e| blank(&mut e.hotkey));
            blank(&mut monitor.toggle_hotkey);
        }
        config.check()?;
        Ok(config)
    }

    /// Disambiguated labels for the tray, CLI and error messages.
    pub fn monitor_labels(&self) -> Vec<String> {
        let bases: Vec<String> = self
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| m.label(i))
            .collect();
        bases
            .iter()
            .enumerate()
            .map(|(i, base)| {
                if bases.iter().filter(|b| *b == base).count() > 1 {
                    match self.monitors[i].device.as_deref().filter(|d| !d.trim().is_empty()) {
                        Some(device) => format!("{base} ({device})"),
                        None => format!("{base} (#{})", i + 1),
                    }
                } else {
                    base.clone()
                }
            })
            .collect()
    }

    /// A short list of configured monitors for error messages.
    pub fn monitors_summary(&self) -> String {
        if self.monitors.is_empty() {
            return "No monitors are configured.".into();
        }
        let labels = self.monitor_labels();
        labels
            .iter()
            .enumerate()
            .map(|(i, label)| format!("{}. {label}", i + 1))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Finds a configured monitor by 1-based number, name or device.
    pub fn find_monitor(&self, selector: &str) -> Result<usize, String> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err("No monitor selected".into());
        }
        if let Ok(n) = selector.parse::<usize>() {
            if (1..=self.monitors.len()).contains(&n) {
                return Ok(n - 1);
            }
            return Err(format!(
                "Monitor {n} does not exist. Configured monitors: {}",
                self.monitors_summary()
            ));
        }
        let labels = self.monitor_labels();
        let exact: Vec<usize> = (0..self.monitors.len())
            .filter(|&i| {
                let m = &self.monitors[i];
                m.name
                    .as_deref()
                    .is_some_and(|n| n.trim().eq_ignore_ascii_case(selector))
                    || m.device
                        .as_deref()
                        .is_some_and(|d| d.trim().eq_ignore_ascii_case(selector))
                    || labels[i].eq_ignore_ascii_case(selector)
            })
            .collect();
        match exact.len() {
            1 => return Ok(exact[0]),
            2.. => return Err(Self::ambiguous_monitor(selector, &labels, &exact)),
            _ => {}
        }
        let low = selector.to_ascii_lowercase();
        let fuzzy: Vec<usize> = (0..self.monitors.len())
            .filter(|&i| {
                let m = &self.monitors[i];
                m.name
                    .as_deref()
                    .is_some_and(|n| n.to_ascii_lowercase().contains(&low))
                    || m.device
                        .as_deref()
                        .is_some_and(|d| d.to_ascii_lowercase().contains(&low))
                    || labels[i].to_ascii_lowercase().contains(&low)
            })
            .collect();
        match fuzzy.len() {
            1 => Ok(fuzzy[0]),
            0 => Err(format!(
                "No monitor matches {selector:?}. Configured monitors: {}",
                self.monitors_summary()
            )),
            _ => Err(Self::ambiguous_monitor(selector, &labels, &fuzzy)),
        }
    }

    fn ambiguous_monitor(selector: &str, labels: &[String], matches: &[usize]) -> String {
        let options: Vec<String> = matches
            .iter()
            .map(|&i| format!("{}. {}", i + 1, labels[i]))
            .collect();
        format!(
            "Monitor {selector:?} matches more than one: {}. Use the number or device.",
            options.join(", ")
        )
    }

    /// The configured monitor index for a target, if it is configured.
    pub fn monitor_for_target(&self, target: &Option<Target>) -> Option<usize> {
        let target = target.clone()?;
        self.monitors
            .iter()
            .position(|m| ddc::target_matches(&m.target(), &target))
    }

    /// Checks what a working config needs: at least one monitor, each with an input; a target for
    /// every monitor when there are several; no two monitors on the same display; and valid,
    /// unique shortcuts.
    pub fn check(&self) -> Result<(), String> {
        if self.monitors.is_empty() {
            return Err("Add at least one monitor.".into());
        }
        let labels = self.monitor_labels();
        if self.monitors.len() > 1 {
            for (i, monitor) in self.monitors.iter().enumerate() {
                let has_name = monitor
                    .name
                    .as_deref()
                    .is_some_and(|n| !n.trim().is_empty());
                let has_device = monitor
                    .device
                    .as_deref()
                    .is_some_and(|d| !d.trim().is_empty());
                if !has_name && !has_device {
                    return Err(format!(
                        "{}: choose a display in Settings (only one monitor may be left on automatic).",
                        labels[i]
                    ));
                }
            }
        }
        for i in 0..self.monitors.len() {
            for j in (i + 1)..self.monitors.len() {
                if targets_conflict(&self.monitors[i].target(), &self.monitors[j].target()) {
                    return Err(format!(
                        "{} and {} refer to the same display; give each a different display or device.",
                        labels[i], labels[j]
                    ));
                }
            }
        }
        let mut shortcuts: Vec<(String, &Option<String>)> = Vec::new();
        for (i, monitor) in self.monitors.iter().enumerate() {
            if monitor.inputs.is_empty() {
                return Err(format!("{}: add at least one input.", labels[i]));
            }
            for (j, e) in monitor.inputs.iter().enumerate() {
                let name = e.name.trim();
                if name.is_empty() {
                    return Err(format!("{}: input {} has no name.", labels[i], j + 1));
                }
                if !(1..=0xFFFF).contains(&e.value) {
                    return Err(format!(
                        "{}: {name}: input code {} is out of range (1 to 65535).",
                        labels[i], e.value
                    ));
                }
                shortcuts.push((format!("{}: {name}", labels[i]), &e.hotkey));
            }
            shortcuts.push((format!("{}: Next input", labels[i]), &monitor.toggle_hotkey));
        }
        let mut seen: Vec<Hotkey> = Vec::new();
        for (owner, text) in shortcuts {
            let Some(text) = text.as_deref().filter(|t| !t.trim().is_empty()) else {
                continue;
            };
            let hotkey = Hotkey::parse(text)
                .ok_or_else(|| format!("{owner}: shortcut {text:?} is not valid."))?;
            if seen.contains(&hotkey) {
                return Err(format!(
                    "The shortcut {hotkey} is assigned more than once ({owner})."
                ));
            }
            seen.push(hotkey);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Vec<InputEntry> {
        let mut v = vec![
            InputEntry::new("DisplayPort", 15, None),
            InputEntry::new("HDMI-1", 17, None),
            InputEntry::new("HDMI-2", 18, None),
            InputEntry::new("USB-C", 27, None),
        ];
        v[1].include_in_next = false;
        v[2].include_in_next = false;
        v
    }

    fn monitor(name: &str, device: &str, inputs: Vec<InputEntry>) -> MonitorConfig {
        MonitorConfig::new(
            Some(name.into()),
            Some(device.into()),
            inputs,
            Some("Ctrl+Alt+Shift+S".into()),
        )
    }

    /// A device id that `platform::ddc::is_device` accepts on the current OS.
    #[cfg(windows)]
    fn device(n: u8) -> String {
        format!(r"\\.\DISPLAY{n}")
    }

    #[cfg(target_os = "linux")]
    fn device(n: u8) -> String {
        format!("/dev/i2c-{}", 20 + n)
    }

    /// A device id from the other supported OS; it must be ignored by target matching.
    #[cfg(windows)]
    fn other_os_device() -> String {
        "/dev/i2c-20".into()
    }

    #[cfg(target_os = "linux")]
    fn other_os_device() -> String {
        r"\\.\DISPLAY1".into()
    }

    #[test]
    fn next_skips_excluded_inputs() {
        let v = inputs();
        assert_eq!(next_input(&v, Some(0x1B0F)).unwrap().name, "USB-C"); // DP -> USB-C, skipping HDMI
        assert_eq!(next_input(&v, Some(27)).unwrap().name, "DisplayPort"); // wraps around
        assert_eq!(next_input(&v, Some(17)).unwrap().name, "USB-C"); // leaving an excluded input works
        assert_eq!(next_input(&v, None).unwrap().name, "DisplayPort"); // unknown current: first included
        let mut none = v.clone();
        none.iter_mut().for_each(|e| e.include_in_next = false);
        assert!(next_input(&none, Some(15)).is_err());
        assert!(next_input(&[], None).is_err());
    }

    #[test]
    fn legacy_config_migrates_to_one_monitor() {
        let cfg: AppConfig = serde_json::from_str(
            r#"{"MonitorName":"AW3926QW","MonitorDevice":null,
                "Inputs":[{"Name":"DP","Value":15,"Hotkey":"Ctrl+Alt+Shift+1"}],
                "ToggleHotkey":"Ctrl+Alt+Shift+S"}"#,
        )
        .unwrap();
        assert_eq!(cfg.monitors.len(), 1);
        assert_eq!(cfg.monitors[0].name.as_deref(), Some("AW3926QW"));
        assert_eq!(cfg.monitors[0].inputs[0].name, "DP");
        assert_eq!(
            cfg.monitors[0].toggle_hotkey.as_deref(),
            Some("Ctrl+Alt+Shift+S")
        );
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""Monitors""#));
        assert!(!json.contains("MonitorName"));
    }

    #[test]
    fn old_configs_include_every_input() {
        let cfg: AppConfig =
            serde_json::from_str(r#"{"Inputs":[{"Name":"DP","Value":15,"Hotkey":null}]}"#).unwrap();
        assert!(cfg.monitors[0].inputs[0].include_in_next);
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""IncludeInNext":true"#));
    }

    #[test]
    fn export_round_trip() {
        let mut cfg = AppConfig::default();
        cfg.monitors = vec![
            monitor("AW3926QW", "/dev/i2c-20", inputs()),
            MonitorConfig::new(
                Some("U2720Q".into()),
                Some("/dev/i2c-21".into()),
                vec![InputEntry::new("HDMI-1", 17, None)],
                None,
            ),
        ];
        let text = cfg.to_json().unwrap();
        assert_eq!(AppConfig::from_export(&text).unwrap(), cfg);
        assert_eq!(AppConfig::from_export(&format!("\u{FEFF}{text}")).unwrap(), cfg);
    }

    #[test]
    fn legacy_export_imports() {
        let text = r#"{"MonitorName":"AW3926QW","Inputs":[{"Name":"DP","Value":15}],
                       "ToggleHotkey":"Ctrl+Alt+Shift+S"}"#;
        let cfg = AppConfig::from_export(text).unwrap();
        assert_eq!(cfg.monitors.len(), 1);
        assert_eq!(cfg.monitors[0].name.as_deref(), Some("AW3926QW"));
        assert_eq!(
            cfg.monitors[0].toggle_hotkey.as_deref(),
            Some("Ctrl+Alt+Shift+S")
        );
    }

    #[test]
    fn import_rejects_bad_files() {
        let bad = [
            "",
            "not json",
            "[1, 2]",
            "{}",
            r#"{"Inputs": "DP"}"#,
            r#"{"Inputs": []}"#,
            r#"{"Inputs": [{}]}"#,
            r#"{"Inputs": [{"Name": "DP"}]}"#,
            r#"{"Inputs": [{"Value": 15}]}"#,
            r#"{"Inputs": [{"Name": " ", "Value": 15}]}"#,
            r#"{"Inputs": [{"Name": "DP", "Value": "15"}]}"#,
            r#"{"Inputs": [{"Name": "DP", "Value": -1}]}"#,
            r#"{"Inputs": [{"Name": "DP", "Value": 0}]}"#,
            r#"{"Inputs": [{"Name": "DP", "Value": 70000}]}"#,
            r#"{"Inputs": [{"Name": "DP", "Value": 15, "Hotkey": "Ctrl+Banana"}]}"#,
            r#"{"Inputs": [{"Name": "DP", "Value": 15, "Hotkey": "Ctrl+1"},
                           {"Name": "HDMI", "Value": 17, "Hotkey": "ctrl+1"}]}"#,
            r#"{"Inputs": [{"Name": "DP", "Value": 15, "Hotkey": "Ctrl+1"}], "ToggleHotkey": "Ctrl+1"}"#,
            r#"{"Inputs": [{"Name": "DP", "Value": 15}], "ShowNotifications": "yes"}"#,
            r#"{"Inputs": [{"Name": "DP", "Value": 15}"#,
            r#"{"Monitors": "x"}"#,
            r#"{"Monitors": []}"#,
            r#"{"Monitors": [{}]}"#,
            r#"{"Monitors": [{"Inputs": []}]}"#,
            r#"{"Monitors": [{"Inputs": [{"Name": "DP"}]}]}"#,
            r#"{"Monitors": [{"Inputs": [{"Name": "DP", "Value": 15}]},
                             {"Inputs": [{"Name": "HDMI", "Value": 17}]}]}"#,
        ];
        for text in bad {
            assert!(AppConfig::from_export(text).is_err(), "accepted: {text}");
        }
        let duplicate_device = serde_json::json!({
            "Monitors": [
                {"Name": "A", "Device": device(1), "Inputs": [{"Name": "DP", "Value": 15}]},
                {"Name": "B", "Device": device(1), "Inputs": [{"Name": "HDMI", "Value": 17}]},
            ]
        })
        .to_string();
        assert!(AppConfig::from_export(&duplicate_device).is_err());
        let duplicate_hotkey = serde_json::json!({
            "Monitors": [
                {"Name": "A", "Device": device(1), "Inputs": [{"Name": "DP", "Value": 15, "Hotkey": "Ctrl+1"}]},
                {"Name": "B", "Device": device(2), "Inputs": [{"Name": "HDMI", "Value": 17, "Hotkey": "Ctrl+1"}]},
            ]
        })
        .to_string();
        assert!(AppConfig::from_export(&duplicate_hotkey).is_err());
        // Minimal and lenient: other fields default, unknown fields and blank shortcuts are ignored.
        let ok = AppConfig::from_export(
            r#"{"Inputs": [{"Name": "DP", "Value": 15, "Hotkey": ""}], "Extra": 1}"#,
        )
        .unwrap();
        assert_eq!(ok.monitors[0].inputs[0].name, "DP");
        assert_eq!(ok.monitors[0].inputs[0].hotkey, None);
        assert!(ok.show_notifications);
        let ok = AppConfig::from_export(
            r#"{"Monitors": [{"Name": "A", "Device": "/dev/i2c-20",
                               "Inputs": [{"Name": "DP", "Value": 15, "Hotkey": ""}]}]}"#,
        )
        .unwrap();
        assert_eq!(ok.monitors.len(), 1);
        assert_eq!(ok.monitors[0].inputs[0].hotkey, None);
    }

    #[test]
    fn checks_monitors_and_shortcuts() {
        let mut cfg = AppConfig::default();
        cfg.monitors = vec![
            monitor(
                "A",
                &device(1),
                vec![InputEntry::new("DP", 15, Some("Ctrl+1".into()))],
            ),
            monitor(
                "B",
                &device(2),
                vec![InputEntry::new("HDMI", 17, Some("Ctrl+1".into()))],
            ),
        ];
        assert!(cfg.check().unwrap_err().contains("assigned more than once"));
        cfg.monitors[1].inputs[0].hotkey = Some("Ctrl+2".into());
        cfg.monitors[1].toggle_hotkey = None;
        cfg.monitors[0].toggle_hotkey = None;
        assert_eq!(cfg.check(), Ok(()));
        // Two entries on the same physical device.
        cfg.monitors[1].device = Some(device(1));
        assert!(cfg.check().unwrap_err().contains("same display"));
        // More than one monitor needs an explicit target.
        cfg.monitors[1].device = None;
        cfg.monitors[1].name = None;
        assert!(cfg.check().unwrap_err().contains("choose a display"));
    }

    #[test]
    fn finds_monitors_by_number_name_and_device() {
        let mut cfg = AppConfig::default();
        cfg.monitors = vec![
            monitor("AW3926QW", "/dev/i2c-20", vec![InputEntry::new("DP", 15, None)]),
            monitor("AW3926QW", "/dev/i2c-21", vec![InputEntry::new("DP", 15, None)]),
            monitor("U2720Q", "/dev/i2c-22", vec![InputEntry::new("HDMI", 17, None)]),
        ];
        assert_eq!(cfg.find_monitor("1"), Ok(0));
        assert_eq!(cfg.find_monitor("U2720Q"), Ok(2));
        assert_eq!(cfg.find_monitor("/dev/i2c-21"), Ok(1));
        // Ambiguous name: both AW3926QW entries match.
        assert!(cfg.find_monitor("AW3926QW").unwrap_err().contains("more than one"));
        assert!(cfg.find_monitor("9").unwrap_err().contains("does not exist"));
        let labels = cfg.monitor_labels();
        assert_eq!(labels[0], "AW3926QW (/dev/i2c-20)");
        assert_eq!(labels[1], "AW3926QW (/dev/i2c-21)");
        assert_eq!(labels[2], "U2720Q");
    }

    #[test]
    fn known_targets_match() {
        assert!(targets_conflict(
            &(Some("A".into()), Some(device(1))),
            &(Some("B".into()), Some(device(1)))
        ));
        assert!(targets_conflict(&(None, None), &(None, None)));
        assert!(!targets_conflict(
            &(Some("A".into()), None),
            &(Some("B".into()), None)
        ));
        // A name-only entry can resolve to the same display as a device entry with that name.
        assert!(targets_conflict(
            &(Some("A".into()), Some(device(1))),
            &(Some("A".into()), None)
        ));
        assert!(!targets_conflict(
            &(Some("A".into()), Some(device(1))),
            &(Some("B".into()), None)
        ));
        assert!(ddc::target_matches(
            &(Some("A".into()), Some(device(1))),
            &(Some("A".into()), Some(device(1)))
        ));
        // A device from the other OS is ignored.
        assert!(ddc::target_matches(
            &(Some("A".into()), Some(other_os_device())),
            &(Some("A".into()), None)
        ));
    }
}
