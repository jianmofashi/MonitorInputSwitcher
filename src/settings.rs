//! Settings window (egui), run as a short-lived child process of the tray app.
//!
//! It edits config.json, then asks the running app to reload. DDC operations (detect, read,
//! "Switch now") go through the running app so they stay serialized with everything else.

use crate::config::{APP_NAME, AppConfig, InputEntry, MonitorConfig, autostart};
use crate::ddc::{self, DisplayInfo, Target};
use crate::hotkey::{DIGIT_ROW, Hotkey};
use crate::ipc::{self, Request};
use crate::platform::{self, ICON_PNG};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

/// Scan codes of the punctuation keys (PC set 1, which Linux evdev codes match for these keys), by
/// the egui key of their US-layout position.
const PUNCTUATION_SCANCODES: &[(egui::Key, u32)] = &[
    (egui::Key::Backtick, 0x29),
    (egui::Key::Minus, 0x0C),
    (egui::Key::Equals, 0x0D),
    (egui::Key::OpenBracket, 0x1A),
    (egui::Key::CloseBracket, 0x1B),
    (egui::Key::Backslash, 0x2B),
    (egui::Key::Semicolon, 0x27),
    (egui::Key::Quote, 0x28),
    (egui::Key::Comma, 0x33),
    (egui::Key::Period, 0x34),
    (egui::Key::Slash, 0x35),
];

struct Row {
    name: String,
    value: u32,
    hotkey: Option<Hotkey>,
    in_next: bool,
    testing: bool,
}

/// One monitor being edited. Inputs and the Next shortcut belong to this draft only.
struct MonitorDraft {
    /// Stable for async messages, also survives reordering.
    id: u64,
    name: Option<String>,
    device: Option<String>,
    inputs: Vec<Row>,
    toggle: Option<Hotkey>,
    detecting: bool,
    inputs_text: String,
    current_text: String,
}

