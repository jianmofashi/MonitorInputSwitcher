//! Monitor Input Switcher: switch the monitor input over DDC/CI, from the tray or a global
//! shortcut, on Linux (KDE Plasma) and Windows.
//!
//! No arguments starts the tray app. Subcommands control the running app, or act directly on the
//! monitor when it isn't running.

// A tray app: no console window on Windows (CLI commands attach to the parent's console).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod autoconfig;
mod config;
mod daemon;
mod ddc;
mod hotkey;
mod ipc;
mod platform;
mod settings;

use config::{APP_ID, APP_NAME, AppConfig, MonitorConfig};
use ddc::Target;
use ipc::Request;

const USAGE: &str = "\
Usage: monitor-input-switcher [COMMAND] [OPTIONS]

Without a command, starts the tray app (only one instance runs).

Commands:
  switch <INPUT>   switch to an input, by name or input code (e.g. \"USB-C\", HDMI-1, 27, 0x11)
  next             switch to the next configured input
  get              print the current input
  list             list detected displays
  monitors         list configured monitors
  capabilities     show the monitor's model and the input codes it accepts
  settings         open the Settings window of the running app
  export <FILE>    save the settings to a file
  import <FILE>    replace the settings with ones saved by export
  quit             stop the running app
  help             show this help

Options (switch, next, get, capabilities):
  -m, --monitor <N|NAME|DEVICE>  use that configured monitor (default: the first one)";

/// Everything except the command and its one positional argument: `-m/--monitor <value>`.
fn parse_args(raw: Vec<String>) -> Result<(String, String, Option<String>), String> {
    let mut positional = Vec::new();
    let mut monitor = None;
    let mut args = raw.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-m" | "--monitor" => {
                monitor = Some(args.next().ok_or("--monitor needs a value")?);
            }
            "-h" | "--help" => positional.push("help".into()),
            _ => {
                if let Some(value) = arg.strip_prefix("--monitor=") {
                    monitor = Some(value.to_string());
                } else if arg.starts_with('-') && arg != "-" {
                    return Err(format!("Unknown option {arg}"));
                } else {
                    positional.push(arg);
                }
            }
        }
    }
    let cmd = positional.first().cloned().unwrap_or_else(|| "run".into());
    let arg = positional.get(1).cloned().unwrap_or_default();
    if positional.len() > 2 {
        return Err("Too many arguments".into());
    }
    Ok((cmd, arg, monitor))
}

/// The configured monitor a CLI command works on, or None for the legacy "whatever answers" mode.
fn select_target(config: &AppConfig, selector: Option<&str>) -> Result<Option<Target>, String> {
    match selector {
        Some(selector) => config
            .find_monitor(selector)
            .map(|i| Some(config.monitors[i].target())),
        None => Ok(config.monitors.first().map(MonitorConfig::target)),
    }
}

/// `export <FILE>` / `import <FILE>`: copies config.json out, or replaces it and has the running
/// app (if any) reload it.
fn transfer(cmd: &str, file: &str) -> Result<String, String> {
    let path = std::path::Path::new(file);
    if cmd == "export" {
        let config = match AppConfig::load() {
            config::Loaded::Ok(c) => c,
            config::Loaded::FirstRun(_) => return Err("There are no settings to export yet".into()),
            config::Loaded::Error(_, e) => return Err(e),
        };
        config.export(path)?;
        return Ok(format!("Settings exported to {}", path.display()));
    }
    let config = AppConfig::import(path)?;
    config
        .save()
        .map_err(|e| format!("Could not save settings: {e}"))?;
    match ipc::send_cmd("reload") {
        Some(r) if !r.ok => Err(format!("Settings imported, but the app could not reload them: {}", r.message)),
        _ => Ok(format!("Settings imported from {}", path.display())),
    }
}

/// The tray app isn't running: talk to the monitor directly.
fn run_locally(cmd: &str, arg: &str, monitor_selector: Option<&str>) -> Result<String, String> {
    let config = AppConfig::load_or_default();
    let target = select_target(&config, monitor_selector)?;
    let profile = config.monitor_for_target(&target);
    let (name, device) = target.clone().unwrap_or_default();
    let mut monitor = ddc::Monitor::new(name, device).with_strict(config.monitors.len() > 1);
    let inputs = profile
        .map(|i| &config.monitors[i].inputs[..])
        .unwrap_or(&[]);
    match cmd {
        "list" => ddc::list_displays().map(|d| ipc::displays_text(&d)),
        "monitors" => Ok(ipc::monitors_text(&config)),
        "caps" => monitor.capabilities().map(|c| ipc::caps_text(&c)),
        "get" => monitor
            .get_input()
            .map(|v| ipc::describe_input(&config, &target, v)),
        "switch" => {
            let (value, name) = match ipc::find_input(inputs, arg) {
                Some(e) => (e.value, e.name.clone()),
                // A raw input code that isn't configured, e.g. "switch 17".
                None => {
                    let value = ipc::parse_value(arg).ok_or_else(|| {
                        format!(
                            "No input named or valued {arg:?}. Inputs: {}",
                            ipc::inputs_text(inputs)
                        )
                    })?;
                    (value, ddc::input_name(value))
                }
            };
            monitor.set_input(value)?;
            Ok(match profile {
                Some(i) if config.monitors.len() > 1 => {
                    let label = config.monitor_labels()[i].clone();
                    format!("Switched to {name} on {label}")
                }
                _ => format!("Switched to {name}"),
            })
        }
        "next" => {
            if profile.is_none() {
                return Err(format!(
                    "No configured monitor to advance. {}",
                    config.monitors_summary()
                ));
            }
            let current = monitor.get_input().ok().filter(|value| {
                ddc::status_name(*value).is_none()
                    && inputs
                        .iter()
                        .any(|entry| ddc::matches(*value, entry.value))
            });
            let target = config::next_input(inputs, current)?;
            monitor.set_input(target.value)?;
            Ok(match profile {
                Some(i) if config.monitors.len() > 1 => {
                    let label = config.monitor_labels()[i].clone();
                    format!("Switched to {} on {label}", target.name)
                }
                _ => format!("Switched to {}", target.name),
            })
        }
        _ => Err(format!("{APP_NAME} is not running")),
    }
}

