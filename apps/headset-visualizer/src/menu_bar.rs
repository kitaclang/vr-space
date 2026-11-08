//! Menu bar icon with the app's controls, and keyboard shortcuts that work from any app.
//!
//! Both turn into `Command`s that are sent to the winit event loop through an
//! `EventLoopProxy`, so the render loop handles them like its own key presses. Nothing here
//! runs per frame: menu clicks and shortcuts are rare, and the display and window lists are
//! checked every couple of seconds (the window list is fetched by a background thread in
//! `macos_capture`).

use std::sync::Mutex;

use core_graphics::display::CGDisplay;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use log::warn;
use tray_icon::menu::accelerator::Accelerator;
use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::event_loop::EventLoopProxy;

use crate::macos_capture::{windows_if_changed, CaptureSource, WindowInfo};
use crate::Command;

/// Longest window label in the menu; longer titles are cut with an ellipsis.
const MAX_LABEL_CHARS: usize = 60;
/// Most windows listed, so the menu stays usable with many windows open.
const MAX_WINDOWS: usize = 40;

/// Shortcuts that work from any app: Control-Option plus a key.
fn shortcut_mods() -> Modifiers {
    Modifiers::CONTROL | Modifiers::ALT
}

/// (key, command, menu item id, menu label) for everything that has a global shortcut.
const SHORTCUTS: [(Code, Command, &str, &str); 6] = [
    (Code::KeyC, Command::Recenter, "recenter", "Recenter"),
    (Code::ArrowUp, Command::Further, "further", "Further away"),
    (Code::ArrowDown, Command::Closer, "closer", "Closer"),
    (Code::Equal, Command::Bigger, "bigger", "Bigger"),
    (Code::Minus, Command::Smaller, "smaller", "Smaller"),
    (Code::KeyR, Command::ResetScreen, "reset", "Reset size and distance"),
];

pub struct MenuBar {
    _tray: TrayIcon,
    _hotkeys: Option<GlobalHotKeyManager>,
    display_menu: Submenu,
    window_menu: Submenu,
    fullscreen_menu: Submenu,
    hud_item: CheckMenuItem,
    login_item: Option<CheckMenuItem>,
    displays: Vec<u32>,
    windows: Vec<WindowInfo>,
    windows_seq: u64,
    shown: CaptureSource,
    fullscreen: Option<u32>,
}

