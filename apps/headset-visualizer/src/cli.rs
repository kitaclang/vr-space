//! Command-line flags, and how they combine with saved settings at startup.

use clap::parser::ValueSource;
use clap::{ArgMatches, Parser};

use crate::settings::{SavedWindow, Settings};
use crate::scene::ScreenPlacement;

// ================= CLI =================

#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Headset pose visualizer (wgpu + winit, Metal on macOS)")]
pub struct Cli {
    /// Enable vsync (off by default for lowest latency)
    #[arg(long, default_value_t = false)]
    pub vsync: bool,

    /// Invert X (roll)
    #[arg(long, default_value_t = false)]
    pub invert_x: bool,

    /// Invert Y (pitch)
    #[arg(long, default_value_t = false)]
    pub invert_y: bool,

    /// Invert Z (yaw)
    #[arg(long, default_value_t = false)]
    pub invert_z: bool,

    /// Poll interval for headset fusion update in milliseconds (0 = as fast as possible)
    #[arg(long, default_value_t = 0)]
    pub poll_ms: u64,

    /// Disable HUD text to save GPU/CPU
    #[arg(long, default_value_t = false)]
    pub no_hud: bool,

    /// HUD font size in logical pixels
    #[arg(long, default_value_t = 16.0)]
    pub hud_size: f32,

    /// When vsync is off, target max FPS by pacing the CPU (0 = unlimited)
    #[arg(long, default_value_t = 0)]
    pub max_fps: u32,

    /// Display to show on the virtual screen, by ID from --list-displays (0 = main display)
    #[arg(long, default_value_t = 0)]
    pub display: u32,

    /// List the displays that can be captured, then exit
    #[arg(long, default_value_t = false)]
    pub list_displays: bool,

    /// Show a single window instead of a display: a window ID from --list-windows, or an app
    /// name or part of a window title (for example Safari)
    #[arg(long, value_name = "ID|NAME", conflicts_with = "display")]
    pub window: Option<String>,

    /// List the windows that can be captured, then exit
    #[arg(long, default_value_t = false)]
    pub list_windows: bool,

    /// Height of the virtual screen in scene units (one grid square = 1 unit)
    #[arg(long, default_value_t = 1.4)]
    pub screen_height: f32,

    /// Distance from your eyes to the virtual screen in scene units
    #[arg(long, default_value_t = 4.0)]
    pub screen_distance: f32,

    /// Vertical field of view in degrees
    #[arg(long, default_value_t = 60.0)]
    pub fov: f32,

    /// Open fullscreen on this display, by ID from --list-displays (use the glasses' display).
    /// 0 = main display. Press F in the app to toggle fullscreen.
    #[arg(long, value_name = "ID")]
    pub fullscreen: Option<u32>,

    /// Neck model strength: moves the eyes as if the head turns around the neck
    /// (1 = average head, 0 = off)
    #[arg(long, default_value_t = 1.0)]
    pub neck_model: f32,

    /// Hide the mouse cursor on the virtual screen
    #[arg(long)]
    pub no_cursor: bool,

    /// Predict head movement this many milliseconds ahead of when a frame is drawn, to make
    /// up for the time until it's on the glasses (0 = off; try 10 to 30)
    #[arg(long, default_value_t = 0.0)]
    pub predict_ms: f32,

    /// Curve the virtual screen around you: 0 = flat, 1 = every part of it the same distance
    /// from your eyes, like a curved monitor wrapped around your head
    #[arg(long, default_value_t = 0.0)]
    pub curve: f32,

    /// Copy each captured frame through the CPU instead of letting the GPU read it in place
    /// (slower; for comparison, or if the screen looks wrong)
    #[arg(long)]
    pub copy_frames: bool,

    /// Recenter only the left-right direction (yaw), keeping the horizon level even if
    /// your head is tilted when you recenter
    #[arg(long)]
    pub recenter_yaw_only: bool,