fn main() {
    let (cmd, arg, monitor) = match parse_args(std::env::args().skip(1).collect()) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if !matches!(cmd.as_str(), "run" | "settings-window") {
        platform::attach_console();
    }
    let monitor_command = matches!(
        cmd.as_str(),
        "switch" | "next" | "get" | "capabilities" | "caps"
    );
    if monitor.is_some() && !monitor_command {
        eprintln!(
            "The --monitor option applies to switch, next, get and capabilities.\n\n{USAGE}"
        );
        std::process::exit(2);
    }
    let target = if monitor_command {
        let config = AppConfig::load_or_default();
        match select_target(&config, monitor.as_deref()) {
            Ok(target) => target,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    let code = match cmd.as_str() {
        "run" => daemon::run(),
        "settings-window" => settings::run(),
        "help" => {
            println!("{USAGE}");
            0
        }
        "switch" if arg.is_empty() => {
            eprintln!("Usage: {APP_ID} switch <INPUT>");
            2
        }
        "export" | "import" if arg.is_empty() => {
            eprintln!("Usage: {APP_ID} {cmd} <FILE>");
            2
        }
        "export" | "import" => match transfer(&cmd, &arg) {
            Ok(m) => {
                println!("{m}");
                0
            }
            Err(m) => {
                eprintln!("{m}");
                1
            }
        },
        "switch" | "next" | "get" | "list" | "capabilities" | "caps" | "monitors" | "settings"
        | "quit" => {
            let cmd = if cmd == "capabilities" { "caps" } else { &cmd };
            let req = Request {
                cmd: cmd.into(),
                arg: (!arg.is_empty()).then(|| arg.clone()),
                monitor: target.clone(),
                ..Default::default()
            };
            let result = match ipc::send(&req) {
                Some(r) if r.ok => Ok(r.message),
                Some(r) => Err(r.message),
                None => run_locally(cmd, &arg, monitor.as_deref()),
            };
            match result {
                Ok(m) => {
                    println!("{m}");
                    0
                }
                Err(m) => {
                    eprintln!("{m}");
                    1
                }
            }
        }
        other => {
            eprintln!("Unknown command {other:?}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_monitor_option() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_args(args(&["switch", "USB-C", "--monitor", "2"])).unwrap(),
            ("switch".into(), "USB-C".into(), Some("2".into()))
        );
        assert_eq!(
            parse_args(args(&["-m", "U2720Q", "get"])).unwrap(),
            ("get".into(), String::new(), Some("U2720Q".into()))
        );
        assert_eq!(
            parse_args(args(&["--monitor=/dev/i2c-21", "next"])).unwrap(),
            ("next".into(), String::new(), Some("/dev/i2c-21".into()))
        );
        assert_eq!(parse_args(args(&[])).unwrap().0, "run");
        assert_eq!(parse_args(args(&["-h"])).unwrap().0, "help");
        assert!(parse_args(args(&["--monitor"])).is_err());
        assert!(parse_args(args(&["switch", "a", "b"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
    }

    #[test]
    fn selects_configured_monitor() {
        let mut config = AppConfig::default();
        config.monitors = vec![
            MonitorConfig::new(
                Some("A".into()),
                Some("/dev/i2c-20".into()),
                vec![config::InputEntry::new("DP", 15, None)],
                None,
            ),
            MonitorConfig::new(
                Some("B".into()),
                Some("/dev/i2c-21".into()),
                vec![config::InputEntry::new("HDMI", 17, None)],
                None,
            ),
        ];
        assert_eq!(
            select_target(&config, None).unwrap(),
            Some((Some("A".into()), Some("/dev/i2c-20".into())))
        );
        assert_eq!(
            select_target(&config, Some("2")).unwrap(),
            Some((Some("B".into()), Some("/dev/i2c-21".into())))
        );
        assert!(select_target(&config, Some("nope")).is_err());
        assert_eq!(select_target(&AppConfig::default(), None).unwrap(), None);
    }
}
