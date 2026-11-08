//! vr-space: a head-tracked virtual monitor for 3DoF AR glasses on macOS.
//!
//! `main` parses the command line, sets up the window, the head tracking thread, the GPU and
//! the screen capture, then runs the event loop. The pieces live in their own modules.

use std::sync::Arc;
use std::time::Instant;

use clap::{CommandFactory, FromArgMatches};
use env_logger::Env;
use log::{info, warn};

use winit::event_loop::{EventLoop, EventLoopBuilder};
use winit::window::Window;
#[cfg(not(target_os = "macos"))]
use winit::window::Fullscreen as WinitFullscreen;

#[cfg(target_os = "macos")]
use crate::macos_capture::{
    find_window, list_displays, list_windows, match_saved_window, resolve_display_id, start_stream, start_window_watcher,
    CaptureSource,
};

// The running app: state between events and what each event does
mod app;
#[cfg(target_os = "macos")]
use crate::app::Capture;
use crate::app::{App, Fullscreen};

// What the user can ask for (keys, menu bar, global shortcuts)
mod command;
pub(crate) use crate::command::Command;

// Command-line flags and how they combine with saved settings
mod cli;
use crate::cli::{Cli, ExplicitFlags, StartupPlan};
#[cfg(target_os = "macos")]
use crate::cli::WindowChoice;

// Head tracking thread and the pose it publishes
mod tracking;
use crate::tracking::{PoseAtomic, START_INSTANT};

// Axis mapping, recentering, neck model, projection
mod camera;

// Displays and fullscreen
mod window_placement;
use crate::window_placement::{find_monitor, monitor_id};

// Device, surface, scene and HUD renderer; showing captured frames
mod gpu;
use crate::gpu::init_gpu;

// macOS screen capture (virtual monitor)
#[cfg(target_os = "macos")]
mod macos_capture;

// Latency measurements for the HUD and the log
mod latency;

// Head-pose prediction (--predict-ms)
mod prediction;

// HUD layout and text
mod hud;

// Captured frames shown in place on the GPU (IOSurface -> Metal texture)
#[cfg(target_os = "macos")]
mod iosurface;

// Menu bar icon and shortcuts that work from any app
#[cfg(target_os = "macos")]
mod menu_bar;

// Running as vr-space.app: log file, opening at login, permission wording
#[cfg(target_os = "macos")]
mod macos_app;

// Settings saved between runs
mod settings;

// App and menu bar icons, drawn in code
mod icon;

// The grid and the virtual screen, drawn without needing a window
mod scene;
use crate::scene::build_grid_vertices;
use crate::settings::SettingsStore;


// ================= Main =================