impl MonitorDraft {
    fn target(&self) -> Target {
        (
            clean_target_part(&self.name),
            clean_target_part(&self.device),
        )
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Capture {
    Row(usize),
    Toggle,
}

enum Msg {
    Displays(Result<Vec<DisplayInfo>, String>),
    Current {
        draft: u64,
        result: Result<u32, String>,
    },
    Tested {
        draft: u64,
        row: usize,
        result: Result<(), String>,
    },
    Capabilities {
        draft: u64,
        result: Result<String, String>,
    },
    /// None: the file dialog was cancelled.
    Exported(Result<Option<PathBuf>, String>),
    Imported(Result<Option<(PathBuf, AppConfig)>, String>),
}

const EXPORT_FILE_NAME: &str = "monitor-input-switcher-settings.json";

fn file_dialog(title: &str) -> rfd::FileDialog {
    rfd::FileDialog::new()
        .set_title(title)
        .add_filter("Settings", &["json"])
}

fn rows_from(monitor: &MonitorConfig) -> Vec<Row> {
    monitor
        .inputs
        .iter()
        .map(|e| Row {
            name: e.name.clone(),
            value: e.value,
            hotkey: Hotkey::parse_opt(&e.hotkey),
            in_next: e.include_in_next,
            testing: false,
        })
        .collect()
}

fn clean_target_part(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// --- DDC through the running app, or directly when it isn't running ----------------------------

fn remote(req: Request) -> Option<Result<ipc::Response, String>> {
    ipc::send(&req).map(|r| if r.ok { Ok(r) } else { Err(r.message) })
}

fn list_displays() -> Result<Vec<DisplayInfo>, String> {
    match remote(Request {
        cmd: "list".into(),
        ..Default::default()
    }) {
        Some(r) => r.map(|r| r.displays.unwrap_or_default()),
        None => ddc::list_displays(),
    }
}

fn get_input(target: Target) -> Result<u32, String> {
    match remote(Request {
        cmd: "get".into(),
        monitor: Some(target.clone()),
        ..Default::default()
    }) {
        Some(r) => r.and_then(|r| r.value.ok_or_else(|| "No value returned".into())),
        None => ddc::Monitor::new(target.0, target.1)
            .with_strict(true)
            .get_input(),
    }
}

fn capabilities(target: Target) -> Result<String, String> {
    match remote(Request {
        cmd: "caps".into(),
        monitor: Some(target.clone()),
        ..Default::default()
    }) {
        Some(r) => r.and_then(|r| r.caps.ok_or_else(|| "No capabilities returned".into())),
        None => ddc::Monitor::new(target.0, target.1)
            .with_strict(true)
            .capabilities(),
    }
}

fn set_input(target: Target, value: u32) -> Result<(), String> {
    match remote(Request {
        cmd: "set".into(),
        monitor: Some(target.clone()),
        value: Some(value),
        ..Default::default()
    }) {
        Some(r) => r.map(|_| ()),
        None => ddc::Monitor::new(target.0, target.1)
            .with_strict(true)
            .set_input(value),
    }
}

struct SettingsApp {
    /// The loaded config, used as the base for export/save (keeps unknown global fields).
    config: AppConfig,
    drafts: Vec<MonitorDraft>,
    selected: usize,
    autostart: bool,
    notifications: bool,
    startup_notification: bool,
    displays: Option<Vec<DisplayInfo>>,
    displays_error: Option<String>,
    next_id: u64,
    /// Content height the window was last grown to fit (see `fit_to_content`).
    fitted_height: f32,
    error: Option<String>,
    /// Outcome of the last import or export.
    status: Option<String>,
    /// An import or export file dialog is open.
    file_dialog: bool,
    capture: Option<Capture>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl SettingsApp {
    fn new(ctx: &egui::Context) -> Self {
        let config = AppConfig::load_or_default();
        let (tx, rx) = mpsc::channel();
        let mut app = SettingsApp {
            drafts: Vec::new(),
            selected: 0,
            autostart: autostart::is_enabled(),
            notifications: config.show_notifications,
            startup_notification: config.show_startup_notification,
            config: config.clone(),
            displays: None,
            displays_error: None,
            next_id: 0,
            fitted_height: 0.0,
            error: None,
            status: None,
            file_dialog: false,
            capture: None,
            tx,
            rx,
        };
        app.drafts = app.drafts_from(&config);
        app.load_monitors(ctx);
        app
    }

    fn new_draft_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn drafts_from(&mut self, config: &AppConfig) -> Vec<MonitorDraft> {
        let mut drafts: Vec<MonitorDraft> = config
            .monitors
            .iter()
            .map(|monitor| MonitorDraft {
                id: {
                    self.next_id += 1;
                    self.next_id
                },
                name: monitor.name.clone(),
                device: monitor.device.clone(),
                inputs: rows_from(monitor),
                toggle: Hotkey::parse_opt(&monitor.toggle_hotkey),
                detecting: false,
                inputs_text: String::new(),
                current_text: String::new(),
            })
            .collect();
        if drafts.is_empty() {
            let id = self.new_draft_id();
            drafts.push(MonitorDraft {
                id,
                name: None,
                device: None,
                inputs: Vec::new(),
                toggle: None,
                detecting: false,
                inputs_text: String::new(),
                current_text: String::new(),
            });
        }
        drafts
    }

    /// Shows imported settings in the form; they are applied on Save. "Start with login" is
    /// per machine and not part of the file, so it stays as it is.
    fn load_form(&mut self, config: AppConfig) {
        self.set_capture(None);
        self.drafts = self.drafts_from(&config);
        self.selected = 0;
        self.notifications = config.show_notifications;
        self.startup_notification = config.show_startup_notification;
        self.config = config;
    }

    fn export(&mut self, ctx: &egui::Context) {
        let config = match self.form_config() {
            Ok(c) => c,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.file_dialog = true;
        self.spawn(ctx, move || {
            let Some(path) = file_dialog("Export settings")
                .set_file_name(EXPORT_FILE_NAME)
                .save_file()
            else {
                return Msg::Exported(Ok(None));
            };
            Msg::Exported(config.export(&path).map(|()| Some(path)))
        });
    }

    fn import(&mut self, ctx: &egui::Context) {
        self.file_dialog = true;
        self.spawn(ctx, || {
            let Some(path) = file_dialog("Import settings").pick_file() else {
                return Msg::Imported(Ok(None));
            };
            Msg::Imported(AppConfig::import(&path).map(|c| Some((path, c))))
        });
    }

    fn spawn(&self, ctx: &egui::Context, job: impl FnOnce() -> Msg + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
    }

    fn load_monitors(&mut self, ctx: &egui::Context) {
        self.displays = None;
        self.displays_error = None;
        self.spawn(ctx, || Msg::Displays(list_displays()));
    }

    fn target(&self) -> Target {
        self.drafts
            .get(self.selected)
            .map(MonitorDraft::target)
            .unwrap_or_default()
    }

    fn draft_id(&self, index: usize) -> u64 {
        self.drafts.get(index).map_or(0, |d| d.id)
    }

    fn draft_label(&self, index: usize) -> String {
        let Some(draft) = self.drafts.get(index) else {
            return String::new();
        };
        let name = draft
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .or_else(|| draft.device.clone())
            .unwrap_or_else(|| "(new monitor)".into());
        format!("{}. {name}", index + 1)
    }

    /// DDC-capable displays not already assigned to a draft.
    fn available_displays(&self) -> Vec<DisplayInfo> {
        let displays = self.displays.as_deref().unwrap_or(&[]);
        displays
            .iter()
            .filter(|display| {
                display.valid
                    && !self.drafts.iter().any(|draft| {
                        draft.device.as_deref() == Some(display.bus.as_str())
                            || (draft.device.is_none()
                                && draft.name.as_deref().is_some_and(|name| {
                                    name.eq_ignore_ascii_case(&display.model)
                                }))
                    })
            })
            .cloned()
            .collect()
    }

    /// The target shown in the picker. A legacy name-only target follows the matching detected
    /// display, without changing the saved device unless the user picks a display.
    fn picker_target(&self) -> Target {
        self.drafts
            .get(self.selected)
            .map(|draft| self.resolved_target(draft))
            .unwrap_or_default()
    }

    /// Fills in the device for a name-only target when exactly one detected display matches. This
    /// keeps the old Settings behaviour (picking a detected monitor saved its device) and makes
    /// multi-monitor entries unambiguous.
    fn resolved_target(&self, draft: &MonitorDraft) -> Target {
        let (name, device) = draft.target();
        if device.is_none()
            && let Some(name) = name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            && let Some(display) = self
                .displays
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .find(|display| display.model.eq_ignore_ascii_case(name))
        {
            return (
                Some(display.model.clone()).filter(|model| !model.is_empty()),
                Some(display.bus.clone()).filter(|bus| !bus.is_empty()),
            );
        }
        (name, device)
    }

    /// The target picker entries for the selected draft, including a "(not detected)" entry.
    fn display_options(&self) -> Vec<(String, Target)> {
        let current = self.picker_target();
        let mut options = Vec::new();
        if current == (None, None) {
            options.push(("(choose a monitor)".to_string(), current.clone()));
        }
        for display in self.displays.as_deref().unwrap_or(&[]) {
            let target = (
                Some(display.model.clone()).filter(|m| !m.is_empty()),
                Some(display.bus.clone()).filter(|b| !b.is_empty()),
            );
            options.push((display.label(), target));
        }
        if current != (None, None) && !options.iter().any(|(_, target)| *target == current) {
            options.push(("(not detected)".to_string(), current));
        }
        options
    }

    fn set_capture(&mut self, capture: Option<Capture>) {
        if self.capture.is_some() != capture.is_some() {
            // While recording, global shortcuts are suspended so the keys reach this window.
            platform::block_shortcuts_for_capture(capture.is_some());
        }
        self.capture = capture;
    }

    fn handle_capture(&mut self, ctx: &egui::Context) {
        let Some(capture) = self.capture else { return };
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            let egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                modifiers,
                ..
            } = ev
            else {
                continue;
            };
            let plain = !(modifiers.ctrl || modifiers.alt || modifiers.shift);
            let selected = self.selected;
            let slot = match capture {
                Capture::Row(i) => match self
                    .drafts
                    .get_mut(selected)
                    .and_then(|d| d.inputs.get_mut(i))
                {
                    Some(row) => &mut row.hotkey,
                    None => return self.set_capture(None),
                },
                Capture::Toggle => match self.drafts.get_mut(selected) {
                    Some(draft) => &mut draft.toggle,
                    None => return self.set_capture(None),
                },
            };
            match key {
                egui::Key::Escape if plain => {}
                egui::Key::Backspace | egui::Key::Delete if plain => *slot = None,
                _ => {
                    // Digit-row keys are recorded by position, as on Windows: Shift+1 stays "Shift+1" on AZERTY.
                    let digit = physical_key
                        .map(|k| k.name())
                        .filter(|n| n.len() == 1 && DIGIT_ROW.iter().any(|d| n.starts_with(*d)));
                    // Punctuation keys are recorded as the character the layout puts on them. egui
                    // has no name for characters like "²", so it reports such keys by their US
                    // position instead (the "²" key of a French keyboard arrives as Backtick).
                    let punctuation = physical_key
                        .and_then(|p| PUNCTUATION_SCANCODES.iter().find(|(k, _)| *k == p))
                        .and_then(|(_, code)| platform::layout_char(*code))
                        .map(String::from);
                    let raw = digit
                        .map(str::to_string)
                        .or(punctuation)
                        .unwrap_or_else(|| key.name().to_string());
                    let Some(name) = crate::hotkey::canonical_key(&raw) else {
                        continue;
                    };
                    *slot = Some(Hotkey {
                        ctrl: modifiers.ctrl,
                        alt: modifiers.alt,
                        shift: modifiers.shift,
                        meta: false,
                        key: name,
                    });
                }
            }
            return self.set_capture(None);
        }
    }

    fn hotkey_button(&mut self, ui: &mut egui::Ui, which: Capture) {
        let recording = self.capture == Some(which);
        let value = match which {
            Capture::Row(i) => self
                .drafts
                .get(self.selected)
                .and_then(|draft| draft.inputs.get(i))
                .and_then(|row| row.hotkey.clone()),
            Capture::Toggle => self
                .drafts
                .get(self.selected)
                .and_then(|draft| draft.toggle.clone()),
        };
        let text = if recording {
            "Press keys... (Esc cancels)".to_string()
        } else {
            value
                .as_ref()
                .map_or("(none) - click to set".into(), Hotkey::to_string)
        };
        let button = egui::Button::new(text)
            .selected(recording)
            .min_size(egui::vec2(200.0, 0.0));
        let resp = ui
            .add(button)
            .on_hover_text("Click, then press the key combination. Backspace clears it.");
        if resp.clicked() {
            self.set_capture(if recording { None } else { Some(which) });
        }
    }

    fn add_monitor(&mut self, ctx: &egui::Context, display: Option<&DisplayInfo>) {
        let id = self.new_draft_id();
        let (name, device) = match display {
            Some(display) => (
                Some(display.model.clone()).filter(|m| !m.is_empty()),
                Some(display.bus.clone()).filter(|b| !b.is_empty()),
            ),
            None => (None, None),
        };
        let detecting = display.is_some();
        self.drafts.push(MonitorDraft {
            id,
            name,
            device,
            inputs: Vec::new(),
            toggle: None,
            detecting,
            inputs_text: if detecting {
                "Detecting inputs...".to_string()
            } else {
                String::new()
            },
            current_text: String::new(),
        });
        self.selected = self.drafts.len() - 1;
        self.set_capture(None);
        if detecting {
            let target = self.target();
            self.spawn(ctx, move || Msg::Capabilities {
                draft: id,
                result: capabilities(target),
            });
        }
    }

    fn remove_selected(&mut self) {
        if self.drafts.len() <= 1 {
            return;
        }
        self.drafts.remove(self.selected);
        self.selected = self.selected.min(self.drafts.len() - 1);
        self.set_capture(None);
    }

    fn move_selected(&mut self, up: bool) {
        let other = if up {
            self.selected.checked_sub(1)
        } else {
            (self.selected + 1 < self.drafts.len()).then_some(self.selected + 1)
        };
        if let Some(other) = other {
            self.drafts.swap(self.selected, other);
            self.selected = other;
            self.set_capture(None);
        }
    }

    fn used_hotkeys(&self) -> Vec<Hotkey> {
        self.drafts
            .iter()
            .flat_map(|draft| {
                draft
                    .inputs
                    .iter()
                    .filter_map(|row| row.hotkey.clone())
                    .chain(draft.toggle.clone())
            })
            .collect()
    }

    fn free_default_hotkey(&self) -> Option<Hotkey> {
        let used = self.used_hotkeys();
        (0..9)
            .filter_map(|i| crate::autoconfig::default_hotkey(i).and_then(|h| Hotkey::parse(&h)))
            .find(|hotkey| !used.contains(hotkey))
    }

    /// Adds a row for every input the monitor lists that isn't configured yet.
    fn add_detected_inputs(&mut self, draft_id: u64, caps: &str) {
        let Some(index) = self.drafts.iter().position(|draft| draft.id == draft_id) else {
            return;
        };
        let found = crate::autoconfig::input_entries(&ddc::caps_inputs(caps));
        if found.is_empty() {
            self.drafts[index].inputs_text =
                "The monitor's capabilities list no input codes; add them by hand.".into();
            return;
        }
        let mut added = Vec::new();
        for (value, name) in &found {
            if self.drafts[index]
                .inputs
                .iter()
                .any(|row| row.value == *value || ddc::matches(row.value, *value))
            {
                continue;
            }
            let hotkey = self.free_default_hotkey();
            self.drafts[index].inputs.push(Row {
                name: name.clone(),
                value: *value,
                hotkey,
                in_next: true,
                testing: false,
            });
            added.push(name.clone());
        }
        let all: Vec<String> = found
            .iter()
            .map(|(value, name)| format!("{name} ({value})"))
            .collect();
        let mut text = format!(
            "The monitor lists {} input(s): {}. {}",
            found.len(),
            all.join(", "),
            if added.is_empty() {
                "All are already configured.".to_string()
            } else {
                format!(
                    "Added: {}. Untick In Next for inputs with nothing plugged in.",
                    added.join(", ")
                )
            }
        );
        if !found
            .iter()
            .any(|(value, _)| matches!(*value, 0x11 | 0x12 | 0x0F))
        {
            text.push_str(
                " If this monitor also has HDMI or DisplayPort, add it by hand (HDMI-1 = 17, \
                 DisplayPort = 15): some monitors do not list every input.",
            );
        }
        self.drafts[index].inputs_text = text;
    }

    /// The settings as currently shown in the form, checked the same way as an import.
    fn form_config(&self) -> Result<AppConfig, String> {
        let mut cfg = self.config.clone();
        cfg.monitors = self
            .drafts
            .iter()
            .map(|draft| {
                let (name, device) = self.resolved_target(draft);
                MonitorConfig::new(
                    name,
                    device,
                    draft
                        .inputs
                        .iter()
                        .map(|row| InputEntry {
                            name: row.name.trim().into(),
                            value: row.value,
                            hotkey: row.hotkey.as_ref().map(Hotkey::to_string),
                            include_in_next: row.in_next,
                        })
                        .collect(),
                    draft.toggle.as_ref().map(Hotkey::to_string),
                )
            })
            .collect();
        cfg.show_notifications = self.notifications;
        cfg.show_startup_notification = self.startup_notification;
        cfg.check()?;
        Ok(cfg)
    }

    fn save(&mut self, ctx: &egui::Context) {
        let cfg = match self.form_config() {
            Ok(c) => c,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.config = cfg;
        if let Err(e) = self.config.save() {
            self.error = Some(format!("Could not save settings: {e}"));
            return;
        }
        if let Err(e) = autostart::set_enabled(self.autostart) {
            self.error = Some(format!("Could not update startup setting: {e}"));
            return;
        }
        if let Some(r) = ipc::send_cmd("reload")
            && !r.ok
        {
            self.error = Some(r.message);
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl SettingsApp {
    /// Grows the window when its content got taller than the window (more inputs, a status line,
    /// a wrapped hint), up to 90% of the monitor height. It only reacts to content growth, so a
    /// window the user made smaller stays that way until something new is added.
    fn fit_to_content(&mut self, ctx: &egui::Context, content: f32, visible: f32) {
        let overflow = content - visible;
        if overflow <= 1.0 || content <= self.fitted_height + 1.0 {
            return;
        }
        self.fitted_height = content;
        let (inner, monitor) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().monitor_size));
        let Some(inner) = inner else { return };
        let mut height = inner.height() + overflow;
        if let Some(monitor) = monitor {
            height = height.min(monitor.y * 0.9);
        }
        if height > inner.height() + 1.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                inner.width(),
                height,
            )));
        }
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Displays(Ok(displays)) => {
                    self.displays = Some(displays);
                    self.displays_error = None;
                }
                Msg::Displays(Err(e)) => {
                    self.displays = Some(Vec::new());
                    self.displays_error = Some(format!("Detection failed: {e}"));
                }
                Msg::Current { draft, result } => {
                    if let Some(draft) = self.drafts.iter_mut().find(|d| d.id == draft) {
                        match result {
                            Ok(v) => {
                                let name = draft
                                    .inputs
                                    .iter()
                                    .find(|row| ddc::matches(v, row.value))
                                    .map(|row| format!(" = {}", row.name))
                                    .unwrap_or_default();
                                draft.current_text = if let Some(status) = ddc::status_name(v) {
                                    // AOC monitors report DVI/HDMI as a single status, not an
                                    // input code, so there is nothing unambiguous to check.
                                    format!("Current: {status}; raw reply {v} (0x{v:02X})")
                                } else {
                                    format!("Current: {v} (0x{v:02X}){name}")
                                };
                            }
                            Err(e) => draft.current_text = format!("Error: {e}"),
                        }
                    }
                }
                Msg::Capabilities { draft, result } => {
                    if !self.drafts.iter().any(|d| d.id == draft) {
                        continue;
                    }
                    if let Some(draft) = self.drafts.iter_mut().find(|d| d.id == draft) {
                        draft.detecting = false;
                    }
                    match result {
                        Ok(caps) => self.add_detected_inputs(draft, &caps),
                        Err(e) => {
                            if let Some(draft) = self.drafts.iter_mut().find(|d| d.id == draft) {
                                draft.inputs_text =
                                    format!("Could not read the monitor's capabilities: {e}");
                            }
                        }
                    }
                }
                Msg::Tested { draft, row, result } => {
                    if let Some(draft) = self.drafts.iter_mut().find(|d| d.id == draft) {
                        if let Some(row) = draft.inputs.get_mut(row) {
                            row.testing = false;
                        }
                    }
                    if let Err(e) = result {
                        self.error = Some(format!("Switch failed: {e}"));
                    }
                }
                Msg::Exported(result) => {
                    self.file_dialog = false;
                    match result {
                        Ok(Some(path)) => {
                            self.error = None;
                            self.status = Some(format!("Settings exported to {}", path.display()));
                        }
                        Ok(None) => {}
                        Err(e) => self.error = Some(e),
                    }
                }
                Msg::Imported(result) => {
                    self.file_dialog = false;
                    match result {
                        Ok(Some((path, config))) => {
                            self.load_form(config);
                            self.error = None;
                            self.status = Some(format!(
                                "Imported {}. Review the settings, then click Save to apply them.",
                                path.display()
                            ));
                        }
                        Ok(None) => {}
                        Err(e) => self.error = Some(e),
                    }
                }
            }
        }
        self.handle_capture(&ctx);

