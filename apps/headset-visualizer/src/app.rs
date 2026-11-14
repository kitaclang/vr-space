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
#[cfg(target_os = "macos")]
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use glam::Quat;
use log::{info, warn};
use wgpu_glyph::{HorizontalAlign, Layout, Section, Text, VerticalAlign};
use winit::event::{ElementState, Event, KeyboardInput, MouseButton, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopProxy};
use winit::window::Window;

use crate::camera::{camera_view, head_rotation_from_pose, make_proj, recenter_calibration, upright_roll_correction};
use crate::cli::Cli;
use crate::command::{command_for_key, Command};
use crate::gpu::{configure_surface, Gpu};
use crate::hud::{self, screen_status_line, HudLayout, LabelMetrics};
use crate::latency::{self, FpsMeter, LatencyMeter};
use crate::prediction::{self, Lead, PosePredictor};
use crate::scene::{ScreenPlacement, SCREEN_STEP};
use crate::settings::{Settings, SettingsStore};
use crate::tracking::{now_ns_from_start, PoseAtomic, PoseCopy};
use crate::window_placement::{apply_fullscreen, find_monitor, monitor_id};
#[cfg(target_os = "macos")]
use serde_json::{json, Value};
#[cfg(target_os = "macos")]
use crate::control_socket::Server;

#[derive(Debug)]
pub(crate) enum UserEvent { Command(Command), #[cfg(target_os = "macos")] Control(Value, SyncSender<Value>) }
#[cfg(target_os = "macos")]
use crate::window_placement::warn_if_covering_capture;

#[cfg(target_os = "macos")]
use crate::gpu::show_captured_frame;
#[cfg(target_os = "macos")]
use crate::macos_app;
#[cfg(target_os = "macos")]
use crate::macos_capture::{
    capture_status, capture_status_at, open_screen_recording_settings, stop_stream, stop_stream_at, switch_source, take_latest_frame, take_window_event,
    resolve_display_id, window_info, CaptureSource, CaptureStatus, CapturedWindowEvent,
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
    /// Optional right-hand screen; the primary moves to the left while this is shown.
    pub second: Option<CaptureSource>,
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
    proxy: EventLoopProxy<UserEvent>,

    // Calibration lives on the render thread only, so no shared state or lock is needed.
    // `calib_inv` is the inverse of the head orientation at the last recenter; applying it as a
    // rotation (not per-axis Euler subtraction) stays correct with pitch/roll and across ±180°.
    calibrate_pending: Option<u64>,
    calib_inv: Quat,
    upright_roll: Option<Quat>,
    reference_dirty: bool,
    roll_trim_tenths: i32,
    connection_id: u64,

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
    #[cfg(target_os = "macos")]
    control_server: Option<Server>,
}

#[cfg(target_os = "macos")]
fn capture_needs_restart(current: Option<CaptureSource>, requested: CaptureSource, status: CaptureStatus) -> bool {
    current != Some(requested) || !matches!(status, CaptureStatus::WaitingForFrames | CaptureStatus::Capturing)
}

fn take_recenter_request(pending: &mut Option<u64>, connected: bool, connection_id: u64) -> bool {
    pending.take().is_some_and(|id| connected && id == connection_id)
}

#[cfg(test)]
mod tests {
    use super::take_recenter_request;

    #[cfg(target_os = "macos")]
    #[test]
    fn source_setters_retry_failed_capture_but_leave_running_capture_alone() {
        use super::{capture_needs_restart, CaptureSource, CaptureStatus};
        let source = CaptureSource::Display(7);
        for status in [CaptureStatus::Starting, CaptureStatus::NoPermission, CaptureStatus::Failed] {
            assert!(capture_needs_restart(Some(source), source, status));
        }
        for status in [CaptureStatus::WaitingForFrames, CaptureStatus::Capturing] {
            assert!(!capture_needs_restart(Some(source), source, status));
            assert!(capture_needs_restart(None, source, status));
            assert!(capture_needs_restart(Some(source), CaptureSource::Window(8), status));
        }
    }

