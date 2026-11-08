//! The running app: everything the event loop needs between events, and what it does with
//! each one.
//!
//! `main` sets up the window, GPU, head tracking and capture, then hands them to `App::new`
//! and forwards every winit event to `App::handle_event`. From there:
//! - `window_event`: keys, clicks and resizes (keys and clicks become `Command`s);
//! - `every_two_seconds`: the captured window was resized or closed, the glasses' display
//!   was plugged in, the menu's lists;
//! - `init`: the menu bar icon and fullscreen, once the app has finished launching;
//! - `render`: one frame (see "How the visualizer works" in AGENTS.md);
//! - `run_command`, then `remember`: carry out a `Command` and save what it changed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::Quat;
use log::{info, warn};
use wgpu_glyph::{HorizontalAlign, Layout, Section, Text, VerticalAlign};
use winit::event::{ElementState, Event, KeyboardInput, MouseButton, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopProxy};
use winit::window::Window;

use crate::camera::{camera_view, head_rotation_from_pose, make_proj, recenter_calibration};
use crate::cli::Cli;
use crate::command::{command_for_key, Command};
use crate::gpu::Gpu;
use crate::hud::{self, screen_status_line, HudLayout, LabelMetrics};
use crate::latency::{self, FpsMeter, LatencyMeter};
use crate::prediction::{self, Lead, PosePredictor};
use crate::scene::{ScreenPlacement, SCREEN_STEP};
use crate::settings::{Settings, SettingsStore};
use crate::tracking::{now_ns_from_start, PoseAtomic, PoseCopy};
use crate::window_placement::{apply_fullscreen, find_monitor, monitor_id};

#[cfg(target_os = "macos")]
use crate::gpu::show_captured_frame;
#[cfg(target_os = "macos")]
use crate::macos_app;
#[cfg(target_os = "macos")]
use crate::macos_capture::{
    capture_status, open_screen_recording_settings, stop_stream, switch_source, take_latest_frame, take_window_event,
    window_info, CaptureSource, CaptureStatus, CapturedWindowEvent,
};
#[cfg(target_os = "macos")]
use crate::menu_bar::MenuBar;
#[cfg(target_os = "macos")]
use crate::settings::SavedWindow;

/// How often the HUD's text is re-formatted (it is still drawn every frame).
const HUD_REFRESH_NS: u64 = 100_000_000;

/// How often the latency figures go to the log.
const LATENCY_LOG_NS: u64 = 10_000_000_000;

/// What's on the virtual screen and where to go back to (macOS only).
#[cfg(target_os = "macos")]
pub struct Capture {
    /// The display or window being captured.
    pub shown: CaptureSource,
    /// The display to go back to if the shown window closes (a real ID, not 0).
    pub last_display: u32,
}

/// Where the window is fullscreen, from the startup plan.
pub struct Fullscreen {
    /// The display the window fills now (`None`: a normal window).
    pub on: Option<u32>,
    /// A display to go fullscreen on once it's connected (the glasses, plugged in later).
    pub pending: Option<u32>,
}

pub struct App {
    cli: Cli,
    window: Window,
    gpu: Gpu,
    pose: Arc<PoseAtomic>,
    proxy: EventLoopProxy<Command>,

    // Calibration lives on the render thread only, so no shared state or lock is needed.
    // `calib_inv` is the inverse of the head orientation at the last recenter; applying it as a
    // rotation (not per-axis Euler subtraction) stays correct with pitch/roll and across ±180°.
    calibrate_pending: bool,
    calib_inv: Quat,

    // Mouse position and the window's scale, both for the HUD's buttons (physical pixels).
    cursor_px: (f32, f32),
    scale_factor: f32,

    fullscreen_on: Option<u32>,
    pending_fullscreen: Option<u32>,
    #[cfg(target_os = "macos")]
    capture: Capture,
    #[cfg(target_os = "macos")]
    menu_bar: Option<MenuBar>,
    last_slow_check: Instant,
    // Hints shown over the virtual screen when capture isn't working: no permission, failed.
    #[cfg(target_os = "macos")]
    capture_hints: (String, String),

