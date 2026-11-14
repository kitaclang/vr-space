//! What the user can ask for, from a key in the window, the menu bar or a global shortcut.

/// Something the user asked for, from a key in the window, the menu bar or a global shortcut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Recenter,
    ToggleHorizon,
    RollLeft,
    RollRight,
    Further,
    Closer,
    Bigger,
    Smaller,
    ResetScreen,
    /// Set screen curvature (0 = flat, 1 = wrapped around the eye).
    SetCurve(f32),
    ToggleFullscreen,
    /// Go fullscreen on this display, or back to a window with `None`.
    FullscreenOn(Option<u32>),
    /// Capture this display instead.
    ShowDisplay(u32),
    /// Capture this window instead, by window ID.
    ShowWindow(u32),
    ToggleHud,
    OpenScreenSettings,
    /// Turn opening at login on or off (only offered when running as vr-space.app).
    ToggleOpenAtLogin,
    /// Turn head-pose prediction on or off.
    TogglePrediction,
    /// Predict one step further ahead / less far ahead.
    PredictMore,
    PredictLess,
    /// Switch between the GPU reading captured frames in place and copying them.
    ToggleFrameCopying,
    Quit,
}

/// The command for a key pressed while the vr-space window has focus. Held keys repeat, so
/// the screen moves smoothly.
pub fn command_for_key(key: winit::event::VirtualKeyCode) -> Option<Command> {
    use winit::event::VirtualKeyCode as K;
    Some(match key {
        K::Escape => Command::Quit,
        K::C => Command::Recenter,
        K::Left => Command::RollLeft,
        K::Right => Command::RollRight,
        K::F => Command::ToggleFullscreen,
        K::H => Command::ToggleHud,
        K::P => Command::OpenScreenSettings,
        K::Up => Command::Further,
        K::Down => Command::Closer,
        K::Equals | K::Plus | K::NumpadAdd => Command::Bigger,
        K::Minus | K::NumpadSubtract => Command::Smaller,
        K::R => Command::ResetScreen,
        K::X => Command::TogglePrediction,
        K::RBracket => Command::PredictMore,
        K::LBracket => Command::PredictLess,
        K::Z => Command::ToggleFrameCopying,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::event::VirtualKeyCode as K;

    #[test]
    fn keys_match_the_readme() {
        for (key, cmd) in [
            (K::Escape, Command::Quit),
            (K::C, Command::Recenter),
            (K::Left, Command::RollLeft),
            (K::Right, Command::RollRight),
            (K::F, Command::ToggleFullscreen),
            (K::H, Command::ToggleHud),
            (K::P, Command::OpenScreenSettings),
            (K::Up, Command::Further),
            (K::Down, Command::Closer),
            (K::Equals, Command::Bigger),
            (K::NumpadAdd, Command::Bigger),
            (K::Minus, Command::Smaller),
            (K::R, Command::ResetScreen),
            (K::X, Command::TogglePrediction),
            (K::RBracket, Command::PredictMore),
            (K::LBracket, Command::PredictLess),
            (K::Z, Command::ToggleFrameCopying),
        ] {
            assert_eq!(command_for_key(key), Some(cmd), "{key:?}");
        }
        assert_eq!(command_for_key(K::Q), None, "unused keys do nothing");
        assert_eq!(command_for_key(K::Space), None);
    }
}