    #[test]
    fn recenter_request_cannot_cross_a_disconnect_or_reconnect() {
        let mut pending = Some(1);
        assert!(!take_recenter_request(&mut pending, false, 1));
        assert!(!take_recenter_request(&mut pending, true, 2));
        pending = Some(1);
        assert!(!take_recenter_request(&mut pending, true, 2));
        pending = Some(2);
        assert!(take_recenter_request(&mut pending, true, 2));
        assert!(!take_recenter_request(&mut pending, true, 2));
    }
}

impl App {
    #[cfg(target_os = "macos")]
    pub fn set_control_server(&mut self, server: Server) { self.control_server = Some(server); }

    #[cfg(target_os = "macos")]
    fn handle_control(&mut self, request: Value) -> Value {
        match crate::control::decode_request(request) {
            Ok(crate::control::Action::Status) => crate::control::success(self.control_status()),
            Ok(action) => match self.execute_control(action) {
                Ok(result) => crate::control::success(result),
                Err(error) => {
                    self.refresh_menus();
                    crate::control::failure(error)
                },
            },
            Err(error) => crate::control::failure(error),
        }
    }

    #[cfg(target_os = "macos")]
    fn execute_control(&mut self, action: crate::control::Action) -> anyhow::Result<Value> {
        use crate::control::Action as A;
        let command = match action {
            A::Recenter => {
                if !self.pose.read().3 { anyhow::bail!("head tracking is not connected; recenter was not queued"); }
                Command::Recenter
            }
            A::Horizon { on } if self.cli.recenter_yaw_only != on => Command::SetHorizon(on),
            A::Horizon { .. } => return Ok(json!({"message":"already set"})),
            A::Further => Command::Further, A::Closer => Command::Closer,
            A::Bigger => Command::Bigger, A::Smaller => Command::Smaller, A::ResetScreen => Command::ResetScreen,
            A::RollLeft => Command::RollLeft, A::RollRight => Command::RollRight,
            A::SetCurve { value } => Command::SetCurve(value),
            A::ShowDisplay { id } => Command::ShowDisplay(id),
            A::ShowWindow { id } if id != 0 => Command::ShowWindow(id),
            A::ShowWindow { .. } => anyhow::bail!("window ID 0 is invalid"),
            A::SecondDisplay { id } => Command::SecondDisplay(id),
            A::SecondWindow { id } if id != 0 => Command::SecondWindow(id),
            A::SecondWindow { .. } => anyhow::bail!("window ID 0 is invalid"),
            A::HideSecondScreen => Command::HideSecondScreen,
            A::Fullscreen { id: Some(id) } if find_monitor(&self.window, id).is_none() => {
                anyhow::bail!("display {id} is not connected; run --list-displays and retry after it appears")
            }
            A::Fullscreen { id } => Command::FullscreenOn(id),
            A::Hud { on } if !self.gpu.hud_enabled && on && self.gpu.glyph_brush.is_none() => anyhow::bail!("HUD is unavailable (disabled at startup)"),
            A::Hud { on } if self.gpu.hud_enabled != on => Command::SetHud(on),
            A::Hud { .. } => return Ok(json!({"message":"already set"})),
            A::Prediction { milliseconds: Some(ms), .. } => Command::SetPrediction(ms),
            A::Prediction { milliseconds: None, on: Some(on) } if self.predict_lead.is_on() != on => Command::SetPredictionEnabled(on),
            A::Prediction { milliseconds: None, on: Some(_) } => return Ok(json!({"message":"already set"})),
            A::Prediction { milliseconds: None, on: None } => Command::TogglePrediction,
            A::PredictionMore => Command::PredictMore,
            A::PredictionLess => Command::PredictLess,
            A::CopyFrames { on } if !on && !self.gpu.zero_copy_available => anyhow::bail!("zero-copy frame reading is unavailable on this GPU"),
            A::CopyFrames { on } => Command::SetFrameCopying(on),
            A::OpenScreenSettings => Command::OpenScreenSettings,
            A::OpenAtLogin { on } => {
                let status = crate::macos_app::login_status();
                if status == crate::macos_app::LoginStatus::Unavailable { anyhow::bail!("Open at Login is unavailable outside a supported vr-space.app bundle"); }
                if on && status == crate::macos_app::LoginStatus::RequiresApproval { return Ok(json!({"message":"approval required in System Settings > General > Login Items", "status":status})); }
                let status = crate::macos_app::set_login_enabled(on)?;
                if on && status == crate::macos_app::LoginStatus::NotFound { anyhow::bail!("Open at Login service was not found; reinstall or relaunch vr-space.app"); }
                if let Some(bar) = self.menu_bar.as_mut() { bar.set_open_at_login(crate::macos_app::open_at_login()); }
                return Ok(json!({"status":status, "message":if status == crate::macos_app::LoginStatus::RequiresApproval {"approval required in System Settings > General > Login Items"} else {"updated"}}));
            }
            A::Quit => Command::Quit,
            A::Status => unreachable!(),
        };
        self.run_command(command)?;
        self.remember(command);
        Ok(json!({"message":if command == Command::Recenter {"recenter queued for next render"} else if command == Command::Quit {"quit requested"} else {"updated"}}))
    }