impl MenuBar {
    /// Create the menu bar icon and register the shortcuts. On macOS this must run on the main
    /// thread after the event loop has started (winit's `StartCause::Init`).
    ///
    /// `shown` is the captured display or window, `fullscreen` the display the window is
    /// fullscreen on.
    /// `open_at_login` is `None` when that setting isn't available (not running as an app).
    pub fn new(
        proxy: EventLoopProxy<Command>,
        shown: CaptureSource,
        fullscreen: Option<u32>,
        hud_on: bool,
        hud_available: bool,
        open_at_login: Option<bool>,
    ) -> anyhow::Result<Self> {
        let screen_menu = Submenu::new("Screen", true);
        for (code, _, id, label) in &SHORTCUTS[1..] {
            let accel = Accelerator::new(shortcut_mods(), *code);
            screen_menu.append(&MenuItem::with_id(*id, *label, true, Some(accel)))?;
        }

        let display_menu = Submenu::new("Show display", true);
        let window_menu = Submenu::new("Show window", true);
        let fullscreen_menu = Submenu::new("Fullscreen on", true);
        let hud_item = CheckMenuItem::with_id("hud", "Show HUD", hud_available, hud_on, None);
        let (recenter_code, _, recenter_id, recenter_label) = SHORTCUTS[0];
        let recenter = MenuItem::with_id(
            recenter_id,
            recenter_label,
            true,
            Some(Accelerator::new(shortcut_mods(), recenter_code)),
        );
        let settings = MenuItem::with_id("settings", "Screen Recording settings…", true, None);
        // Only when running as vr-space.app on macOS 13 or later.
        let login_item = open_at_login
            .map(|on| CheckMenuItem::with_id("login", "Open at Login", true, on, None));
        let quit = MenuItem::with_id("quit", "Quit vr-space", true, None);

        let menu = Menu::new();
        let items: [&dyn IsMenuItem; 9] = [
            &recenter,
            &screen_menu,
            &PredefinedMenuItem::separator(),
            &display_menu,
            &window_menu,
            &fullscreen_menu,
            &hud_item,
            &PredefinedMenuItem::separator(),
            &settings,
        ];
        menu.append_items(&items)?;
        if let Some(item) = &login_item {
            menu.append(item)?;
        }
        menu.append_items(&[&PredefinedMenuItem::separator(), &quit])?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon_templated(menu_bar_icon()?)
            .with_tooltip("vr-space")
            .build()?;

        // Menu clicks arrive on the main thread; forward them to the event loop. The proxy is
        // only Send, and the handler must be Sync, hence the mutex (locked once per click).
        let menu_proxy = Mutex::new(proxy.clone());
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if let Some(cmd) = command_for_menu_id(&event.id.0) {
                if let Ok(p) = menu_proxy.lock() {
                    let _ = p.send_event(cmd);
                }
            }
        }));

        let hotkeys = register_shortcuts(proxy);

        let mut bar = Self {
            _tray: tray,
            _hotkeys: hotkeys,
            display_menu,
            window_menu,
            fullscreen_menu,
            hud_item,
            login_item,
            displays: Vec::new(),
            windows: Vec::new(),
            windows_seq: 0,
            shown,
            fullscreen,
        };
        bar.refresh();
        bar.rebuild_window_menu();
        Ok(bar)
    }

    /// Rebuild the display submenus if a display was connected or removed (for example the
    /// glasses being plugged in), and the window submenu if windows opened, closed or changed
    /// title. Cheap and never blocks; call it every second or two.
    pub fn refresh(&mut self) {
        let ids = CGDisplay::active_displays().unwrap_or_default();
        if ids != self.displays {
            self.displays = ids;
            self.rebuild_display_menus();
        }
        if let Some(windows) = windows_if_changed(&mut self.windows_seq) {
            self.windows = windows;
            self.rebuild_window_menu();
        }
    }

    /// Update the check marks after the app's state changed.
    pub fn set_state(&mut self, shown: CaptureSource, fullscreen: Option<u32>, hud_on: bool) {
        self.hud_item.set_checked(hud_on);
        if (shown, fullscreen) != (self.shown, self.fullscreen) {
            let window_changed = matches!(shown, CaptureSource::Window(_)) || matches!(self.shown, CaptureSource::Window(_));
            self.shown = shown;
            self.fullscreen = fullscreen;
            self.rebuild_display_menus();
            if window_changed {
                self.rebuild_window_menu();
            }
        }
    }

    fn rebuild_window_menu(&self) {
        let menu = &self.window_menu;
        while menu.remove_at(0).is_some() {}
        if self.windows.is_empty() {
            let _ = menu.append(&MenuItem::new("No windows to show", false, None));
            return;
        }
        for w in self.windows.iter().take(MAX_WINDOWS) {
            let _ = menu.append(&CheckMenuItem::with_id(
                format!("window:{}", w.id),
                shorten(&w.label(), MAX_LABEL_CHARS),
                true,
                self.shown == CaptureSource::Window(w.id),
                None,
            ));
        }
        if self.windows.len() > MAX_WINDOWS {
            let more = format!("{} more (use --list-windows)", self.windows.len() - MAX_WINDOWS);
            let _ = menu.append(&MenuItem::new(more, false, None));
        }
    }

    /// Update the Open at Login check mark (`None`: the setting couldn't be read).
    pub fn set_open_at_login(&mut self, on: Option<bool>) {
        if let (Some(item), Some(on)) = (&self.login_item, on) {
            item.set_checked(on);
        }
    }

    fn rebuild_display_menus(&self) {
        for menu in [&self.display_menu, &self.fullscreen_menu] {
            while menu.remove_at(0).is_some() {}
        }
        let main_id = CGDisplay::main().id;
        for &id in &self.displays {
            let label = display_label(id, main_id);
            let _ = self.display_menu.append(&CheckMenuItem::with_id(
                format!("display:{id}"),
                &label,
                true,
                self.shown == CaptureSource::Display(id),
                None,
            ));
            let _ = self.fullscreen_menu.append(&CheckMenuItem::with_id(
                format!("fullscreen:{id}"),
                &label,
                true,
                Some(id) == self.fullscreen,
                None,
            ));
        }
        let _ = self.fullscreen_menu.append(&PredefinedMenuItem::separator());
        let _ = self.fullscreen_menu.append(&CheckMenuItem::with_id(
            "fullscreen:window",
            "Not fullscreen (window)",
            true,
            self.fullscreen.is_none(),
            None,
        ));
    }
}