fn main() -> anyhow::Result<()> {
    // Initialize start instant for latency metrics
    START_INSTANT.get_or_init(Instant::now);

    // App logs at info; wgpu/naga logs at warn, since their info level logs every buffer it creates.
    // RUST_LOG overrides this (for example RUST_LOG=debug).
    let mut logger = env_logger::Builder::from_env(
        Env::default().default_filter_or("info,wgpu_core=warn,wgpu_hal=warn,wgpu=warn,naga=warn"),
    );
    // Opened from Finder or at login there's no terminal to show the log, so write it to a file.
    #[cfg(target_os = "macos")]
    if macos_app::in_bundle() && !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        if let Some(file) = macos_app::log_file_path().and_then(|p| std::fs::File::create(p).ok()) {
            logger.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    logger.init();

    // Older macOS versions pass a -psn_ argument to apps opened from Finder.
    #[cfg(target_os = "macos")]
    let args = macos_app::without_psn_args(std::env::args_os());
    #[cfg(not(target_os = "macos"))]
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // Parsed in two steps to learn which flags were actually given (they beat saved settings).
    let matches = Cli::command().get_matches_from(args);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    cli.validate()?;
    let explicit = ExplicitFlags::from_matches(&matches);

    if let Some(dir) = &cli.write_iconset {
        icon::write_iconset(dir)?;
        println!("Wrote the app icon to {}", dir.display());
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    if cli.list_displays {
        use winit::platform::macos::MonitorHandleExtMacOS;
        let displays = list_displays()?;
        if displays.is_empty() {
            println!("No displays found (is Screen Recording permission granted?)");
        }
        // Display names (for example the glasses' model) come from the window system.
        let event_loop = EventLoop::new();
        let names: Vec<(u32, String)> = event_loop
            .available_monitors()
            .map(|m| (m.native_id(), m.name().unwrap_or_default()))
            .collect();
        for d in displays {
            let name = names.iter().find(|(id, _)| *id == d.id).map(|(_, n)| n.as_str()).unwrap_or("");
            println!(
                "{:>10}  {:>4}x{:<4} px  {}{}",
                d.id, d.pixel_width, d.pixel_height, name,
                if d.is_main { "  (main)" } else { "" }
            );
        }
        println!("Show a display with --display <ID>; open fullscreen on the glasses with --fullscreen <ID>.");
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    if cli.list_windows {
        let windows = list_windows()?;
        if windows.is_empty() {
            println!("No windows found (is Screen Recording permission granted?)");
        }
        for w in windows {
            println!("{:>10}  {:>4}x{:<4} pt  {}", w.id, w.width, w.height, w.label());
        }
        println!("Show a window with --window <ID>, or --window <app name>.");
        return Ok(());
    }

    info!(
        "vsync={}, invert=({},{},{}), poll_ms={}, no_hud={}, hud_size={}, max_fps={}",
        cli.vsync, cli.invert_x, cli.invert_y, cli.invert_z, cli.poll_ms, cli.no_hud, cli.hud_size, cli.max_fps
    );

    // Saved settings fill in what the command line doesn't say. `settings` is updated when
    // you change something in the app and written back then.
    let (settings_store, settings) = if cli.ignore_settings {
        SettingsStore::disabled()
    } else {
        SettingsStore::open(settings::default_path())
    };
    let plan = StartupPlan::new(&cli, explicit, &settings);

    // Window and event loop (winit 0.28 API)
    let mut builder = EventLoopBuilder::<Command>::with_user_event();
    // Live in the menu bar only: no Dock icon (and no Dock menu to quit from by accident).
    // The window still takes keys when clicked, and fullscreen doesn't need a Dock icon
    // (see apply_fullscreen).
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder.build();
    let window = Window::new(&event_loop).expect("create window");
    window.set_title("Headset Visualizer");

    // Go fullscreen on the chosen display (normally the glasses). If it isn't connected yet,
    // `pending_fullscreen` goes fullscreen on it once it shows up.
    let mut fullscreen_on: Option<u32> = None;
    let mut pending_fullscreen: Option<u32> = None;
    if let Some(id) = plan.fullscreen {
        match find_monitor(&window, id) {
            Some(monitor) => {
                #[cfg(target_os = "macos")]
                {
                    use winit::platform::macos::MonitorHandleExtMacOS;
                    let main_id = window.primary_monitor().map(|m| m.native_id());
                    let resolve = |d: u32| if d == 0 { main_id } else { Some(d) };
                    if plan.window.is_none() && resolve(id).is_some() && resolve(id) == resolve(plan.display) {
                        warn!(
                            "--fullscreen and --display are the same display, so the virtual screen will show \
                             the app itself. Use --display for the screen you want to see and --fullscreen for the glasses."
                        );
                    }
                }
                info!("Fullscreen on display {id} ({})", monitor.name().unwrap_or_default());
                // On macOS this is applied in StartCause::Init, once the app has finished launching.
                fullscreen_on = Some(monitor_id(&monitor));
                #[cfg(not(target_os = "macos"))]
                window.set_fullscreen(Some(WinitFullscreen::Borderless(Some(monitor))));
            }
            None => {
                warn!("Display {id} isn't connected; going fullscreen on it once it is. Run with --list-displays to see the IDs.");
                pending_fullscreen = Some(id);
            }
        }
    }
    // Scene vertices
    let grid_vertices = build_grid_vertices(20, 1);

    // Shared pose (atomic, lock-free)
    let pose_shared = Arc::new(PoseAtomic::new());

    // Head tracking thread (max cadence by default; optional rate limit via --poll-ms)
    tracking::spawn_tracking_thread(pose_shared.clone(), cli.poll_ms);

    // GPU init
    let mut gpu = pollster::block_on(init_gpu(&window, &cli, &grid_vertices))?;
    gpu.scene.screen = plan.screen;
    gpu.hud_enabled &= plan.hud;
    gpu.scene.write_quad(&gpu.queue);

    // What's on the virtual screen: a display (as a real ID; 0 on the command line means the
    // main display) or a window. `last_display` is where to go back to if the window closes.
    #[cfg(target_os = "macos")]
    let last_display = resolve_display_id(plan.display);
    #[cfg(target_os = "macos")]
    let shown = match &plan.window {
        Some(WindowChoice::Query(query)) => match find_window(query) {
            Ok(id) => CaptureSource::Window(id),
            Err(e) => {
                warn!("{e} Showing display {last_display} instead.");
                CaptureSource::Display(last_display)
            }
        },
        Some(WindowChoice::Saved(saved)) => {
            match list_windows().ok().and_then(|list| match_saved_window(&list, &saved.app, &saved.title)) {
                Some(id) => CaptureSource::Window(id),
                None => {
                    info!("{} isn't open; showing display {last_display}.", saved.app);
                    CaptureSource::Display(last_display)
                }
            }
        }
        None => CaptureSource::Display(last_display),
    };

    // Start virtual monitor stream (macOS) via screencapturekit-rs
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = start_stream(shown, !cli.no_cursor) {
            warn!("Screen capture failed to start: {e}");
        }
        // Keeps the menu's window list current and notices when the shown window is resized
        // or closed.
        start_window_watcher();
    }

    let proxy = event_loop.create_proxy();
    let mut app = App::new(
        cli,
        window,
        gpu,
        pose_shared,
        proxy,
        Fullscreen { on: fullscreen_on, pending: pending_fullscreen },
        #[cfg(target_os = "macos")]
        Capture { shown, last_display },
        plan.predict_ms,
        settings_store,
        settings,
    );
    event_loop.run(move |event, _, control_flow| app.handle_event(event, control_flow));
}