    #[cfg(target_os = "macos")]
    fn control_status(&self) -> Value {
        use crate::macos_capture::CaptureSource as S;
        let source = |s: S| match s { S::Display(id) => json!({"kind":"display","id":id}), S::Window(id) => json!({"kind":"window","id":id}) };
        let cap = |s| match s { CaptureStatus::Starting => "starting", CaptureStatus::NoPermission => "no_permission", CaptureStatus::Failed => "failed", CaptureStatus::WaitingForFrames => "waiting_for_frames", CaptureStatus::Capturing => "capturing" };
        let active = |s| matches!(s, CaptureStatus::WaitingForFrames | CaptureStatus::Capturing);
        let primary_capture = capture_status();
        json!({
            "source":source(self.capture.shown), "second_source":self.capture.second.map(source),
            "fullscreen":{"display":self.fullscreen_on,"pending_display":self.pending_fullscreen},
            "screen":{"distance":self.gpu.scene.screen.distance,"height":self.gpu.scene.screen.height,"curvature":self.gpu.scene.curve},
            "hud":{"available":self.gpu.glyph_brush.is_some(),"enabled":self.gpu.hud_enabled},
            "horizon":{"enabled":self.cli.recenter_yaw_only,"awaiting_recenter":self.reference_dirty || (self.cli.recenter_yaw_only && self.upright_roll.is_none())},
            "roll_trim_degrees":self.roll_trim_tenths as f32 / 10.0,
            "prediction":{"enabled":self.predict_lead.is_on(),"lead_ms":self.predict_lead.ms},
            "frame_copying":{"available":self.gpu.zero_copy_available,"enabled":!self.gpu.zero_copy},
            "tracking":{"connected":self.pose.read().3},
            "capture":{"primary":cap(primary_capture),"primary_source_active":active(primary_capture),"second":self.capture.second.map(|_| cap(capture_status_at(1)))},
            "open_at_login":crate::macos_app::login_status()
        })
    }

