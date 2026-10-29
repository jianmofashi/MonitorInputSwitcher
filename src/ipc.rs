//! The protocol between the CLI / Settings window and the running app: one JSON line each way
//! over the platform's local transport (Unix socket on Linux, named pipe on Windows).

use crate::config::{AppConfig, InputEntry};
use crate::ddc::{DisplayInfo, Target};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Request {
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<String>,
    /// For "set"/"get" against a monitor that is not saved yet (Settings window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<(Option<String>, Option<String>)>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displays: Option<Vec<DisplayInfo>>,
    /// Raw DDC/CI capabilities string ("caps").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<String>,
}

impl Response {
    pub fn ok(message: impl Into<String>) -> Self {
        Response {
            ok: true,
            message: message.into(),
            ..Default::default()
        }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Response {
            ok: false,
            message: message.into(),
            ..Default::default()
        }
    }
}

/// Sends a request to the running instance; None if no instance is listening.
pub fn send(req: &Request) -> Option<Response> {
    let mut stream = crate::platform::ipc::connect()?;
    let mut line = serde_json::to_string(req).ok()?;
    line.push('\n');
    if stream.write_all(line.as_bytes()).is_err() {
        return Some(Response::err("Could not talk to the running instance"));
    }
    let mut reply = String::new();
    let _ = BufReader::new(stream).read_line(&mut reply);
    Some(
        serde_json::from_str(&reply)
            .unwrap_or_else(|_| Response::err("No reply from the running instance")),
    )
}

/// Serves one connection: reads a request, lets `handle` produce the response, writes it back.
pub fn serve<S: Read + Write>(mut stream: S, handle: impl FnOnce(Request) -> Response) {
    let mut line = String::new();
    if BufReader::new(&mut stream).read_line(&mut line).is_err() {
        return;
    }
    let resp = match serde_json::from_str::<Request>(&line) {
        Ok(req) => handle(req),
        Err(e) => Response::err(format!("Bad request: {e}")),
    };
    let mut out = serde_json::to_string(&resp).unwrap_or_default();
    out.push('\n');
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.flush();
}

pub fn send_cmd(cmd: &str) -> Option<Response> {
    send(&Request {
        cmd: cmd.into(),
        ..Default::default()
    })
}

/// "27" or "0x1B" as an input code.
pub fn parse_value(arg: &str) -> Option<u32> {
    let arg = arg.trim();
    arg.strip_prefix("0x")
        .or_else(|| arg.strip_prefix("0X"))
        .map_or_else(|| arg.parse().ok(), |h| u32::from_str_radix(h, 16).ok())
        .filter(|v| *v <= 0xFFFF)
}

/// An input by name (case-insensitive) or by VCP value.
pub fn find_input<'a>(inputs: &'a [InputEntry], arg: &str) -> Option<&'a InputEntry> {
    let arg = arg.trim();
    inputs
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(arg))
        .or_else(|| {
            let value = parse_value(arg)?;
            inputs.iter().find(|e| e.value == value)
        })
}

pub fn inputs_text(inputs: &[InputEntry]) -> String {
    if inputs.is_empty() {
        return "none".into();
    }
    inputs
        .iter()
        .map(|e| format!("{} ({})", e.name, e.value))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn describe_input(config: &AppConfig, target: &Option<Target>, value: u32) -> String {
    let inputs: &[InputEntry] = match config.monitor_for_target(target) {
        Some(i) => &config.monitors[i].inputs,
        None => &[],
    };
    let name = if inputs.is_empty() {
        config
            .monitors
            .iter()
            .flat_map(|m| m.inputs.iter())
            .find(|e| crate::ddc::matches(value, e.value))
    } else {
        inputs
            .iter()
            .find(|e| crate::ddc::matches(value, e.value))
    };
    let name = name.map(|e| format!(" = {}", e.name)).unwrap_or_default();
    if let Some(status) = crate::ddc::status_name(value) {
        // AOC monitors report DVI/HDMI as one read-only status, not as an input code.
        format!("{value} (0x{value:02X}), {status}")
    } else {
        format!("{value} (0x{value:02X}){name}")
    }
}

/// The configured monitors: default marker, number, label, device and input count.
pub fn monitors_text(config: &AppConfig) -> String {
    if config.monitors.is_empty() {
        return "No monitors configured".into();
    }
    config
        .monitors
        .iter()
        .enumerate()
        .map(|(i, monitor)| {
            let label = monitor.label(i);
            let device = monitor
                .device
                .as_deref()
                .filter(|d| !d.trim().is_empty())
                .filter(|d| *d != label)
                .map(|d| format!("  ({d})"))
                .unwrap_or_default();
            let n = monitor.inputs.len();
            format!(
                "{} {}. {label}{device}  {n} input{}",
                if i == 0 { "*" } else { " " },
                i + 1,
                if n == 1 { "" } else { "s" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn displays_text(displays: &[DisplayInfo]) -> String {
    if displays.is_empty() {
        return "No displays found".into();
    }
    displays
        .iter()
        .map(DisplayInfo::label)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Human-readable summary of a capabilities string: model, input codes with their MCCS names, raw text.
pub fn caps_text(caps: &str) -> String {
    let model = crate::ddc::caps_model(caps).unwrap_or_else(|| "(not reported)".into());
    let inputs = crate::ddc::caps_inputs(caps);
    let inputs = if inputs.is_empty() {
        "  (none listed)".to_string()
    } else {
        inputs
            .iter()
            .map(|v| format!("  {v:>3} (0x{v:02X})  {}", crate::ddc::input_name(*v)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!("Model: {model}\nInputs (VCP 0x60):\n{inputs}\nRaw: {caps}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MonitorConfig;

    fn config() -> AppConfig {
        AppConfig {
            monitors: vec![
                MonitorConfig::new(
                    Some("A".into()),
                    Some("/dev/i2c-20".into()),
                    vec![InputEntry::new("DisplayPort", 15, None)],
                    None,
                ),
                MonitorConfig::new(
                    Some("B".into()),
                    Some("/dev/i2c-21".into()),
                    vec![InputEntry::new("HDMI-1", 17, None)],
                    None,
                ),
            ],
            show_notifications: true,
            show_startup_notification: true,
        }
    }

    #[test]
    fn describes_the_selected_monitor() {
        let config = config();
        let target = Some((Some("B".into()), Some("/dev/i2c-21".into())));
        assert!(describe_input(&config, &target, 17).contains("= HDMI-1"));
        assert!(!describe_input(&config, &target, 15).contains("= DisplayPort"));
    }

    #[test]
    fn describes_aoc_status() {
        let config = config();
        let target = Some((Some("A".into()), Some("/dev/i2c-20".into())));
        let text = describe_input(&config, &target, 0x0300);
        assert!(text.contains("DVI/HDMI"), "{text}");
        assert!(!text.contains("DVI-1"), "{text}");
    }

    #[test]
    fn lists_configured_monitors() {
        let text = monitors_text(&config());
        assert!(text.starts_with("* 1. A  (/dev/i2c-20)  1 input\n"));
        assert!(text.contains("2. B  (/dev/i2c-21)  1 input"));
        assert_eq!(monitors_text(&AppConfig::default()), "No monitors configured");
    }
}