    /// Don't load or save settings (screen size and distance, display or window, fullscreen,
    /// HUD) in ~/Library/Application Support/vr-space/settings.json
    #[arg(long)]
    pub ignore_settings: bool,

    /// Write the app icon as PNGs into this .iconset folder, then exit (used by
    /// scripts/bundle-macos.sh)
    #[arg(long, value_name = "DIR", hide = true)]
    pub write_iconset: Option<std::path::PathBuf>,
}

/// Which of the flags that saved settings can stand in for were given on the command line.
/// Those win over the saved settings.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExplicitFlags {
    pub screen_distance: bool,
    pub screen_height: bool,
    /// `--display` or `--window`: either one decides what's shown.
    pub source: bool,
    pub fullscreen: bool,
    pub predict_ms: bool,
}

impl ExplicitFlags {
    pub fn from_matches(m: &ArgMatches) -> Self {
        let given = |id: &str| m.value_source(id) == Some(ValueSource::CommandLine);
        Self {
            screen_distance: given("screen_distance"),
            screen_height: given("screen_height"),
            source: given("display") || given("window"),
            fullscreen: given("fullscreen"),
            predict_ms: given("predict_ms"),
        }
    }
}

/// What to show at startup when it's a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowChoice {
    /// `--window`: an ID, app name or part of a title.
    Query(String),
    /// The window shown last time, found again by its app and title.
    Saved(SavedWindow),
}

/// How to start: the command line, with the saved settings filling in what it doesn't say.
#[derive(Clone, Debug)]
pub struct StartupPlan {
    pub screen: ScreenPlacement,
    /// Display to show, as on the command line (0 = main display). Also where to go back to
    /// if the window closes.
    pub display: u32,
    pub window: Option<WindowChoice>,
    pub fullscreen: Option<u32>,
    pub hud: bool,
    pub predict_ms: f32,
}

impl StartupPlan {
    pub fn new(cli: &Cli, explicit: ExplicitFlags, saved: &Settings) -> Self {
        let pick = |given: bool, flag: f32, saved: Option<f32>| if given { flag } else { saved.unwrap_or(flag) };
        let (display, window) = if explicit.source {
            (cli.display, cli.window.clone().map(WindowChoice::Query))
        } else {
            (saved.display.unwrap_or(cli.display), saved.window.clone().map(WindowChoice::Saved))
        };
        Self {
            screen: ScreenPlacement {
                distance: pick(explicit.screen_distance, cli.screen_distance, saved.screen_distance),
                height: pick(explicit.screen_height, cli.screen_height, saved.screen_height),
            },
            display,
            window,
            fullscreen: if explicit.fullscreen { cli.fullscreen } else { saved.fullscreen },
            hud: !cli.no_hud && saved.hud.unwrap_or(true),
            predict_ms: pick(explicit.predict_ms, cli.predict_ms, saved.predict_ms),
        }
    }
}