    fn refresh_menus(&mut self) {
        self.gpu.hud_text.clear();
        #[cfg(target_os = "macos")]
        if let Some(bar) = self.menu_bar.as_mut() {
            bar.set_state(self.capture.shown, self.fullscreen_on, self.gpu.hud_enabled);
            bar.set_second(self.capture.second);
            bar.set_horizon(self.cli.recenter_yaw_only);
            bar.set_curve(self.gpu.scene.curve);
            bar.set_latency(self.predict_lead.ms, !self.gpu.zero_copy, self.gpu.zero_copy_available);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cli: Cli,
        window: Window,
        gpu: Gpu,
        pose: Arc<PoseAtomic>,
        proxy: EventLoopProxy<UserEvent>,
        fullscreen: Fullscreen,
        #[cfg(target_os = "macos")] capture: Capture,
        predict_ms: f32,
        settings_store: SettingsStore,
        settings: Settings,
    ) -> Self {
        let scale_factor = window.scale_factor() as f32;
        let roll_trim_tenths = settings.roll_trim_tenths.unwrap_or(0);
        Self {
            cli,
            window,
            gpu,
            pose,
            proxy,
            calibrate_pending: None,
            calib_inv: Quat::IDENTITY,
            upright_roll: None,
            reference_dirty: false,
            roll_trim_tenths,
            connection_id: 0,
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
            #[cfg(target_os = "macos")]
            control_server: None,
        }
    }

    pub fn handle_event(&mut self, event: Event<'_, UserEvent>, control_flow: &mut ControlFlow) {
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
            Event::UserEvent(UserEvent::Command(cmd)) => Some(cmd),
            #[cfg(target_os = "macos")]
            Event::UserEvent(UserEvent::Control(request, reply)) => {
                let quit = matches!(crate::control::decode_request(request.clone()), Ok(crate::control::Action::Quit));
                let response = self.handle_control(request);
                let _ = reply.try_send(response);
                if quit {
                    stop_stream();
                    *control_flow = ControlFlow::Exit;
                }
                None
            }
            Event::LoopDestroyed => {
                // Every way of quitting ends here: the menu bar, Esc, closing the window, and a
                // Quit from macOS itself (Cmd-Q, logout), which skips Command::Quit.
                #[cfg(target_os = "macos")]
                {
                    stop_stream();
                    drop(self.control_server.take());
                }
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
                #[cfg(target_os = "macos")]
                stop_stream();
                *control_flow = ControlFlow::Exit;
            }
            match self.run_command(cmd) {
                Ok(()) => self.remember(cmd),
                Err(e) => { warn!("Command {cmd:?} failed: {e}"); self.refresh_menus(); }
            }
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
                    configure_surface(&gpu.surface, &gpu.device, &gpu.config);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor as f32;
                let size = self.window.inner_size();
                if size.width > 0 && size.height > 0 {
                    gpu.config.width = size.width;
                    gpu.config.height = size.height;
                    configure_surface(&gpu.surface, &gpu.device, &gpu.config);
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
            match take_window_event(0) {
                Some(CapturedWindowEvent::Resized) => {
                    if let Err(e) = switch_source(0, capture.shown, !no_cursor) {
                        warn!("Couldn't restart the window capture: {e}");
                    }
                }
                Some(CapturedWindowEvent::Closed) => {
                    let display = capture.last_display;
                    info!("The shown window was closed; showing display {display}.");
                    capture.shown = CaptureSource::Display(display);
                    if let Err(e) = switch_source(0, capture.shown, !no_cursor) {
                        warn!("Couldn't switch to display {display}: {e}");
                    }
                }
                None => {}
            }
            if let Some(source) = capture.second {
                let hide = match take_window_event(1) {
                    Some(CapturedWindowEvent::Resized) => {
                        if let Err(e) = switch_source(1, source, !no_cursor) {
                            warn!("Couldn't restart the second window capture: {e}");
                            true
                        } else {
                            false
                        }
                    }
                    Some(CapturedWindowEvent::Closed) => {
                        info!("The second window was closed; returning to one screen.");
                        true
                    }
                    None => false,
                };
                if hide {
                    stop_stream_at(1);
                    capture.second = None;
                    self.gpu.set_secondary(false);
                    self.gpu.hud_text.clear();
                }
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
                bar.set_second(self.capture.second);
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
                bar.set_horizon(self.cli.recenter_yaw_only);
                bar.set_curve(self.gpu.scene.curve);
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
                configure_surface(&gpu.surface, &gpu.device, &gpu.config);
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
                configure_surface(&gpu.surface, &gpu.device, &gpu.config);
                return true;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Read pose
        let (r, p, y, has, t_ns) = self.pose.read();
        let connection_id = self.pose.connection_id();
        if has && connection_id != self.connection_id {
            // A new fusion has a new reference frame; the old calibration no longer applies.
            self.calib_inv = Quat::IDENTITY;
            self.upright_roll = None;
            self.predictor.reset();
            self.connection_id = connection_id;
        }
        let mut head = head_rotation_from_pose(PoseCopy { roll: r, pitch: p, yaw: y }, (cli.invert_x, cli.invert_y, cli.invert_z));
        // A recenter belongs only to the connection on which it was requested.
        let recenter = take_recenter_request(&mut self.calibrate_pending, has, connection_id);
        // Apply a mode change only on the next connected recenter, avoiding a sudden jump.
        if recenter && self.reference_dirty {
            self.upright_roll = None;
            self.reference_dirty = false;
            self.predictor.reset();
        }
        // In yaw-only mode the first connected recenter establishes upright for this session.
        if cli.recenter_yaw_only && recenter && self.upright_roll.is_none() {
            let correction = upright_roll_correction(head);
            let (yaw, pitch, roll) = head.to_euler(glam::EulerRot::YXZ);
            info!("Yaw-only upright reference: raw roll/pitch/yaw=[{:.2}, {:.2}, {:.2}] deg; mapped yaw/pitch/roll=[{:.2}, {:.2}, {:.2}] deg; roll correction={:.2} deg",
                r.to_degrees(), p.to_degrees(), y.to_degrees(), yaw.to_degrees(), pitch.to_degrees(), roll.to_degrees(), -roll.to_degrees());
            self.upright_roll = Some(correction);
            self.predictor.reset();
        }
        if let Some(correction) = self.upright_roll {
            head *= correction;
        }

        // Recenter: make the current head orientation (or its heading) the new "forward".
        if recenter {
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

        let trim = Quat::from_rotation_z((self.roll_trim_tenths as f32 / 10.0).to_radians());
        let vp = make_proj(gpu.config.width, gpu.config.height, cli.fov) * camera_view(self.calib_inv * shown_head * trim, cli.neck_model);
        gpu.scene.set_view_proj(&gpu.queue, vp);

        // Show the newest screen frame (macOS only). Skips entirely when no new frame has
        // arrived.
        #[cfg(target_os = "macos")]
        let new_frames_displayed_at = {
            let mut times = [None; 2];
            let count = if self.capture.second.is_some() { 2 } else { 1 };
            for (index, time) in times.iter_mut().enumerate().take(count) {
                if let Some(frame) = take_latest_frame(index, &mut gpu.frame_seq[index]) {
                    *time = Some(frame.displayed_at);
                    let started = Instant::now();
                    show_captured_frame(gpu, index, frame);
                    self.latency.upload.push_ns(started.elapsed().as_nanos() as u64);
                }
            }
            times
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
            let screen_line = screen_status_line(gpu.scene.tex_size(), self.capture.shown, 0);
            #[cfg(not(target_os = "macos"))]
            let screen_line = screen_status_line(gpu.scene.tex_size());
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
                format!("roll trim: {:.1}°", self.roll_trim_tenths as f32 / 10.0),
            ];
            #[cfg(target_os = "macos")]
            if let Some(source) = self.capture.second {
                gpu.hud_text.push(format!("second {}", screen_status_line(gpu.scene.tex_size_at(1), source, 1)));
            }
            if self.reference_dirty || (cli.recenter_yaw_only && self.upright_roll.is_none()) {
                gpu.hud_text.push("horizon: recenter upright to set level".into());
            }
            if let Some(status) = self.pose.gyro_status(now_ns) {
                gpu.hud_text.push(status);
            }
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
        let needs_settings = matches!(capture, CaptureStatus::NoPermission | CaptureStatus::Failed)
            || (self.capture.second.is_some() && matches!(capture_status_at(1), CaptureStatus::NoPermission | CaptureStatus::Failed));
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
        for displayed_at in new_frames_displayed_at.into_iter().flatten().filter(|&t| t != 0) {
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
    fn run_command(&mut self, cmd: Command) -> anyhow::Result<()> {
        let gpu = &mut self.gpu;
        match cmd {
            Command::Quit => {}
            Command::Recenter => {
                self.calibrate_pending = self.pose.read().3.then(|| self.pose.connection_id());
            }
            Command::ToggleHorizon => {
                self.cli.recenter_yaw_only = !self.cli.recenter_yaw_only;
                self.reference_dirty = true;
            }
            Command::SetHorizon(on) => { self.cli.recenter_yaw_only = on; self.reference_dirty = true; }
            Command::RollLeft => self.roll_trim_tenths = (self.roll_trim_tenths - 1).max(-450),
            Command::RollRight => self.roll_trim_tenths = (self.roll_trim_tenths + 1).min(450),
            Command::Further => gpu.scene.screen.scale_distance(SCREEN_STEP),
            Command::Closer => gpu.scene.screen.scale_distance(1.0 / SCREEN_STEP),
            Command::Bigger => gpu.scene.screen.scale_height(SCREEN_STEP),
            Command::Smaller => gpu.scene.screen.scale_height(1.0 / SCREEN_STEP),
            Command::ResetScreen => gpu.scene.screen = ScreenPlacement::from_cli(&self.cli),
            Command::SetCurve(curve) => {
                gpu.scene.curve = curve;
                gpu.scene.write_quad(&gpu.queue);
            }
            Command::ToggleFullscreen => {
                // Fullscreen on whichever display the window is on, or back to a window.
                let target =
                    if self.fullscreen_on.is_some() { None } else { self.window.current_monitor().map(|m| monitor_id(&m)) };
                self.fullscreen_on = apply_fullscreen(&self.window, self.fullscreen_on, target);
            }
            Command::FullscreenOn(id) => self.fullscreen_on = apply_fullscreen(&self.window, self.fullscreen_on, id),
            Command::ShowDisplay(id) => {
                #[cfg(target_os = "macos")]
                {
                    let id = resolve_display_id(id);
                    if capture_needs_restart(Some(self.capture.shown), CaptureSource::Display(id), capture_status()) {
                        // Restarting the capture blocks briefly; fine for a menu click.
                        match switch_source(0, CaptureSource::Display(id), !self.cli.no_cursor) {
                            Ok(()) => {
                                self.capture.shown = CaptureSource::Display(id);
                                self.capture.last_display = id;
                            }
                            Err(e) => anyhow::bail!("couldn't switch to display {id}: {e}"),
                        }
                    }
                }
                #[cfg(not(target_os = "macos"))]
                let _ = id;
            }
            Command::ShowWindow(id) => {
                #[cfg(target_os = "macos")]
                if capture_needs_restart(Some(self.capture.shown), CaptureSource::Window(id), capture_status()) {
                    match switch_source(0, CaptureSource::Window(id), !self.cli.no_cursor) {
                        Ok(()) => self.capture.shown = CaptureSource::Window(id),
                        Err(e) => anyhow::bail!("couldn't switch to window {id}: {e}"),
                    }
                }
                #[cfg(not(target_os = "macos"))]
                let _ = id;
            }
            Command::SecondDisplay(id) | Command::SecondWindow(id) => {
                #[cfg(target_os = "macos")]
                {
                    let source = if matches!(cmd, Command::SecondDisplay(_)) {
                        CaptureSource::Display(resolve_display_id(id))
                    } else {
                        CaptureSource::Window(id)
                    };
                    if capture_needs_restart(self.capture.second, source, capture_status_at(1)) {
                        match switch_source(1, source, !self.cli.no_cursor) {
                            Ok(()) => {
                                // Discard old images only after accepting the new source.
                                gpu.set_secondary(false);
                                gpu.set_secondary(true);
                                self.capture.second = Some(source);
                            }
                            Err(e) => {
                                warn!("Couldn't start the second screen: {e}");
                                if !matches!(capture_status_at(1), CaptureStatus::WaitingForFrames | CaptureStatus::Capturing) {
                                    gpu.set_secondary(false);
                                    stop_stream_at(1);
                                    self.capture.second = None;
                                }
                                anyhow::bail!("couldn't start the second screen: {e}");
                            }
                        }
                    }
                }
                #[cfg(not(target_os = "macos"))]
                let _ = id;
            }
            Command::HideSecondScreen => {
                #[cfg(target_os = "macos")]
                {
                    stop_stream_at(1);
                    self.capture.second = None;
                    gpu.set_secondary(false);
                }
            }
            Command::ToggleHud => {
                if gpu.glyph_brush.is_some() { gpu.hud_enabled = !gpu.hud_enabled; }
            }
            Command::SetHud(on) => {
                if gpu.glyph_brush.is_none() { anyhow::bail!("HUD is unavailable (disabled at startup)"); }
                gpu.hud_enabled = on;
            }
            Command::OpenScreenSettings => {
                #[cfg(target_os = "macos")]
                open_screen_recording_settings()?;
            }
            Command::ToggleOpenAtLogin => {
                #[cfg(target_os = "macos")]
                {
                    let on = macos_app::open_at_login().unwrap_or(false);
                    let now = macos_app::set_open_at_login(!on);
                    if let Some(bar) = self.menu_bar.as_mut() { bar.set_open_at_login(now); }
                }
            }
            Command::TogglePrediction => self.predict_lead.toggle(),
            Command::SetPrediction(ms) => self.predict_lead.set_ms(ms),
            Command::SetPredictionEnabled(on) => self.predict_lead.set_enabled(on),
            Command::PredictMore => self.predict_lead.more(),
            Command::PredictLess => self.predict_lead.less(),
            Command::ToggleFrameCopying => {
                if gpu.zero_copy_available { gpu.zero_copy = !gpu.zero_copy; }
                else { info!("Captured frames can only be copied here: the GPU can't read them in place"); }
            }
            Command::SetFrameCopying(on) => {
                if !on && !gpu.zero_copy_available { anyhow::bail!("zero-copy frame reading is unavailable on this GPU"); }
                gpu.zero_copy = !on;
            }
        }
        if matches!(cmd, Command::Further | Command::Closer | Command::Bigger | Command::Smaller | Command::ResetScreen) {
            self.gpu.scene.write_quad(&self.gpu.queue);
        }
        #[cfg(target_os = "macos")]
        if matches!(cmd, Command::ToggleFullscreen | Command::FullscreenOn(_) | Command::ShowDisplay(_) | Command::SecondDisplay(_)) {
            let main_id = self.window.primary_monitor().map(|m| monitor_id(&m));
            for source in Some(self.capture.shown).into_iter().chain(self.capture.second) {
                let captured_display = match source {
                    CaptureSource::Display(id) => Some(id),
                    CaptureSource::Window(_) => None,
                };
                warn_if_covering_capture(self.fullscreen_on, captured_display, main_id);
            }
        }
        Ok(())
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
            Command::SetCurve(_) => settings.curve = Some(self.gpu.scene.curve),
            Command::ToggleHud | Command::SetHud(_) => settings.hud = Some(self.gpu.hud_enabled),
            Command::ToggleHorizon | Command::SetHorizon(_) => settings.recenter_yaw_only = Some(self.cli.recenter_yaw_only),
            Command::RollLeft | Command::RollRight => settings.roll_trim_tenths = Some(self.roll_trim_tenths),
            Command::TogglePrediction | Command::PredictMore | Command::PredictLess | Command::SetPrediction(_) | Command::SetPredictionEnabled(_) => {
                settings.predict_ms = Some(self.predict_lead.ms);
                info!("Head-pose prediction: {} ms ahead", self.predict_lead.ms);
            }
            #[cfg(target_os = "macos")]
            Command::ShowDisplay(id) if self.capture.shown == CaptureSource::Display(resolve_display_id(id)) => {
                settings.display = Some(resolve_display_id(id));
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
            bar.set_second(self.capture.second);
            bar.set_horizon(self.cli.recenter_yaw_only);
            bar.set_curve(self.gpu.scene.curve);
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
