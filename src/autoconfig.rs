//! Builds a config from whatever monitors are connected: their EDID models and the input codes
//! listed in their DDC/CI capabilities. Used on first run (or whenever no inputs are configured)
//! and by the Settings window's "Detect inputs" button.

use crate::config::InputEntry;
use crate::ddc::{self, DisplayInfo, INPUT_VCP};

pub const TOGGLE_HOTKEY: &str = "Ctrl+Alt+Shift+S";

/// Display names for input codes, sorted by code: "DisplayPort" instead of "DisplayPort-1" when the
/// monitor has only one DisplayPort input, and so on.
pub fn input_entries(codes: &[u32]) -> Vec<(u32, String)> {
    let mut codes: Vec<u32> = codes
        .iter()
        .copied()
        .filter(|code| (1..=0xFFFF).contains(code))
        .collect();
    codes.sort_unstable();
    codes.dedup();
    let names: Vec<String> = codes.iter().map(|c| ddc::input_name(*c)).collect();
    codes
        .iter()
        .zip(&names)
        .map(|(code, name)| {
            let short = name.strip_suffix("-1").filter(|base| {
                !names
                    .iter()
                    .any(|other| other != name && other.strip_suffix("-2") == Some(*base))
            });
            (*code, short.unwrap_or(name).to_string())
        })
        .collect()
}

/// "Ctrl+Alt+Shift+<n>" for the n-th input (1-9), None after that.
pub fn default_hotkey(index: usize) -> Option<String> {
    (index < 9).then(|| format!("Ctrl+Alt+Shift+{}", index + 1))
}

pub struct Detected {
    pub display: DisplayInfo,
    pub inputs: Vec<InputEntry>,
    /// Why the input list came from a fallback rather than the capabilities, if it did.
    pub note: Option<String>,
}

/// Detects one monitor and its inputs. Blocks for a few seconds (capabilities are slow to read).
pub fn detect_display(display: &DisplayInfo) -> Result<Detected, String> {
    let label = display.label();
    let mut bus = ddc::open(&display.bus).map_err(|e| format!("{label}: {e}"))?;
    let caps = bus.capabilities();
    let usable = caps
        .as_ref()
        .ok()
        .map(|caps| ddc::caps_inputs(caps))
        .filter(|codes| !input_entries(codes).is_empty());
    let (codes, note) = match usable {
        Some(codes) => (codes, None),
        None => {
            // No usable list: at least offer the input the monitor is showing now.
            let why = match &caps {
                Ok(_) => "The monitor's capabilities list no usable input codes".to_string(),
                Err(e) => format!("Could not read the monitor's capabilities ({e})"),
            };
            match bus.get_vcp(INPUT_VCP) {
                Ok(v) if !input_entries(&[v & 0xFF]).is_empty() => (
                    vec![v & 0xFF],
                    Some(format!("{why}; only the current input was added.")),
                ),
                Ok(_) => (Vec::new(), Some(format!("{why}; add inputs in Settings."))),
                Err(_) => (Vec::new(), Some(format!("{why}; add inputs in Settings."))),
            }
        }
    };
    let inputs = input_entries(&codes)
        .into_iter()
        .map(|(value, name)| InputEntry::new(name, value, None))
        .collect();
    Ok(Detected {
        display: display.clone(),
        inputs,
        note,
    })
}

/// Detects every DDC/CI-capable monitor. Capabilities are read in parallel because each monitor is
/// independent and can take a couple of seconds.
pub fn detect_all() -> Result<Vec<Detected>, String> {
    let displays = ddc::list_displays()?;
    let valid: Vec<&DisplayInfo> = displays.iter().filter(|d| d.valid).collect();
    if valid.is_empty() {
        return Err(if displays.is_empty() {
            "No monitor found on the graphics adapters' I2C buses".to_string()
        } else {
            "No monitor answers DDC/CI (enable DDC/CI in the monitor's on-screen menu)".to_string()
        });
    }
    let results: Vec<Result<Detected, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = valid
            .iter()
            .map(|display| scope.spawn(move || detect_display(display)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("Monitor detection failed".into())))
            .collect()
    });
    let mut detected = Vec::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(d) if !d.inputs.is_empty() => detected.push(d),
            Ok(d) => errors.push(format!("{}: no inputs were reported", d.display.label())),
            Err(e) => errors.push(e),
        }
    }
    if detected.is_empty() {
        return Err(format!(
            "No monitor could be set up{}",
            if errors.is_empty() {
                String::new()
            } else {
                format!(" ({})", errors.join("; "))
            }
        ));
    }
    Ok(detected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_order() {
        // AW3926QW capabilities list 60(1B 0F 11 12).
        let entries = input_entries(&[0x1B, 0x0F, 0x11, 0x12]);
        assert_eq!(
            entries,
            vec![
                (0x0F, "DisplayPort".to_string()),
                (0x11, "HDMI-1".to_string()),
                (0x12, "HDMI-2".to_string()),
                (0x1B, "USB-C".to_string()),
            ]
        );
        assert_eq!(input_entries(&[0x10, 0x0F])[0].1, "DisplayPort-1");
        // 0 is not a valid VCP input code and is dropped.
        assert_eq!(input_entries(&[0, 0x11]), vec![(0x11, "HDMI".into())]);
        assert_eq!(default_hotkey(0).as_deref(), Some("Ctrl+Alt+Shift+1"));
        assert_eq!(default_hotkey(9), None);
    }
}
