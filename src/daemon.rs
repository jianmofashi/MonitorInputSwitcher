//! The tray app. One event loop thread owns all state; DDC runs on a single worker thread so
//! operations never overlap and never block the menu.

use crate::autoconfig;
use crate::config::{APP_NAME, AppConfig, InputEntry, Loaded, MonitorConfig, autostart};
use crate::ddc::{self, Monitor, Target};
use crate::hotkey::Hotkey;
use crate::ipc::{self, Request, Response};
use crate::platform::{self, ShortcutAction, TrayModel, TrayMonitor, Ui, UiEvent};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

const TOGGLE_ACTION: &str = "next-input";

type Reply = Sender<Response>;

enum Ev {
    Ui(UiEvent),
    SettingsClosed,
    Ipc(Request, Reply),
    InputsKnown(Vec<(Target, u32)>),
    AutoConfigured(Result<Vec<autoconfig::Detected>, String>),
    SwitchDone {
        target: Target,
        entry: Option<InputEntry>,
        result: Result<InputEntry, String>,
        reply: Option<Reply>,
        from_hotkey: bool,
    },
}

/// The monitor/input a global shortcut belongs to.
#[derive(Clone, Copy)]
enum ActionTarget {
    Switch { monitor: usize, input: usize },
    Next { monitor: usize },
}

struct ActionSpec {
    id: String,
    friendly: String,
    hotkey: Option<String>,
    target: ActionTarget,
}

fn input_action_id(monitor: usize, input: usize, single: bool) -> String {
    if single {
        // Keep the pre-multi-monitor ids: KDE stores user customisations by action id.
        format!("switch-input-{}", input + 1)
    } else {
        format!("switch-input-{}-{}", monitor + 1, input + 1)
    }
}

fn toggle_action_id(monitor: usize, single: bool) -> String {
    if single {
        TOGGLE_ACTION.into()
    } else {
        format!("{TOGGLE_ACTION}-{}", monitor + 1)
    }
}

// --- DDC worker ----------------------------------------------------------------------------------

/// Monitors cached by target. Settings may try displays that are not saved yet, so this is not
/// tied to the configured monitor list.
struct Monitors {
    cached: Vec<Monitor>,
    strict: bool,
}

impl Monitors {
    fn get(&mut self, target: Option<Target>) -> &mut Monitor {
        let (name, device) = target.unwrap_or_default();
        let index = match self.cached.iter().position(|m| m.is(&name, &device)) {
            Some(i) => i,
            None => {
                self.cached.push(Monitor::new(name, device));
                self.cached.len() - 1
            }
        };
        let strict = self.strict;
        let monitor = &mut self.cached[index];
        monitor.set_strict(strict);
        monitor
    }

    fn set_strict(&mut self, strict: bool) {
        self.strict = strict;
        for monitor in &mut self.cached {
            monitor.set_strict(strict);
        }
    }
}

type Job = Box<dyn FnOnce(&mut Monitors) + Send>;

fn spawn_ddc_worker(config: &AppConfig) -> Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    let mut monitors = Monitors {
        cached: Vec::new(),
        strict: config.monitors.len() > 1,
    };
    let first = config.monitors.first().map(MonitorConfig::target);
    thread::Builder::new()
        .name("ddc".into())
        .spawn(move || {
            if let Some(target) = first {
                monitors.get(Some(target)).warm_up();
            }
            for job in rx {
                job(&mut monitors);
            }
        })
        .expect("spawn ddc thread");
    tx
}

// --- app -----------------------------------------------------------------------------------------

struct App {
    config: AppConfig,
    /// Current input by configured monitor index, when known.
    last_known: Vec<Option<u32>>,
    refreshing: bool,
    tx: Sender<Ev>,
    ddc: Sender<Job>,
    ui: Ui,
    hotkey_busy: bool,
    settings_open: bool,
}