        // Save and Cancel live in their own bottom panel so they stay visible however tall the rest
        // gets; the rest scrolls if the window is still too small.
        egui::Panel::bottom("buttons").show(ui, |ui| {
            ui.add_space(4.0);
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            } else if let Some(s) = &self.status {
                ui.label(s);
            }
            // Align::Min, not Center: a centered row claims the whole height the panel offers, and
            // the auto-sized panel then grows a little every frame until it fills the window.
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                ui.add_enabled_ui(!self.file_dialog, |ui| {
                    if ui
                        .button("Import...")
                        .on_hover_text("Load settings exported from this or another computer")
                        .clicked()
                    {
                        self.import(&ctx);
                    }
                    if ui
                        .button("Export...")
                        .on_hover_text("Save the settings shown here to a file")
                        .clicked()
                    {
                        self.export(&ctx);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if ui.button("Cancel").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.button("Save").clicked() {
                        self.save(&ctx);
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let scroll = egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.strong("Monitors");
                    let labels: Vec<String> =
                        (0..self.drafts.len()).map(|i| self.draft_label(i)).collect();
                    let available = self.available_displays();
                    let displays_ready = self.displays.is_some();
                    let mut selected = self.selected;
                    let mut add: Option<Option<DisplayInfo>> = None;
                    ui.horizontal_wrapped(|ui| {
                        egui::ComboBox::from_id_salt("monitor")
                            .width(260.0)
                            .selected_text(
                                labels.get(self.selected).cloned().unwrap_or_default(),
                            )
                            .show_ui(ui, |ui| {
                                for (i, label) in labels.iter().enumerate() {
                                    ui.selectable_value(&mut selected, i, label);
                                }
                            });
                        egui::ComboBox::from_id_salt("add_monitor")
                            .width(200.0)
                            .selected_text("Add monitor...")
                            .show_ui(ui, |ui| {
                                if !displays_ready {
                                    ui.label("Detecting displays...");
                                }
                                for display in &available {
                                    if ui.selectable_label(false, display.label()).clicked() {
                                        add = Some(Some(display.clone()));
                                    }
                                }
                                if ui.selectable_label(false, "Blank monitor").clicked() {
                                    add = Some(None);
                                }
                            });
                        if ui.button("Refresh").clicked() {
                            self.load_monitors(&ctx);
                        }
                    });
                    if selected != self.selected {
                        self.selected = selected;
                        self.set_capture(None);
                    }
                    if let Some(choice) = add {
                        self.add_monitor(&ctx, choice.as_ref());
                    }
                    ui.horizontal_wrapped(|ui| {
                        let only_one = self.drafts.len() <= 1;
                        if ui
                            .add_enabled(!only_one, egui::Button::new("Remove monitor"))
                            .on_hover_text("At least one monitor is required")
                            .clicked()
                        {
                            self.remove_selected();
                        }
                        if ui.button("Move up").clicked() {
                            self.move_selected(true);
                        }
                        if ui.button("Move down").clicked() {
                            self.move_selected(false);
                        }
                    });
                    let options = self.display_options();
                    let mut chosen = self.picker_target();
                    let mut changed = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Detected display:");
                        egui::ComboBox::from_id_salt("target")
                            .width(330.0)
                            .selected_text(
                                options
                                    .iter()
                                    .find(|(_, target)| *target == chosen)
                                    .map(|(label, _)| label.clone())
                                    .unwrap_or_else(|| "(not detected)".into()),
                            )
                            .show_ui(ui, |ui| {
                                for (label, target) in &options {
                                    if ui
                                        .selectable_value(&mut chosen, target.clone(), label)
                                        .changed()
                                    {
                                        changed = true;
                                    }
                                }
                            });
                        if ui.button("Read current input").clicked() {
                            let target = self.target();
                            if let Some(draft) = self.drafts.get_mut(self.selected) {
                                draft.current_text = "Reading...".into();
                            }
                            let draft = self.draft_id(self.selected);
                            self.spawn(&ctx, move || Msg::Current {
                                draft,
                                result: get_input(target),
                            });
                        }
                    });
                    if changed {
                        if let Some(draft) = self.drafts.get_mut(self.selected) {
                            draft.name = clean_target_part(&chosen.0);
                            draft.device = clean_target_part(&chosen.1);
                            draft.current_text.clear();
                            draft.inputs_text.clear();
                        }
                    }
                    if let Some(error) = &self.displays_error {
                        ui.label(error);
                    }
                    if let Some(draft) = self.drafts.get(self.selected) {
                        if !draft.current_text.is_empty() {
                            ui.label(&draft.current_text);
                        }
                    }
                });

                let selected = self.selected;
                let draft_id = self.draft_id(selected);
                let target = self.target();
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.strong("Inputs");
                    let mut remove = None;
                    let rows = self
                        .drafts
                        .get(selected)
                        .map_or(0, |draft| draft.inputs.len());
                    egui::Grid::new("inputs")
                        .num_columns(6)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            for header in ["Name", "Value", "Shortcut", "In Next", "", ""] {
                                ui.strong(header);
                            }
                            ui.end_row();
                            for i in 0..rows {
                                {
                                    let draft = &mut self.drafts[selected];
                                    ui.add_sized(
                                        [160.0, 22.0],
                                        egui::TextEdit::singleline(&mut draft.inputs[i].name),
                                    );
                                    let standard = ddc::input_name(draft.inputs[i].value);
                                    ui.add(
                                        egui::DragValue::new(&mut draft.inputs[i].value)
                                            .range(0..=0xFFFF),
                                    )
                                    .on_hover_text(format!(
                                        "Standard name for this code: {standard} (your monitor may label it differently)"
                                    ));
                                }
                                self.hotkey_button(ui, Capture::Row(i));
                                let mut switch_now = false;
                                {
                                    let draft = &mut self.drafts[selected];
                                    ui.checkbox(&mut draft.inputs[i].in_next, "").on_hover_text(
                                        "Include this input when cycling with \"Next input\". Untick inputs with nothing plugged in.",
                                    );
                                    let testing = draft.inputs[i].testing;
                                    if ui
                                        .add_enabled(!testing, egui::Button::new("Switch now"))
                                        .clicked()
                                    {
                                        draft.inputs[i].testing = true;
                                        switch_now = true;
                                    }
                                    if ui.button("Remove").clicked() {
                                        remove = Some(i);
                                    }
                                }
                                if switch_now {
                                    let value = self.drafts[selected].inputs[i].value;
                                    let target = target.clone();
                                    self.spawn(&ctx, move || Msg::Tested {
                                        draft: draft_id,
                                        row: i,
                                        result: set_input(target, value),
                                    });
                                }
                                ui.end_row();
                            }
                        });
                    if let Some(i) = remove {
                        self.drafts[selected].inputs.remove(i);
                        self.set_capture(None);
                    }
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Add input").clicked() {
                            self.drafts[selected].inputs.push(Row {
                                name: "New input".into(),
                                value: 0,
                                hotkey: None,
                                in_next: true,
                                testing: false,
                            });
                        }
                        let detecting = self
                            .drafts
                            .get(selected)
                            .is_some_and(|draft| draft.detecting);
                        let detect = egui::Button::new(if detecting {
                            "Detecting..."
                        } else {
                            "Detect inputs"
                        });
                        if ui
                            .add_enabled(!detecting, detect)
                            .on_hover_text(
                                "Ask the selected monitor which inputs it has, and add the missing ones",
                            )
                            .clicked()
                        {
                            if let Some(draft) = self.drafts.get_mut(selected) {
                                draft.detecting = true;
                                draft.inputs_text.clear();
                            }
                            let target = target.clone();
                            self.spawn(&ctx, move || Msg::Capabilities {
                                draft: draft_id,
                                result: capabilities(target),
                            });
                        }
                    });
                    if let Some(draft) = self.drafts.get(selected) {
                        if !draft.inputs_text.is_empty() {
                            ui.label(&draft.inputs_text);
                        }
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Next-input shortcut (this monitor):");
                        self.hotkey_button(ui, Capture::Toggle);
                    });
                    ui.weak(format!(
                        "Click a name to rename it. Value = the input code (VCP 0x60) sent to the monitor; hover it for the standard name. \
                         Each monitor has its own input list and Next shortcut. \"Next input\" only cycles through inputs ticked In Next.{}",
                        platform::SHORTCUTS_HINT
                    ));
                });

                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.strong("Options");
                    ui.checkbox(&mut self.autostart, platform::START_WITH_LOGIN);
                    ui.checkbox(&mut self.notifications, "Show a notification after switching");
                    ui.checkbox(
                        &mut self.startup_notification,
                        "Show a notification when the app starts",
                    );
                });
            });
            self.fit_to_content(&ctx, scroll.content_size.y, scroll.inner_rect.height());
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.capture.is_some() {
            platform::block_shortcuts_for_capture(false);
        }
    }
}

pub fn run() -> i32 {
    let icon = image::load_from_memory(ICON_PNG).ok().map(|img| {
        let rgba = img.to_rgba8();
        egui::IconData {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        }
    });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("{APP_NAME} - Settings"))
        .with_app_id(crate::config::APP_ID)
        .with_inner_size([820.0, 520.0])
        .with_min_inner_size([720.0, 420.0]);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    match eframe::run_native(
        &format!("{APP_NAME} - Settings"),
        options,
        Box::new(|cc| Ok(Box::new(SettingsApp::new(&cc.egui_ctx)))),
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Could not open the Settings window: {e}");
            1
        }
    }
}
