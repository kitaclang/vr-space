//! Finding displays and filling one with the window (fullscreen done by hand on macOS).

use log::warn;
use winit::window::Window;
#[cfg(not(target_os = "macos"))]
use winit::window::Fullscreen;

// Find a monitor by display ID (CGDirectDisplayID, as shown by --list-displays). 0 = main display.
pub fn find_monitor(window: &Window, id: u32) -> Option<winit::monitor::MonitorHandle> {
    if id == 0 {
        return window.primary_monitor();
    }
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::MonitorHandleExtMacOS;
        window.available_monitors().find(|m| m.native_id() == id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// The display ID of a winit monitor (0 where it isn't known).
pub fn monitor_id(monitor: &winit::monitor::MonitorHandle) -> u32 {
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::MonitorHandleExtMacOS;
        monitor.native_id()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = monitor;
        0
    }
}

/// Fill display `id` with the window, or go back to a normal window with `None`.
/// Returns the display the window now fills (unchanged if `id` wasn't found).
///
/// On macOS this doesn't use native fullscreen (winit's `Fullscreen`): that needs its own
/// Space, which macOS doesn't reliably give a window on a secondary display such as the
/// glasses, and never gives an app without a Dock icon. Instead the window loses its border,
/// covers the display exactly, sits above the menu bar and stays on every Space.
pub fn apply_fullscreen(window: &Window, current: Option<u32>, id: Option<u32>) -> Option<u32> {
    match id {
        None => {
            #[cfg(target_os = "macos")]
            {
                set_above_menu_bar(window, false);
                window.set_decorations(true);
                window.set_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
            }
            #[cfg(not(target_os = "macos"))]
            window.set_fullscreen(None);
            None
        }
        Some(id) => match find_monitor(window, id) {
            Some(monitor) => {
                let actual = monitor_id(&monitor);
                #[cfg(target_os = "macos")]
                {
                    // Raise it first: macOS keeps normal-level windows below the menu bar.
                    set_above_menu_bar(window, true);
                    window.set_decorations(false);
                    window.set_outer_position(monitor.position());
                    window.set_inner_size(monitor.size());
                }
                #[cfg(not(target_os = "macos"))]
                window.set_fullscreen(Some(Fullscreen::Borderless(Some(monitor))));
                Some(actual)
            }
            None => {
                warn!("Display {id} not found; staying as it is.");
                current
            }
        },
    }
}

/// Raise the window above the menu bar and keep it on every Space of its display, or put it
/// back to a normal window.
#[cfg(target_os = "macos")]
pub fn set_above_menu_bar(window: &Window, on: bool) {
    use objc2::runtime::AnyObject;
    use objc2::msg_send;
    use winit::platform::macos::WindowExtMacOS;

    // NSStatusWindowLevel (25) + 1: above the menu bar and its status items, below pop-up menus.
    const ABOVE_MENU_BAR: isize = 26;
    const NORMAL_LEVEL: isize = 0;
    // NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorStationary
    const ALL_SPACES: usize = (1 << 0) | (1 << 4);

    let ns_window = window.ns_window() as *const AnyObject;
    if ns_window.is_null() {
        return;
    }
    // SAFETY: ns_window is the live NSWindow behind `window`, used on the main thread.
    unsafe {
        let ns_window = &*ns_window;
        let level = if on { ABOVE_MENU_BAR } else { NORMAL_LEVEL };
        let _: () = msg_send![ns_window, setLevel: level];
        let behavior: usize = if on { ALL_SPACES } else { 0 };
        let _: () = msg_send![ns_window, setCollectionBehavior: behavior];
    }
}