impl Cli {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !(self.screen_height > 0.0 && self.screen_height.is_finite()) {
            anyhow::bail!("--screen-height must be greater than 0");
        }
        if !(self.screen_distance > 0.0 && self.screen_distance.is_finite()) {
            anyhow::bail!("--screen-distance must be greater than 0");
        }
        if !(self.fov > 1.0 && self.fov < 179.0) {
            anyhow::bail!("--fov must be between 1 and 179 degrees");
        }
        if !(0.0..=3.0).contains(&self.neck_model) {
            anyhow::bail!("--neck-model must be between 0 and 3");
        }
        if !(0.0..=1.0).contains(&self.curve) {
            anyhow::bail!("--curve must be between 0 and 1");
        }
        if !(0.0..=100.0).contains(&self.predict_ms) {
            anyhow::bail!("--predict-ms must be between 0 and 100");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SavedWindow;
    use clap::{CommandFactory, FromArgMatches};

    fn parse(args: &[&str]) -> anyhow::Result<Cli> {
        let cli = Cli::try_parse_from(std::iter::once("headset-visualizer").chain(args.iter().copied()))?;
        cli.validate()?;
        Ok(cli)
    }

    #[test]
    fn default_flags_are_valid() {
        let cli = parse(&[]).unwrap();
        assert!(!cli.recenter_yaw_only);
    }

    fn plan(args: &[&str], saved: &Settings) -> StartupPlan {
        let argv: Vec<&str> = std::iter::once("headset-visualizer").chain(args.iter().copied()).collect();
        let matches = Cli::command().try_get_matches_from(argv).unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        StartupPlan::new(&cli, ExplicitFlags::from_matches(&matches), saved)
    }

    fn saved_all() -> Settings {
        Settings {
            screen_distance: Some(2.5),
            screen_height: Some(2.0),
            display: Some(7),
            window: Some(SavedWindow { app: "Safari".into(), title: "Docs".into() }),
            fullscreen: Some(9),
            hud: Some(false),
            predict_ms: Some(25.0),
        }
    }

    #[test]
    fn saved_settings_fill_in_when_no_flags_are_given() {
        let p = plan(&[], &saved_all());
        assert_eq!(p.screen, ScreenPlacement { distance: 2.5, height: 2.0 });
        assert_eq!(p.display, 7);
        assert_eq!(p.window, Some(WindowChoice::Saved(SavedWindow { app: "Safari".into(), title: "Docs".into() })));
        assert_eq!(p.fullscreen, Some(9));
        assert!(!p.hud);
        assert_eq!(p.predict_ms, 25.0);

        // Nothing saved: the defaults.
        let p = plan(&[], &Settings::default());
        assert_eq!(p.screen, ScreenPlacement { distance: 4.0, height: 1.4 });
        assert_eq!((p.display, p.window, p.fullscreen, p.hud), (0, None, None, true));
        assert_eq!(p.predict_ms, 0.0, "prediction is off by default");
    }

    #[test]
    fn flags_beat_saved_settings() {
        let p = plan(&["--screen-distance", "6", "--display", "3", "--fullscreen", "4", "--no-hud"], &saved_all());
        assert_eq!(p.screen, ScreenPlacement { distance: 6.0, height: 2.0 }, "height still saved");
        assert_eq!((p.display, p.window.clone()), (3, None), "--display drops the saved window");
        assert_eq!(p.fullscreen, Some(4));
        assert!(!p.hud);

        let p = plan(&["--window", "Notes"], &saved_all());
        assert_eq!(p.window, Some(WindowChoice::Query("Notes".into())));
        assert_eq!(p.display, 0, "not the saved display either");

        // A flag set to its default value still counts as given.
        assert_eq!(plan(&["--screen-height", "1.4"], &saved_all()).screen.height, 1.4);
        assert_eq!(plan(&["--predict-ms", "0"], &saved_all()).predict_ms, 0.0, "turned off on the command line");
    }

    #[test]
    fn window_and_display_are_exclusive() {
        assert_eq!(parse(&["--window", "Safari"]).unwrap().window.as_deref(), Some("Safari"));
        assert!(parse(&["--window", "Safari", "--display", "2"]).is_err());
    }

    #[test]
    fn bad_flag_values_are_rejected() {
        for args in [
            &["--screen-height", "0"][..],
            &["--screen-distance", "-1"],
            &["--fov", "180"],
            &["--fov", "1"],
            &["--neck-model", "3.5"],
            &["--screen-height", "NaN"],
            &["--predict-ms", "101"],
            &["--curve", "1.5"],
            &["--curve", "-0.1"],
            &["--predict-ms", "NaN"],
        ] {
            assert!(parse(args).is_err(), "accepted {args:?}");
        }
        assert!(parse(&["--predict-ms", "20"]).is_ok());
        assert!(parse(&["--curve", "0.5"]).is_ok());
    }
}
