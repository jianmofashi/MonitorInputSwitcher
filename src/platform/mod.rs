//! Everything that differs between Linux and Windows lives behind this module. Each OS module
//! provides the same items:
//!
//! - `Ui`: tray icon + menu (`TrayModel`, one entry per configured monitor), notifications, and
//!   global shortcuts (`start`, `update`, `notify`, `register_shortcuts`, `suspend_shortcuts`,
//!   `shutdown`)
//! - `block_shortcuts_for_capture`: lets the Settings window record a combination that is in use
//! - `ddc`: `list_displays`, `open`, `is_device`, and `Bus` (`get_vcp`, `set_vcp`, `capabilities`)
//! - `ipc`: `instance_lock`, `connect`, `listen`, `cleanup` (single instance + CLI transport)
//! - `config_dir`, `autostart`, `META_NAME`, `START_WITH_LOGIN`, `attach_console`

use crate::hotkey::Hotkey;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// What one configured monitor looks like in the tray.
#[derive(Clone, Debug, Default)]
pub struct TrayMonitor {
    pub label: String,
    /// (name, shortcut, checked = current input)
    pub inputs: Vec<(String, Option<Hotkey>, bool)>,
    pub toggle: Option<Hotkey>,
}

/// What the tray menu shows.
#[derive(Clone, Debug, Default)]
pub struct TrayModel {
    pub monitors: Vec<TrayMonitor>,
    pub autostart: bool,
}

/// Something the user did in the tray menu or with a global shortcut.
#[derive(Debug)]
pub enum UiEvent {
    Switch { monitor: usize, input: usize },
    Next { monitor: usize },
    MenuOpened,
    SetAutostart(bool),
    OpenSettings,
    Quit,
    /// A global shortcut was pressed (action id).
    Hotkey(String),
    /// The user changed a shortcut outside the app (KDE System Settings).
    #[cfg_attr(windows, allow(dead_code))]
    HotkeyChanged(String, Option<Hotkey>),
}

/// A global shortcut the app owns.
#[derive(Clone, Debug)]
pub struct ShortcutAction {
    pub id: String,
    pub friendly: String,
    pub hotkey: Option<Hotkey>,
}

pub const ICON_PNG: &[u8] = include_bytes!("../../assets/icon-256.png");