pub fn run() -> i32 {
    let Some(_lock) = platform::ipc::instance_lock() else {
        // Already running: bring up its Settings window instead of starting a second copy.
        let reply = ipc::send_cmd("settings");
        eprintln!(
            "{APP_NAME} is already running{}",
            if reply.is_some() {
                ""
            } else {
                " (not responding)"
            }
        );
        return 0;
    };

    let (config, first_run, load_error) = match AppConfig::load() {
        Loaded::Ok(c) => (c, false, None),
        Loaded::FirstRun(c) => (c, true, None),
        Loaded::Error(c, e) => (c, false, Some(e)),
    };
    if first_run {
        // Non-fatal on failure: startup can be enabled from the menu.
        let _ = config.save();
        let _ = autostart::set_enabled(true);
    } else {
        autostart::refresh_path();
    }

    let (tx, rx) = mpsc::channel();
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let forward = tx.clone();
    thread::spawn(move || {
        for ev in ui_rx {
            if forward.send(Ev::Ui(ev)).is_err() {
                break;
            }
        }
    });
    let ui = match Ui::start(ui_tx) {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let last_known = vec![None; config.monitors.len()];
    let mut app = App {
        ddc: spawn_ddc_worker(&config),
        config,
        last_known,
        refreshing: false,
        tx: tx.clone(),
        ui,
        hotkey_busy: false,
        settings_open: false,
    };
    app.update_tray();
    app.register_hotkeys(true);

    let ipc_tx = tx.clone();
    if let Err(e) = platform::ipc::listen(move |stream| {
        ipc::serve(stream, |req| {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = ipc_tx.send(Ev::Ipc(req, reply_tx));
            reply_rx
                .recv()
                .unwrap_or_else(|_| Response::err("The app is exiting"))
        })
    }) {
        app.notify_error("Command-line control unavailable", &e);
    }

    if let Some(e) = load_error {
        app.notify_error("Settings not loaded", &e);
    } else if app.config.monitors.iter().all(|m| m.inputs.is_empty()) {
        // First run (or nothing configured): set up whatever monitors are connected. Its
        // notification also says the app is running.
        app.start_autoconfig();
    } else if app.config.show_startup_notification || first_run {
        // Say the app started: otherwise it sits silently in the tray (often in the hidden
        // overflow area) and it isn't obvious that launching it did anything. Settings can turn
        // this off, except on the very first run.
        let mut body =
            "Running in the system tray: click its icon to switch input or open Settings."
                .to_string();
        if let Some(hk) = app
            .config
            .monitors
            .first()
            .and_then(|m| m.toggle_hotkey.as_deref())
            .and_then(Hotkey::parse)
        {
            body.push_str(&format!(
                " {hk} switches to the next input{}.",
                if app.config.monitors.len() > 1 {
                    " of the first monitor"
                } else {
                    ""
                }
            ));
        }
        if first_run {
            body.push_str(" It will also start at login.");
        }
        app.ui
            .notify(&format!("{APP_NAME} is running"), &body, false, 6000);
    }

    app.event_loop(rx);
    platform::ipc::cleanup();
    app.ui.shutdown();
    0
}

impl App {
    fn event_loop(&mut self, rx: Receiver<Ev>) {
        for ev in rx {
            match ev {
                Ev::Ui(UiEvent::Switch { monitor, input }) => {
                    self.switch(monitor, input, None, false)
                }
                Ev::Ui(UiEvent::Next { monitor }) => self.next_input(monitor, None, false),
                Ev::Ui(UiEvent::Hotkey(action)) => self.on_hotkey(&action),
                Ev::Ui(UiEvent::HotkeyChanged(action, hotkey)) => {
                    self.on_hotkey_changed(&action, hotkey)
                }
                Ev::Ui(UiEvent::MenuOpened) => self.refresh_current_inputs(),
                Ev::Ui(UiEvent::SetAutostart(on)) => {
                    if let Err(e) = autostart::set_enabled(on) {
                        self.notify_error("Could not change startup setting", &e.to_string());
                    }
                    self.update_tray();
                }
                Ev::Ui(UiEvent::OpenSettings) => self.open_settings(),
                Ev::Ui(UiEvent::Quit) => break,
                Ev::SettingsClosed => {
                    self.settings_open = false;
                    platform::block_shortcuts_for_capture(false); // in case Settings died while recording
                    self.ui.suspend_shortcuts(false);
                    self.update_tray();
                }
                Ev::Ipc(req, reply) => {
                    if self.handle_request(req, reply) {
                        break;
                    }
                }
                Ev::InputsKnown(values) => self.inputs_known(values),
                Ev::SwitchDone {
                    target,
                    entry,
                    result,
                    reply,
                    from_hotkey,
                } => self.switch_done(target, entry, result, reply, from_hotkey),
                Ev::AutoConfigured(result) => self.autoconfigured(result),
            }
        }
    }

    fn update_tray(&self) {
        let cfg = &self.config;
        let labels = cfg.monitor_labels();
        let monitors = cfg
            .monitors
            .iter()
            .enumerate()
            .map(|(i, monitor)| TrayMonitor {
                label: labels
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("Monitor {}", i + 1)),
                inputs: monitor
                    .inputs
                    .iter()
                    .map(|e| {
                        (
                            e.name.clone(),
                            Hotkey::parse_opt(&e.hotkey),
                            self.last_known
                                .get(i)
                                .copied()
                                .flatten()
                                .is_some_and(|v| ddc::matches(v, e.value)),
                        )
                    })
                    .collect(),
                toggle: Hotkey::parse_opt(&monitor.toggle_hotkey),
            })
            .collect();
        self.ui.update(TrayModel {
            monitors,
            autostart: autostart::is_enabled(),
        });
    }

    fn notify_error(&mut self, summary: &str, body: &str) {
        self.ui.notify(summary, body, true, 8000);
    }

    fn run_ddc(&self, job: impl FnOnce(&mut Monitors) + Send + 'static) {
        let _ = self.ddc.send(Box::new(job));
    }

    fn refresh_current_inputs(&mut self) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        let targets: Vec<Target> = self.config.monitors.iter().map(MonitorConfig::target).collect();
        let tx = self.tx.clone();
        self.run_ddc(move |m| {
            let mut values = Vec::new();
            for target in targets {
                if let Ok(value) = m.get(Some(target.clone())).get_input() {
                    values.push((target, value));
                }
            }
            let _ = tx.send(Ev::InputsKnown(values));
        });
    }

    fn inputs_known(&mut self, values: Vec<(Target, u32)>) {
        self.refreshing = false;
        for (target, value) in values {
            if let Some(i) = self
                .config
                .monitors
                .iter()
                .position(|m| ddc::target_matches(&m.target(), &target))
            {
                if self.last_known.len() <= i {
                    self.last_known.resize(i + 1, None);
                }
                // AOC monitors report DVI/HDMI as one read-only status, and some monitors report
                // an input that is not in their configured list. Keep the last switch in that
                // case instead of losing the check mark.
                if ddc::status_name(value).is_none()
                    && self.config.monitors[i]
                        .inputs
                        .iter()
                        .any(|entry| ddc::matches(value, entry.value))
                {
                    self.last_known[i] = Some(value);
                }
            }
        }
        self.update_tray();
    }

    // --- switching -------------------------------------------------------------------------------

    fn switch(&self, monitor: usize, index: usize, reply: Option<Reply>, from_hotkey: bool) {
        let found = self
            .config
            .monitors
            .get(monitor)
            .and_then(|m| m.inputs.get(index).cloned().map(|e| (m.target(), e)));
        match found {
            Some((target, entry)) => self.switch_to(target, entry, reply, from_hotkey),
            None => {
                if let Some(reply) = reply {
                    let _ = reply.send(Response::err("No such monitor or input"));
                }
            }
        }
    }

    fn switch_to(
        &self,
        target: Target,
        entry: InputEntry,
        reply: Option<Reply>,
        from_hotkey: bool,
    ) {
        let tx = self.tx.clone();
        self.run_ddc(move |m| {
            let result = m
                .get(Some(target.clone()))
                .set_input(entry.value)
                .map(|_| entry.clone());
            let _ = tx.send(Ev::SwitchDone {
                target,
                entry: Some(entry),
                result,
                reply,
                from_hotkey,
            });
        });
    }

    fn next_input(&self, monitor: usize, reply: Option<Reply>, from_hotkey: bool) {
        let Some(profile) = self.config.monitors.get(monitor) else {
            if let Some(reply) = reply {
                let _ = reply.send(Response::err(format!(
                    "No such monitor. {}",
                    self.config.monitors_summary()
                )));
            }
            return;
        };
        let target = profile.target();
        let inputs = profile.inputs.clone();
        let last = self.last_known.get(monitor).copied().flatten();
        let tx = self.tx.clone();
        self.run_ddc(move |m| {
            let current = m
                .get(Some(target.clone()))
                .get_input()
                .ok()
                .filter(|value| {
                    ddc::status_name(*value).is_none()
                        && inputs
                            .iter()
                            .any(|entry| ddc::matches(*value, entry.value))
                })
                .or(last);
            let result = crate::config::next_input(&inputs, current)
                .cloned()
                .and_then(|next| {
                    m.get(Some(target.clone()))
                        .set_input(next.value)
                        .map(|_| next)
                });
            let _ = tx.send(Ev::SwitchDone {
                target,
                entry: None,
                result,
                reply,
                from_hotkey,
            });
        });
    }

    fn switch_done(
        &mut self,
        target: Target,
        entry: Option<InputEntry>,
        result: Result<InputEntry, String>,
        reply: Option<Reply>,
        from_hotkey: bool,
    ) {
        if from_hotkey {
            self.hotkey_busy = false;
        }
        let monitor = self
            .config
            .monitors
            .iter()
            .position(|m| ddc::target_matches(&m.target(), &target));
        match result {
            Ok(switched) => {
                if let Some(i) = monitor {
                    if self.last_known.len() <= i {
                        self.last_known.resize(i + 1, None);
                    }
                    self.last_known[i] = Some(switched.value);
                }
                self.update_tray();
                let label = monitor.and_then(|i| self.config.monitor_labels().get(i).cloned());
                let msg = match label {
                    Some(label) if self.config.monitors.len() > 1 => {
                        format!("Switched to {} on {label}", switched.name)
                    }
                    _ => format!("Switched to {}", switched.name),
                };
                if self.config.show_notifications {
                    self.ui.notify("Input switched", &msg, false, 1500);
                }
                if let Some(reply) = reply {
                    let _ = reply.send(Response::ok(msg));
                }
            }
            Err(e) => {
                let title = entry.map_or("Could not switch input".to_string(), |entry| {
                    format!("Could not switch to {}", entry.name)
                });
                self.notify_error(&title, &e);
                if let Some(reply) = reply {
                    let _ = reply.send(Response::err(e));
                }
            }
        }
    }

    // --- global shortcuts ------------------------------------------------------------------------

    /// Every shortcut the app owns, in config order.
    fn hotkey_actions(&self) -> Vec<ActionSpec> {
        let single = self.config.monitors.len() == 1;
        let labels = self.config.monitor_labels();
        let mut out = Vec::new();
        for (monitor_index, monitor) in self.config.monitors.iter().enumerate() {
            let label = labels
                .get(monitor_index)
                .cloned()
                .unwrap_or_else(|| format!("Monitor {}", monitor_index + 1));
            for (input_index, entry) in monitor.inputs.iter().enumerate() {
                out.push(ActionSpec {
                    id: input_action_id(monitor_index, input_index, single),
                    friendly: if single {
                        format!("Switch to {}", entry.name)
                    } else {
                        format!("Switch {label} to {}", entry.name)
                    },
                    hotkey: entry.hotkey.clone(),
                    target: ActionTarget::Switch {
                        monitor: monitor_index,
                        input: input_index,
                    },
                });
            }
            if monitor.inputs.len() > 1 {
                out.push(ActionSpec {
                    id: toggle_action_id(monitor_index, single),
                    friendly: if single {
                        "Next input".into()
                    } else {
                        format!("Next input on {label}")
                    },
                    hotkey: monitor.toggle_hotkey.clone(),
                    target: ActionTarget::Next {
                        monitor: monitor_index,
                    },
                });
            }
        }
        out
    }

    fn register_hotkeys(&mut self, show_errors: bool) {
        let actions = self.hotkey_actions();
        let shortcuts: Vec<ShortcutAction> = actions
            .iter()
            .map(|action| ShortcutAction {
                id: action.id.clone(),
                friendly: action.friendly.clone(),
                hotkey: Hotkey::parse_opt(&action.hotkey),
            })
            .collect();
        match self.ui.register_shortcuts(&shortcuts) {
            Ok(failed_ids) => {
                let failed: Vec<String> = shortcuts
                    .iter()
                    .filter(|action| failed_ids.contains(&action.id))
                    .map(|action| {
                        format!(
                            "{} ({})",
                            action
                                .hotkey
                                .as_ref()
                                .map_or_else(|| "?".to_string(), Hotkey::to_string),
                            action.friendly
                        )
                    })
                    .collect();
                if show_errors && !failed.is_empty() {
                    self.notify_error(
                        "Some shortcuts are unavailable",
                        &format!("Already used by another shortcut: {}", failed.join(", ")),
                    );
                }
            }
            Err(e) if show_errors => self.notify_error("Global shortcuts unavailable", &e),
            Err(_) => {}
        }
    }

    fn on_hotkey(&mut self, action: &str) {
        if self.hotkey_busy || self.settings_open {
            return;
        }
        // Stuck modifiers (keyboard on the monitor's USB hub, moved by the KVM) are the platform's
        // job: Windows releases them before reporting the press, Linux needs nothing (the kernel
        // removes the device and libinput releases its keys).
        let Some(spec) = self
            .hotkey_actions()
            .into_iter()
            .find(|spec| spec.id == action)
        else {
            return;
        };
        match spec.target {
            ActionTarget::Next { monitor } => {
                self.hotkey_busy = true;
                self.next_input(monitor, None, true);
            }
            ActionTarget::Switch { monitor, input } => {
                self.hotkey_busy = true;
                self.switch(monitor, input, None, true);
            }
        }
    }

    /// The user changed one of our shortcuts outside the app (KDE System Settings): keep the
    /// config in sync.
    fn on_hotkey_changed(&mut self, action: &str, hotkey: Option<Hotkey>) {
        let Some(spec) = self
            .hotkey_actions()
            .into_iter()
            .find(|spec| spec.id == action)
        else {
            return;
        };
        if Hotkey::parse_opt(&spec.hotkey) == hotkey {
            return;
        }
        let new = hotkey.map(|h| h.to_string());
        match spec.target {
            ActionTarget::Next { monitor } => {
                if let Some(monitor) = self.config.monitors.get_mut(monitor) {
                    monitor.toggle_hotkey = new;
                }
            }
            ActionTarget::Switch { monitor, input } => {
                if let Some(entry) = self
                    .config
                    .monitors
                    .get_mut(monitor)
                    .and_then(|m| m.inputs.get_mut(input))
                {
                    entry.hotkey = new;
                }
            }
        }
        if let Err(e) = self.config.save() {
            self.notify_error("Could not save settings", &e.to_string());
        }
        self.update_tray();
        self.register_hotkeys(false);
    }

    // --- settings --------------------------------------------------------------------------------

    fn open_settings(&mut self) {
        if self.settings_open {
            return;
        }
        let exe = crate::config::self_exe();
        match Command::new(exe).arg("settings-window").spawn() {
            Ok(mut child) => {
                self.settings_open = true;
                // Windows: release our hotkeys so the Settings window can record them.
                self.ui.suspend_shortcuts(true);
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let _ = child.wait();
                    let _ = tx.send(Ev::SettingsClosed);
                });
            }
            Err(e) => self.notify_error("Could not open Settings", &e.to_string()),
        }
    }

    fn reload_config(&mut self) -> Response {
        let config = match AppConfig::load() {
            Loaded::Ok(c) => c,
            Loaded::FirstRun(_) => return Response::err("Config file missing"),
            Loaded::Error(_, e) => return Response::err(e),
        };
        self.apply_config(config);
        Response::ok("Settings reloaded")
    }

    fn start_autoconfig(&self) {
        let tx = self.tx.clone();
        self.run_ddc(move |_| {
            let _ = tx.send(Ev::AutoConfigured(autoconfig::detect_all()));
        });
    }

    fn autoconfigured(&mut self, result: Result<Vec<autoconfig::Detected>, String>) {
        if self.config.monitors.iter().any(|m| !m.inputs.is_empty()) {
            return; // configured in Settings meanwhile
        }
        let detected = match result {
            Ok(d) => d,
            Err(e) => {
                self.notify_error(
                    "No monitor set up",
                    &format!("{e}. Open Settings to configure one."),
                );
                return;
            }
        };
        let mut monitors = Vec::new();
        let mut notes = Vec::new();
        let mut shortcut_index = 0usize;
        for detected in detected {
            let autoconfig::Detected {
                display,
                inputs,
                note,
            } = detected;
            if inputs.is_empty() {
                continue;
            }
            let inputs: Vec<InputEntry> = inputs
                .into_iter()
                .map(|mut entry| {
                    entry.hotkey = autoconfig::default_hotkey(shortcut_index);
                    shortcut_index += 1;
                    entry
                })
                .collect();
            let first = monitors.is_empty();
            monitors.push(MonitorConfig::new(
                (!display.model.is_empty()).then(|| display.model.clone()),
                (!display.bus.is_empty()).then(|| display.bus.clone()),
                inputs,
                first.then(|| autoconfig::TOGGLE_HOTKEY.to_string()),
            ));
            if let Some(note) = note {
                notes.push(format!(
                    "{}: {note}",
                    if display.model.is_empty() {
                        &display.bus
                    } else {
                        &display.model
                    }
                ));
            }
        }
        if monitors.is_empty() {
            self.notify_error(
                "No monitor set up",
                "No monitor reported any inputs. Open Settings to configure one.",
            );
            return;
        }
        let mut config = self.config.clone();
        config.monitors = monitors;
        if let Err(e) = config.check() {
            self.notify_error("Could not save settings", &e);
            return;
        }
        if let Err(e) = config.save() {
            self.notify_error("Could not save settings", &e.to_string());
        }
        let labels = config.monitor_labels();
        let body = if config.monitors.len() == 1 {
            let summary: Vec<String> = config.monitors[0]
                .inputs
                .iter()
                .map(|entry| match &entry.hotkey {
                    Some(hotkey) => format!("{} ({hotkey})", entry.name),
                    None => entry.name.clone(),
                })
                .collect();
            format!(
                "Running in the system tray. Inputs: {}.",
                summary.join(", ")
            )
        } else {
            let summary: Vec<String> = config
                .monitors
                .iter()
                .enumerate()
                .map(|(i, monitor)| {
                    format!(
                        "{} ({} inputs)",
                        labels.get(i).cloned().unwrap_or_default(),
                        monitor.inputs.len()
                    )
                })
                .collect();
            format!(
                "Running in the system tray. Set up {} monitors: {}. Other monitors have no default Next-input shortcut; set them in Settings.",
                config.monitors.len(),
                summary.join(", ")
            )
        };
        let mut body = body;
        if !notes.is_empty() {
            body.push(' ');
            body.push_str(&notes.join(" "));
        }
        body.push_str(" In Settings, untick \"In Next\" for inputs with nothing plugged in.");
        let title = if config.monitors.len() == 1 {
            format!("Set up for {}", labels.first().cloned().unwrap_or_default())
        } else {
            format!("Set up for {} monitors", config.monitors.len())
        };
        self.apply_config(config);
        self.ui.notify(&title, &body, false, 10000);
    }

    fn apply_config(&mut self, config: AppConfig) {
        let old_targets: Vec<Target> = self
            .config
            .monitors
            .iter()
            .map(MonitorConfig::target)
            .collect();
        let new_targets: Vec<Target> = config.monitors.iter().map(MonitorConfig::target).collect();
        if old_targets != new_targets {
            let old_known = std::mem::take(&mut self.last_known);
            let known: Vec<Option<u32>> = new_targets
                .iter()
                .map(|target| {
                    self.config
                        .monitors
                        .iter()
                        .position(|m| ddc::target_matches(&m.target(), target))
                        .and_then(|i| old_known.get(i).copied().flatten())
                })
                .collect();
            self.last_known = known;
            let strict = config.monitors.len() > 1;
            self.run_ddc(move |m| m.set_strict(strict));
        } else if self.last_known.len() != config.monitors.len() {
            self.last_known.resize(config.monitors.len(), None);
        }
        self.config = config;
        self.update_tray();
        self.register_hotkeys(true);
    }

    /// The monitor a request targets, or the first configured one.
    fn default_target(&self) -> Option<Target> {
        self.config.monitors.first().map(MonitorConfig::target)
    }

    fn requested_target(&self, requested: &Option<Target>) -> Option<Target> {
        requested.clone().or_else(|| self.default_target())
    }

    fn profile_for_target(&self, target: &Option<Target>) -> Option<usize> {
        let target = target.clone().or_else(|| self.default_target())?;
        self.config
            .monitors
            .iter()
            .position(|m| ddc::target_matches(&m.target(), &target))
    }

    /// Returns true when the app should exit.
    fn handle_request(&mut self, req: Request, reply: Reply) -> bool {
        let send = |r: Response| {
            let _ = reply.send(r);
        };
        match req.cmd.as_str() {
            "switch" => {
                let arg = req.arg.unwrap_or_default();
                let target = self.requested_target(&req.monitor);
                let profile = self.profile_for_target(&target);
                let entry = profile
                    .and_then(|i| ipc::find_input(&self.config.monitors[i].inputs, &arg).cloned());
                match entry {
                    Some(entry) => {
                        self.switch_to(target.unwrap_or_default(), entry, Some(reply), false)
                    }
                    None => match ipc::parse_value(&arg) {
                        // A raw input code that isn't configured, e.g. "switch 17".
                        Some(value) => {
                            let entry = InputEntry::new(ddc::input_name(value), value, None);
                            self.switch_to(target.unwrap_or_default(), entry, Some(reply), false)
                        }
                        None => send(Response::err(format!(
                            "No input named or valued {arg:?}. Inputs: {}",
                            profile
                                .map(|i| ipc::inputs_text(&self.config.monitors[i].inputs))
                                .unwrap_or_else(|| {
                                    if self.config.monitors.is_empty() {
                                        "No monitors configured".into()
                                    } else {
                                        self.config.monitors_summary()
                                    }
                                })
                        ))),
                    },
                }
            }
            "next" => {
                let target = self.requested_target(&req.monitor);
                match self.profile_for_target(&target) {
                    Some(monitor) => self.next_input(monitor, Some(reply), false),
                    None => send(Response::err(format!(
                        "No configured monitor to advance. {}",
                        self.config.monitors_summary()
                    ))),
                }
            }
            "get" => {
                let config = self.config.clone();
                let target = self.requested_target(&req.monitor);
                self.run_ddc(move |m| {
                    let response = match m.get(target.clone()).get_input() {
                        Ok(v) => Response {
                            value: Some(v),
                            ..Response::ok(ipc::describe_input(&config, &target, v))
                        },
                        Err(e) => Response::err(e),
                    };
                    let _ = reply.send(response);
                });
            }
            "caps" => {
                let target = self.requested_target(&req.monitor);
                self.run_ddc(move |m| {
                    let response = match m.get(target).capabilities() {
                        Ok(caps) => Response {
                            caps: Some(caps.clone()),
                            ..Response::ok(ipc::caps_text(&caps))
                        },
                        Err(e) => Response::err(e),
                    };
                    let _ = reply.send(response);
                });
            }
            "set" => {
                let (target, value) = (req.monitor, req.value.unwrap_or(0));
                self.run_ddc(move |m| {
                    let _ = reply.send(match m.get(target).set_input(value) {
                        Ok(()) => Response::ok(format!("Set input {value}")),
                        Err(e) => Response::err(e),
                    });
                });
            }
            "list" => self.run_ddc(move |_| {
                let _ = reply.send(match ddc::list_displays() {
                    Ok(d) => Response {
                        displays: Some(d.clone()),
                        ..Response::ok(ipc::displays_text(&d))
                    },
                    Err(e) => Response::err(e),
                });
            }),
            "monitors" => send(Response::ok(ipc::monitors_text(&self.config))),
            "reload" => send(self.reload_config()),
            "settings" => {
                self.open_settings();
                send(Response::ok("Settings opened"));
            }
            "quit" => {
                send(Response::ok("Exiting"));
                return true;
            }
            "ping" => send(Response::ok("running")),
            other => send(Response::err(format!("Unknown command {other:?}"))),
        }
        false
    }
}