fn register_shortcuts(proxy: EventLoopProxy<Command>) -> Option<GlobalHotKeyManager> {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            warn!("Keyboard shortcuts are unavailable: {e}");
            return None;
        }
    };
    let mut bindings = Vec::new();
    for (code, cmd, _, label) in SHORTCUTS {
        let hotkey = HotKey::new(Some(shortcut_mods()), code);
        match manager.register(hotkey) {
            Ok(()) => bindings.push((hotkey.id(), cmd)),
            // Usually another app already uses the same shortcut.
            Err(e) => warn!("Couldn't register the shortcut for \"{label}\": {e}"),
        }
    }
    let proxy = Mutex::new(proxy);
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state != HotKeyState::Pressed {
            return;
        }
        if let Some(&(_, cmd)) = bindings.iter().find(|(id, _)| *id == event.id) {
            if let Ok(p) = proxy.lock() {
                let _ = p.send_event(cmd);
            }
        }
    }));
    Some(manager)
}

fn command_for_menu_id(id: &str) -> Option<Command> {
    if let Some(&(_, cmd, _, _)) = SHORTCUTS.iter().find(|(_, _, item, _)| *item == id) {
        return Some(cmd);
    }
    match id {
        "hud" => Some(Command::ToggleHud),
        "settings" => Some(Command::OpenScreenSettings),
        "login" => Some(Command::ToggleOpenAtLogin),
        "quit" => Some(Command::Quit),
        "fullscreen:window" => Some(Command::FullscreenOn(None)),
        _ => {
            if let Some(n) = id.strip_prefix("display:") {
                n.parse().ok().map(Command::ShowDisplay)
            } else if let Some(n) = id.strip_prefix("window:") {
                n.parse().ok().map(Command::ShowWindow)
            } else if let Some(n) = id.strip_prefix("fullscreen:") {
                n.parse().ok().map(|n| Command::FullscreenOn(Some(n)))
            } else {
                None
            }
        }
    }
}

/// Cut `label` to at most `max` characters, ending with an ellipsis if it was cut.
fn shorten(label: &str, max: usize) -> String {
    if label.chars().count() <= max {
        return label.to_string();
    }
    let mut cut: String = label.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn display_label(id: u32, main_id: u32) -> String {
    let (w, h) = CGDisplay::new(id)
        .display_mode()
        .map(|m| (m.pixel_width(), m.pixel_height()))
        .unwrap_or((0, 0));
    let main = if id == main_id { " (main)" } else { "" };
    format!("Display {id}: {w}x{h}{main}")
}

/// A small pair of glasses, drawn in code (see `icon.rs`) so there's no image asset to license. It's a
/// template image, so macOS colours it to match the menu bar. 80x44 px = 40x22 pt on Retina.
fn menu_bar_icon() -> anyhow::Result<Icon> {
    const W: usize = crate::icon::GLASSES_SIZE.0 as usize;
    const H: usize = crate::icon::GLASSES_SIZE.1 as usize;
    let mut rgba = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let (d, _) = crate::icon::glasses(x as f32 + 0.5, y as f32 + 0.5);
            let alpha = (0.5 - d).clamp(0.0, 1.0);
            rgba[(y * W + x) * 4 + 3] = (alpha * 255.0) as u8;
        }
    }
    Ok(Icon::from_rgba(rgba, W as u32, H as u32)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_map_to_commands() {
        assert_eq!(command_for_menu_id("window:123"), Some(Command::ShowWindow(123)));
        assert_eq!(command_for_menu_id("display:7"), Some(Command::ShowDisplay(7)));
        assert_eq!(command_for_menu_id("fullscreen:7"), Some(Command::FullscreenOn(Some(7))));
        assert_eq!(command_for_menu_id("fullscreen:window"), Some(Command::FullscreenOn(None)));
        assert_eq!(command_for_menu_id("window:abc"), None);
    }

    #[test]
    fn long_labels_are_shortened() {
        assert_eq!(shorten("Safari: Docs", 60), "Safari: Docs");
        assert_eq!(shorten("abcdef", 4), "abc…");
        assert_eq!(shorten("ไทยภาษา", 4), "ไทย…", "counts characters, not bytes");
    }
}