    fps: FpsMeter,
    latency: LatencyMeter,
    latency_logged_ns: u64,
    predictor: PosePredictor,
    predict_lead: Lead,

    settings_store: SettingsStore,
    settings: Settings,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cli: Cli,
        window: Window,
        gpu: Gpu,
        pose: Arc<PoseAtomic>,
        proxy: EventLoopProxy<Command>,
        fullscreen: Fullscreen,
        #[cfg(target_os = "macos")] capture: Capture,
        predict_ms: f32,
        settings_store: SettingsStore,
        settings: Settings,
    ) -> Self {
        let scale_factor = window.scale_factor() as f32;
        Self {
            cli,
            window,
            gpu,
            pose,
            proxy,
            calibrate_pending: false,
            calib_inv: Quat::IDENTITY,
            cursor_px: (-1.0, -1.0),
            scale_factor,
            fullscreen_on: fullscreen.on,
            pending_fullscreen: fullscreen.pending,
            #[cfg(target_os = "macos")]
            capture,
            #[cfg(target_os = "macos")]
            menu_bar: None,
            last_slow_check: Instant::now(),
            #[cfg(target_os = "macos")]
            capture_hints: capture_hints(),
            fps: FpsMeter::new(),
            latency: LatencyMeter::default(),
            latency_logged_ns: 0,
            predictor: PosePredictor::new(),
            predict_lead: Lead::new(predict_ms),
            settings_store,
            settings,
        }
    }

    pub fn handle_event(&mut self, event: Event<'_, Command>, control_flow: &mut ControlFlow) {
        // Frame pacing: only apply WaitUntil when no-vsync. With vsync enabled, let OS/VBlank
        // drive redraws via Wait and explicit request_redraw on events.
        *control_flow = if !self.cli.vsync && self.cli.max_fps > 0 {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_micros((1_000_000u64 / self.cli.max_fps as u64).max(1)))
        } else if self.cli.vsync {
            ControlFlow::Wait
        } else {
            ControlFlow::Poll
        };

        let command = match event {
            Event::WindowEvent { event, window_id } if window_id == self.window.id() => self.window_event(event),
            Event::MainEventsCleared => {
                // Request redraws appropriately:
                // - No vsync: drive redraw every loop to maximize FPS (Poll or WaitUntil pacing).
                // - Vsync: request redraw here so the Window handles VBlank pacing without
                //   starving the event loop.
                self.window.request_redraw();
                if self.last_slow_check.elapsed() >= Duration::from_secs(2) {
                    self.last_slow_check = Instant::now();
                    self.every_two_seconds();
                }
                None
            }
            Event::NewEvents(StartCause::Init) => {
                self.init();
                None
            }
            Event::UserEvent(cmd) => Some(cmd),
            Event::LoopDestroyed => {
                // Every way of quitting ends here: the menu bar, Esc, closing the window, and a
                // Quit from macOS itself (Cmd-Q, logout), which skips Command::Quit.
                #[cfg(target_os = "macos")]
                stop_stream();
                info!("Quitting");
                None
            }
            Event::RedrawRequested(id) if id == self.window.id() => {
                if !self.render() {
                    *control_flow = ControlFlow::Exit;
                }
                None
            }
            _ => None,
        };

        // Carry out a command from a key, the menu bar or a global shortcut.
        if let Some(cmd) = command {
            if cmd == Command::Quit {
                // Stop ScreenCaptureKit cleanly, then exit.
                #[cfg(target_os = "macos")]
                stop_stream();
                *control_flow = ControlFlow::Exit;
            }
            self.run_command(cmd);
            self.remember(cmd);
        }
    }

    /// Keys, clicks, resizes. Returns the command a key or click asks for.
    fn window_event(&mut self, event: WindowEvent<'_>) -> Option<Command> {
        let gpu = &mut self.gpu;
        match event {
            WindowEvent::CloseRequested => return Some(Command::Quit),
            // Keys in the window. Held keys repeat, so the screen moves smoothly.
            WindowEvent::KeyboardInput {
                input: KeyboardInput { state: ElementState::Pressed, virtual_keycode: Some(key), .. },
                ..
            } => return command_for_key(key),
            WindowEvent::Resized(new_size) => {
                if new_size.width > 0 && new_size.height > 0 {
                    gpu.config.width = new_size.width;
                    gpu.config.height = new_size.height;
                    gpu.surface.configure(&gpu.device, &gpu.config);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor as f32;
                let size = self.window.inner_size();
                if size.width > 0 && size.height > 0 {
                    gpu.config.width = size.width;
                    gpu.config.height = size.height;
                    gpu.surface.configure(&gpu.device, &gpu.config);
                }
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                // Only a click on a HUD button does anything (both are in physical pixels).
                let (x, y) = self.cursor_px;
                if gpu.settings_btn.is_some_and(|b| b.contains(x, y)) {
                    return Some(Command::OpenScreenSettings);
                }
                if gpu.hud_enabled && gpu.calibrate_btn.contains(x, y) {
                    return Some(Command::Recenter);
                }
            }
            WindowEvent::CursorMoved { position, .. } => self.cursor_px = (position.x as f32, position.y as f32),
            WindowEvent::CursorLeft { .. } => self.cursor_px = (-1.0, -1.0),
            _ => {}
        }
        None
    }

    /// Things that change rarely and are slow to check: the captured window, the glasses'
    /// display being plugged in, the menu bar's lists.
    fn every_two_seconds(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let no_cursor = self.cli.no_cursor;
            let capture = &mut self.capture;
            // The shown window was resized (restart at the new size) or closed.
            match take_window_event() {
                Some(CapturedWindowEvent::Resized) => {
                    if let Err(e) = switch_source(capture.shown, !no_cursor) {
                        warn!("Couldn't restart the window capture: {e}");
                    }
                }
                Some(CapturedWindowEvent::Closed) => {
                    let display = capture.last_display;
                    info!("The shown window was closed; showing display {display}.");
                    capture.shown = CaptureSource::Display(display);
                    if let Err(e) = switch_source(capture.shown, !no_cursor) {
                        warn!("Couldn't switch to display {display}: {e}");
                    }
                }
                None => {}
            }
            // The display to go fullscreen on (the glasses) was plugged in.
            if let Some(id) = self.pending_fullscreen {
                if find_monitor(&self.window, id).is_some() {
                    info!("Display {id} connected; going fullscreen on it.");
                    self.pending_fullscreen = None;
                    self.fullscreen_on = apply_fullscreen(&self.window, self.fullscreen_on, Some(id));
                }
            }
            if let Some(bar) = self.menu_bar.as_mut() {
                bar.refresh();
                bar.set_state(self.capture.shown, self.fullscreen_on, self.gpu.hud_enabled);
            }
        }
    }

    /// Once the app has finished launching: the menu bar icon (macOS needs the event loop
    /// running for it) and fullscreen.
    fn init(&mut self) {
        #[cfg(target_os = "macos")]
        match MenuBar::new(
            self.proxy.clone(),
            self.capture.shown,
            self.fullscreen_on,
            self.gpu.hud_enabled,
            self.gpu.glyph_brush.is_some(),
            macos_app::open_at_login(),
        ) {
            Ok(mut bar) => {
                bar.set_latency(self.predict_lead.ms, !self.gpu.zero_copy, self.gpu.zero_copy_available);
                self.menu_bar = Some(bar);
            }
            Err(e) => warn!("Menu bar icon unavailable: {e}"),
        }
        #[cfg(not(target_os = "macos"))]
        let _ = &self.proxy;
        if self.fullscreen_on.is_some() {
            self.fullscreen_on = apply_fullscreen(&self.window, None, self.fullscreen_on);
        }
    }

    /// Draw one frame. Returns false when the app should exit (the GPU ran out of memory).
    fn render(&mut self) -> bool {
        let cli = &self.cli;
        let gpu = &mut self.gpu;

        // Acquire frame
        let frame = match gpu.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost) => {
                // Surface lost; reconfigure then try next frame
                warn!("Surface lost, reconfiguring.");
                gpu.surface.configure(&gpu.device, &gpu.config);
                return true;
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                // Critical; exit to avoid freeze
                warn!("Surface out of memory; exiting.");
                return false;
            }
            Err(e) => {
                // Other errors: Timeout/Outdated - reconfigure and skip this frame
                warn!("Surface acquire error: {e}. Reconfiguring surface.");
                gpu.surface.configure(&gpu.device, &gpu.config);
                return true;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Read pose
        let (r, p, y, has, t_ns) = self.pose.read();
        let head = head_rotation_from_pose(PoseCopy { roll: r, pitch: p, yaw: y }, (cli.invert_x, cli.invert_y, cli.invert_z));

        // Recenter: make the current head orientation (or its heading) the new "forward".
        if self.calibrate_pending {
            self.calibrate_pending = false;
            self.calib_inv = recenter_calibration(head, cli.recenter_yaw_only);
            gpu.show_calibrated_toast_until_ns = now_ns_from_start() + 1_000_000_000;
        }

        // With --predict-ms, draw the head where it's expected to be when the frame is on the
        // glasses. Recentering above uses the measured pose.
        let predict_lead_ns = self.predict_lead.ns();
        let shown_head = if predict_lead_ns == 0 {
            head
        } else if has {
            self.predictor.observe(head, t_ns);
            self.predictor.predict(head, prediction::horizon_ns(t_ns, now_ns_from_start(), predict_lead_ns))
        } else {
            self.predictor.reset();
            head
        };

        let vp = make_proj(gpu.config.width, gpu.config.height, cli.fov) * camera_view(self.calib_inv * shown_head, cli.neck_model);
        gpu.scene.set_view_proj(&gpu.queue, vp);

        // Show the newest screen frame (macOS only). Skips entirely when no new frame has
        // arrived.
        #[cfg(target_os = "macos")]
        let new_frame_displayed_at = match take_latest_frame(&mut gpu.frame_seq) {
            Some(frame) => {
                let displayed_at = frame.displayed_at;
                let started = Instant::now();
                show_captured_frame(gpu, frame);
                self.latency.upload.push_ns(started.elapsed().as_nanos() as u64);
                Some(displayed_at)
            }
            None => None,
        };

        // Render 3D scene
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("encoder") });
        gpu.scene.encode(&mut encoder, &view);

        self.fps.on_frame();
        let now_ns = now_ns_from_start();

        // Refresh the HUD strings at most every 100 ms: readable, and it avoids formatting (and
        // allocating) strings every frame. Skipped entirely when the HUD is off.
        if gpu.hud_enabled && (gpu.hud_text.is_empty() || now_ns.saturating_sub(gpu.hud_text_updated_ns) >= HUD_REFRESH_NS) {
            let dev = if has { "connected" } else { "not-found" };
            #[cfg(target_os = "macos")]
            let screen_line = screen_status_line(gpu.scene.tex_size, self.capture.shown);
            #[cfg(not(target_os = "macos"))]
            let screen_line = screen_status_line(gpu.scene.tex_size);
            gpu.hud_text = vec![
                format!("device: {dev}"),
                format!("fps: {:.1}", self.fps.fps.max(0.0)),
                latency::describe("pose->submit", self.latency.pose_to_submit.summary()),
                #[cfg(target_os = "macos")]
                latency::describe("capture->submit", self.latency.capture_to_submit.summary()),
                #[cfg(target_os = "macos")]
                latency::describe("frame upload", self.latency.upload.summary()),
                format!("euler(rad) r={:.3} p={:.3} y={:.3}", r, p, y),
                format!(
                    "invert=({},{},{}) vsync={} frames={}",
                    cli.invert_x,
                    cli.invert_y,
                    cli.invert_z,
                    cli.vsync,
                    if gpu.zero_copy { "shared" } else { "copied" }
                ),
                screen_line,
                format!("distance: {:.2}  height: {:.2}", gpu.scene.screen.distance, gpu.scene.screen.height),
            ];
            gpu.hud_text.push(if self.predict_lead.is_on() {
                format!(
                    "predict: {:.0} ms ahead, head turning {:.0}°/s",
                    self.predict_lead.ms,
                    self.predictor.speed().to_degrees()
                )
            } else {
                "predict: off".to_string()
            });
            gpu.hud_text_updated_ns = now_ns;
        }

        // Queue HUD text every frame: wgpu_glyph draws only what was queued for this frame, so
        // skipping the queue makes the HUD disappear on that frame. Text and buttons are laid
        // out in physical pixels, like the mouse position.
        let scale = gpu.hud_font_size * self.scale_factor;
        #[cfg(target_os = "macos")]
        let capture = capture_status();
        #[cfg(target_os = "macos")]
        let needs_settings = matches!(capture, CaptureStatus::NoPermission | CaptureStatus::Failed);
        #[cfg(not(target_os = "macos"))]
        let needs_settings = false;
        if let Some(font) = gpu.hud_font.as_ref() {
            if gpu.hud_metrics.map_or(true, |m| m.font_px != scale) {
                gpu.hud_metrics = Some(LabelMetrics::measure(font, scale));
            }
        }
        let layout: Option<HudLayout> =
            gpu.hud_metrics.map(|m| hud::layout(gpu.config.width, self.scale_factor, &m, gpu.hud_enabled && needs_settings));
        if let Some(layout) = layout {
            gpu.calibrate_btn = layout.calibrate;
        }
        gpu.settings_btn = layout.and_then(|l| l.settings);
        if gpu.hud_enabled {
            if let (Some(gb), Some(layout)) = (gpu.glyph_brush.as_mut(), layout.as_ref()) {
                let toast = now_ns < gpu.show_calibrated_toast_until_ns;
                hud::queue_hud(gb, layout, &gpu.hud_text, toast, gpu.config.width, gpu.config.height);
            }
        }

        // Screen capture problems: besides the settings button (in the layout above), a hint in
        // the middle of the window (over the still-black virtual screen).
        #[cfg(target_os = "macos")]
        {
            let hint: Option<&str> = match capture {
                CaptureStatus::NoPermission => Some(&self.capture_hints.0),
                CaptureStatus::Failed => Some(&self.capture_hints.1),
                _ => None,
            };
            if let (true, Some(hint), Some(gb)) = (gpu.hud_enabled, hint, gpu.glyph_brush.as_mut()) {
                gb.queue(Section {
                    screen_position: (gpu.config.width as f32 * 0.5, gpu.config.height as f32 * 0.5),
                    bounds: (gpu.config.width as f32 * 0.9, gpu.config.height as f32),
                    text: vec![Text::new(hint).with_color([1.0, 0.85, 0.3, 1.0]).with_scale(scale * 1.1)],
                    layout: Layout::default().h_align(HorizontalAlign::Center).v_align(VerticalAlign::Center),
                });
            }
        }

        // Draw glyphs only if HUD is enabled; this is cheap when nothing queued
        if gpu.hud_enabled {
            if let (Some(gb), Some(sb)) = (gpu.glyph_brush.as_mut(), gpu.staging_belt.as_mut()) {
                gb.draw_queued(&gpu.device, sb, &mut encoder, &view, gpu.config.width, gpu.config.height).ok();
            }
        }

        // Finish staging belt and submit
        if let Some(sb) = gpu.staging_belt.as_mut() {
            sb.finish();
        }
        gpu.queue.submit(Some(encoder.finish()));
        // Without glasses there's no pose sample to measure from.
        if has && t_ns > 0 {
            self.latency.pose_to_submit.push_ns(now_ns_from_start().saturating_sub(t_ns));
        }
        #[cfg(target_os = "macos")]
        if let Some(displayed_at) = new_frame_displayed_at.filter(|&t| t != 0) {
            let ns = latency::mach::ns_between(displayed_at, latency::mach::now());
            self.latency.capture_to_submit.push_ns(ns);
        }
        if now_ns.saturating_sub(self.latency_logged_ns) >= LATENCY_LOG_NS {
            self.latency_logged_ns = now_ns;
            info!("Latency: {}", self.latency.log_line());
        }
        // Return the HUD's staging buffers for reuse once the GPU is done with them. Without
        // this the belt creates a new buffer every frame and memory keeps growing.
        if let Some(sb) = gpu.staging_belt.as_mut() {
            sb.recall();
        }
        // Present the frame (present() does not return a Result on wgpu 0.17)
        frame.present();
        true
    }

    /// Carry out a command from a key, the menu bar or a global shortcut. `Quit` is handled
    /// by `handle_event`, which owns the control flow.
    fn run_command(&mut self, cmd: Command) {
        let gpu = &mut self.gpu;
        match cmd {
            Command::Quit => {}
            Command::Recenter => self.calibrate_pending = true,
            Command::Further => gpu.scene.screen.scale_distance(SCREEN_STEP),
            Command::Closer => gpu.scene.screen.scale_distance(1.0 / SCREEN_STEP),
            Command::Bigger => gpu.scene.screen.scale_height(SCREEN_STEP),
            Command::Smaller => gpu.scene.screen.scale_height(1.0 / SCREEN_STEP),
            Command::ResetScreen => gpu.scene.screen = ScreenPlacement::from_cli(&self.cli),
            Command::ToggleFullscreen => {
                // Fullscreen on whichever display the window is on, or back to a window.
                let target =
                    if self.fullscreen_on.is_some() { None } else { self.window.current_monitor().map(|m| monitor_id(&m)) };
                self.fullscreen_on = apply_fullscreen(&self.window, self.fullscreen_on, target);
            }
            Command::FullscreenOn(id) => self.fullscreen_on = apply_fullscreen(&self.window, self.fullscreen_on, id),
            Command::ShowDisplay(id) => {
                #[cfg(target_os = "macos")]
                if self.capture.shown != CaptureSource::Display(id) {
                    // Restarting the capture blocks briefly; fine for a menu click.
                    match switch_source(CaptureSource::Display(id), !self.cli.no_cursor) {
                        Ok(()) => {
                            self.capture.shown = CaptureSource::Display(id);
                            self.capture.last_display = id;
                        }
                        Err(e) => warn!("Couldn't switch to display {id}: {e}"),
                    }
                }
                #[cfg(not(target_os = "macos"))]
                let _ = id;
            }
            Command::ShowWindow(id) => {
                #[cfg(target_os = "macos")]
                if self.capture.shown != CaptureSource::Window(id) {
                    match switch_source(CaptureSource::Window(id), !self.cli.no_cursor) {
                        Ok(()) => self.capture.shown = CaptureSource::Window(id),
                        Err(e) => warn!("Couldn't switch to window {id}: {e}"),
                    }
                }
                #[cfg(not(target_os = "macos"))]
                let _ = id;
            }
            Command::ToggleHud => {
                // Only possible when the HUD font was set up (not with --no-hud).
                if gpu.glyph_brush.is_some() {
                    gpu.hud_enabled = !gpu.hud_enabled;
                }
            }
            Command::OpenScreenSettings => {
                #[cfg(target_os = "macos")]
                open_screen_recording_settings();
            }
            Command::ToggleOpenAtLogin => {
                #[cfg(target_os = "macos")]
                if let Some(on) = macos_app::open_at_login() {
                    let now = macos_app::set_open_at_login(!on);
                    if let Some(bar) = self.menu_bar.as_mut() {
                        bar.set_open_at_login(now);
                    }
                }
            }
            Command::TogglePrediction => self.predict_lead.toggle(),
            Command::PredictMore => self.predict_lead.more(),
            Command::PredictLess => self.predict_lead.less(),
            Command::ToggleFrameCopying => {
                if gpu.zero_copy_available {
                    gpu.zero_copy = !gpu.zero_copy;
                    info!("Captured frames: {}", if gpu.zero_copy { "read in place by the GPU" } else { "copied through the CPU" });
                } else {
                    info!("Captured frames can only be copied here: the GPU can't read them in place");
                }
            }
        }
        if matches!(cmd, Command::Further | Command::Closer | Command::Bigger | Command::Smaller | Command::ResetScreen) {
            self.gpu.scene.write_quad(&self.gpu.queue);
        }
    }

    /// Save what a command changed, for next time, and update the HUD and menu. Only what you
    /// change is saved, so a window that isn't open at startup is still remembered.
    fn remember(&mut self, cmd: Command) {
        let settings = &mut self.settings;
        match cmd {
            Command::Further | Command::Closer | Command::Bigger | Command::Smaller => {
                settings.screen_distance = Some(self.gpu.scene.screen.distance);
                settings.screen_height = Some(self.gpu.scene.screen.height);
            }
            // Back to the command-line values, and keep following them.
            Command::ResetScreen => {
                settings.screen_distance = None;
                settings.screen_height = None;
            }
            Command::ToggleFullscreen | Command::FullscreenOn(_) => {
                self.pending_fullscreen = None;
                settings.fullscreen = self.fullscreen_on;
            }
            Command::ToggleHud => settings.hud = Some(self.gpu.hud_enabled),
            Command::TogglePrediction | Command::PredictMore | Command::PredictLess => {
                settings.predict_ms = Some(self.predict_lead.ms);
                info!("Head-pose prediction: {} ms ahead", self.predict_lead.ms);
            }
            #[cfg(target_os = "macos")]
            Command::ShowDisplay(id) if self.capture.shown == CaptureSource::Display(id) => {
                settings.display = Some(id);
                settings.window = None;
            }
            #[cfg(target_os = "macos")]
            Command::ShowWindow(id) if self.capture.shown == CaptureSource::Window(id) => {
                settings.window = window_info(id).map(|w| SavedWindow { app: w.app, title: w.title });
            }
            _ => {}
        }
        self.settings_store.save(settings);
        self.gpu.hud_text.clear(); // refresh the HUD's text now
        #[cfg(target_os = "macos")]
        if let Some(bar) = self.menu_bar.as_mut() {
            bar.set_state(self.capture.shown, self.fullscreen_on, self.gpu.hud_enabled);
            bar.set_latency(self.predict_lead.ms, !self.gpu.zero_copy, self.gpu.zero_copy_available);
        }
    }
}

/// The hints shown over the virtual screen when capture isn't working. Who holds the
/// permission depends on whether we run as vr-space.app or from a terminal.
#[cfg(target_os = "macos")]
fn capture_hints() -> (String, String) {
    (
        format!(
            "Screen Recording permission needed\n\n\
             Press P (or click the button at the top right) to open System Settings,\n\
             allow {} under Screen Recording, then quit and reopen {}.",
            macos_app::permission_holder(),
            macos_app::restart_target(),
        ),
        if macos_app::in_bundle() {
            format!(
                "Screen capture failed to start. See the log for details:\n{}",
                macos_app::log_file_path().map(|p| p.display().to_string()).unwrap_or_default(),
            )
        } else {
            "Screen capture failed to start. See the terminal log for details.".to_string()
        },
    )
}
