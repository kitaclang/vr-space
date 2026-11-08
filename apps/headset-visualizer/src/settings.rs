//! Settings saved between runs: what you change in the app (screen distance and size, the
//! display or window shown, the display to go fullscreen on, the HUD), so an app opened from
//! Finder or at login, which gets no command-line flags, starts the way you left it.
//!
//! Stored as JSON in `~/Library/Application Support/vr-space/settings.json`. Every field is
//! optional: a missing one means "use the command-line value or its default". Saving happens
//! only after you change something (a key, a shortcut or a menu click), never per frame.

use std::path::{Path, PathBuf};

use log::{info, warn};
use serde::{Deserialize, Serialize};

/// A window, remembered by its app and title: window IDs change when the app restarts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWindow {
    pub app: String,
    pub title: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Distance to the virtual screen, in scene units.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_distance: Option<f32>,
    /// Height of the virtual screen, in scene units.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_height: Option<f32>,
    /// The display shown on the virtual screen (or gone back to if the window closes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<u32>,
    /// The window shown instead of the display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<SavedWindow>,
    /// The display to go fullscreen on (normally the glasses). `None`: a normal window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<u32>,
    /// Whether the HUD is shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hud: Option<bool>,
    /// How far ahead head movement is predicted, in ms (0 = off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predict_ms: Option<f32>,
}

impl Settings {
    /// Parse settings, dropping values that are out of range (a hand-edited file, say).
    pub fn from_json(text: &str) -> serde_json::Result<Self> {
        let mut s: Settings = serde_json::from_str(text)?;
        let positive = |v: Option<f32>| v.filter(|x| x.is_finite() && *x > 0.0);
        s.screen_distance = positive(s.screen_distance);
        s.screen_height = positive(s.screen_height);
        s.predict_ms = s.predict_ms.filter(|x| (0.0..=100.0).contains(x));
        if s.window.as_ref().is_some_and(|w| w.app.is_empty()) {
            s.window = None;
        }
        Ok(s)
    }

    pub fn to_json(&self) -> String {
        // Serialising plain numbers, strings and bools can't fail.
        serde_json::to_string_pretty(self).unwrap_or_default() + "\n"
    }
}

/// `~/Library/Application Support/vr-space/settings.json`.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join("Library/Application Support/vr-space/settings.json"))
}

/// Loads the settings at startup and writes them back when they change.
pub struct SettingsStore {
    /// `None`: settings are off (`--ignore-settings`, or no home folder).
    path: Option<PathBuf>,
    saved: Settings,
}

impl SettingsStore {
    /// Open the store at `path` and load what's there. A missing file gives empty settings;
    /// an unreadable one is reported and replaced the next time something is saved.
    pub fn open(path: Option<PathBuf>) -> (Self, Settings) {
        let saved = match &path {
            None => Settings::default(),
            Some(p) => match std::fs::read_to_string(p) {
                Ok(text) => Settings::from_json(&text).unwrap_or_else(|e| {
                    warn!("Ignoring the saved settings in {}: {e}", p.display());
                    Settings::default()
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
                Err(e) => {
                    warn!("Couldn't read the saved settings in {}: {e}", p.display());
                    Settings::default()
                }
            },
        };
        if let (Some(p), true) = (&path, saved != Settings::default()) {
            info!("Loaded saved settings from {}", p.display());
        }
        (Self { path, saved: saved.clone() }, saved)
    }

    /// A store that never reads or writes anything.
    pub fn disabled() -> (Self, Settings) {
        (Self { path: None, saved: Settings::default() }, Settings::default())
    }

    /// Write `settings` if they differ from what was last saved. The file is replaced in one
    /// step (write a temporary file, then rename), so a crash can't leave half a file.
    pub fn save(&mut self, settings: &Settings) {
        let Some(path) = &self.path else { return };
        if *settings == self.saved {
            return;
        }
        match write_atomically(path, &settings.to_json()) {
            Ok(()) => self.saved = settings.clone(),
            Err(e) => warn!("Couldn't save settings to {}: {e}", path.display()),
        }
    }
}

fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_leaves_out_unset_values() {
        let s = Settings {
            screen_distance: Some(3.0),
            window: Some(SavedWindow { app: "Safari".into(), title: "Docs".into() }),
            fullscreen: Some(2),
            ..Settings::default()
        };
        let json = s.to_json();
        assert!(!json.contains("screen_height"), "{json}");
        assert_eq!(Settings::from_json(&json).unwrap(), s);
        assert_eq!(Settings::from_json("{}").unwrap(), Settings::default());
    }

    #[test]
    fn drops_bad_values_and_ignores_unknown_ones() {
        let s = Settings::from_json(
            r#"{"screen_distance": -2, "screen_height": 0, "window": {"app": ""}, "hud": false, "predict_ms": 300, "future": 1}"#,
        )
        .unwrap();
        assert_eq!(s, Settings { hud: Some(false), ..Settings::default() });
        assert!(Settings::from_json("not json").is_err());
        assert_eq!(Settings::from_json(r#"{"predict_ms": -1}"#).unwrap().predict_ms, None);
        assert_eq!(Settings::from_json(r#"{"predict_ms": 15}"#).unwrap().predict_ms, Some(15.0));
    }

    #[test]
    fn saves_only_changes_and_reloads_them() {
        let dir = std::env::temp_dir().join(format!("vr-space-settings-test-{}", std::process::id()));
        let path = dir.join("nested/settings.json");
        let _ = std::fs::remove_dir_all(&dir);

        let (mut store, loaded) = SettingsStore::open(Some(path.clone()));
        assert_eq!(loaded, Settings::default(), "no file yet");
        store.save(&Settings::default());
        assert!(!path.exists(), "nothing changed, nothing written");

        let s = Settings { screen_height: Some(2.5), display: Some(7), ..Settings::default() };
        store.save(&s);
        assert_eq!(SettingsStore::open(Some(path.clone())).1, s);

        std::fs::write(&path, "{ broken").unwrap();
        assert_eq!(SettingsStore::open(Some(path.clone())).1, Settings::default());

        let (mut off, _) = SettingsStore::disabled();
        off.save(&s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
