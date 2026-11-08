#![cfg(target_os = "macos")]

// ScreenCaptureKit integration via the screencapturekit crate.
// Captures BGRA frames from the selected display or window and hands the newest one to the
// renderer.
//
// Frame handoff uses three rotating buffers so neither side allocates or holds a lock for long:
// - BACK: written by the capture callback (only that thread touches it).
// - READY: the newest complete frame. The capture callback swaps BACK into it; the renderer
//   swaps its own buffer out of it. Both swaps are O(1), so the lock is held only briefly.
// - The renderer's own buffer, passed to `take_latest_bgra`.
// FRAME_SEQ lets the renderer see "nothing new" with a single atomic load and skip the upload.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::Duration;
use log::{info, warn};

use screencapturekit::{
    output::{CMSampleBuffer, LockTrait},
    shareable_content::{SCDisplay, SCShareableContent, SCWindow},
    stream::{
        configuration::{pixel_format::PixelFormat, SCStreamConfiguration},
        content_filter::SCContentFilter,
        output_type::SCStreamOutputType,
        SCStream,
    },
};

use core_graphics::access::ScreenCaptureAccess;
use core_graphics::display::{CGDirectDisplayID, CGDisplay};

// ================= Screen Recording permission and capture status =================

/// What the capture stream is doing, for the HUD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureStatus {
    /// `start_stream` hasn't finished yet.
    Starting,
    /// macOS hasn't granted Screen Recording to this process (to vr-space.app, or to the
    /// terminal app it was started from).
    NoPermission,
    /// The stream failed to start for another reason; the log has details.
    Failed,
    /// The stream is running but no frame has arrived yet.
    WaitingForFrames,
    /// Frames are arriving.
    Capturing,
}

const STATUS_STARTING: u8 = 0;
const STATUS_NO_PERMISSION: u8 = 1;
const STATUS_FAILED: u8 = 2;
const STATUS_RUNNING: u8 = 3;
static STATUS: AtomicU8 = AtomicU8::new(STATUS_STARTING);

/// Current capture status. Cheap (two atomic loads), safe to call every frame.
pub fn capture_status() -> CaptureStatus {
    match STATUS.load(Ordering::Acquire) {
        STATUS_NO_PERMISSION => CaptureStatus::NoPermission,
        STATUS_FAILED => CaptureStatus::Failed,
        STATUS_RUNNING if FRAME_SEQ.load(Ordering::Acquire) == 0 => CaptureStatus::WaitingForFrames,
        STATUS_RUNNING => CaptureStatus::Capturing,
        _ => CaptureStatus::Starting,
    }
}

/// Whether macOS currently allows this process to record the screen. Run as vr-space.app,
/// the permission belongs to the app; run from a terminal, it belongs to the terminal. Either
/// way a newly granted permission only takes effect after that app is restarted.
pub fn has_screen_recording_permission() -> bool {
    ScreenCaptureAccess.preflight()
}

/// Open System Settings at Privacy & Security > Screen Recording.
pub fn open_screen_recording_settings() {
    const URL: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
    match std::process::Command::new("open").arg(URL).spawn() {
        Ok(_) => info!("Opened System Settings > Privacy & Security > Screen Recording"),
        Err(e) => warn!("Couldn't open System Settings: {e}"),
    }
}

/// Size and stride of a frame returned by [`take_latest_bgra`].
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: u32,
}

struct ReadyFrame {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
}

static READY: Mutex<ReadyFrame> = Mutex::new(ReadyFrame { bytes: Vec::new(), width: 0, height: 0, stride: 0 });
static BACK: Mutex<Vec<u8>> = Mutex::new(Vec::new());
// Incremented (while holding READY) each time a new frame is published. 0 = no frame yet.
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

static RUNNING: AtomicBool = AtomicBool::new(false);

// Stream handle kept alive while running; stored behind a Mutex in OnceLock to avoid Sync on SCStream.
static STREAM_HANDLE: OnceLock<Mutex<Option<SCStream>>> = OnceLock::new();

fn shareable_content() -> anyhow::Result<SCShareableContent> {
    SCShareableContent::get().map_err(|e| anyhow::anyhow!(
        "Can't list displays: {e:?}. If this is a permission error, grant Screen Recording to {} \
         in System Settings > Privacy & Security > Screen Recording, then restart it.",
        crate::macos_app::permission_holder(),
    ))
}

fn find_display(display_id: u32) -> anyhow::Result<SCDisplay> {
    // 0 means the main display (the one with the menu bar). Otherwise match by CGDirectDisplayID.
    let content = shareable_content()?;
    let displays = content.displays();
    let needle: CGDirectDisplayID = if display_id == 0 { CGDisplay::main().id } else { display_id };
    if let Some(d) = displays.iter().find(|d| d.display_id() == needle) {
        return Ok(d.clone());
    }
    if display_id == 0 {
        // Main display not in the shareable list (unexpected): fall back to the first one.
        return displays.first().cloned().ok_or_else(|| anyhow::anyhow!("No displays found"));
    }
    anyhow::bail!("Display id {display_id} not found. Run with --list-displays to see the available IDs.")
}

// ================= Windows =================

/// What to capture: a whole display or a single window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    /// A display, by CGDirectDisplayID (0 = main display).
    Display(u32),
    /// A window, by CGWindowID.
    Window(u32),
}

/// A window that can be captured, as listed by `--list-windows` and the menu bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    pub app: String,
    pub title: String,
    /// Size in points.
    pub width: u32,
    pub height: u32,
}

impl WindowInfo {
    /// "App: title", or just the app when the window has no title.
    pub fn label(&self) -> String {
        if self.title.is_empty() {
            self.app.clone()
        } else {
            format!("{}: {}", self.app, self.title)
        }
    }
}

/// Whether ScreenCaptureKit knows which app owns this window. Some system windows have none,
/// and the screencapturekit crate panics on a missing owner, so check first.
fn has_owner(window: &SCWindow) -> bool {
    use core_foundation::base::TCFType;
    use objc2::runtime::AnyObject;
    let ptr = window.as_concrete_TypeRef() as *const AnyObject;
    // SAFETY: `ptr` is the live SCWindow object; `owningApplication` is a nullable property.
    unsafe {
        let Some(obj) = ptr.as_ref() else { return false };
        let owner: *const AnyObject = objc2::msg_send![obj, owningApplication];
        !owner.is_null()
    }
}

/// Windows worth offering: normal app windows on screen, big enough to read, not our own.
fn capturable_windows(content: &SCShareableContent) -> Vec<WindowInfo> {
    let own_pid = std::process::id() as i32;
    let mut list: Vec<WindowInfo> = content
        .windows()
        .iter()
        .filter(|w| w.window_layer() == 0 && w.is_on_screen() && has_owner(w))
        .filter_map(|w| {
            let frame = w.get_frame();
            if frame.size.width < 40.0 || frame.size.height < 40.0 {
                return None;
            }
            let app = w.owning_application();
            if app.process_id() == own_pid {
                return None;
            }
            Some(WindowInfo {
                id: w.window_id(),
                app: app.application_name(),
                title: w.title(),
                width: frame.size.width as u32,
                height: frame.size.height as u32,
            })
        })
        .collect();
    list.sort_by(|a, b| {
        (a.app.to_lowercase(), a.title.to_lowercase(), a.id)
            .cmp(&(b.app.to_lowercase(), b.title.to_lowercase(), b.id))
    });
    list
}

/// List the windows that can be captured.
pub fn list_windows() -> anyhow::Result<Vec<WindowInfo>> {
    Ok(capturable_windows(&shareable_content()?))
}

/// Pick a window for `--window`: a window ID, or an app name or part of a window title.
/// A number is taken as an ID as it is. Otherwise an exact app name wins, then part of an app
/// name, then part of a title (ignoring case); the first match in `windows` is used.
pub fn match_window(windows: &[WindowInfo], query: &str) -> Option<u32> {
    let query = query.trim();
    if let Ok(id) = query.parse::<u32>() {
        return Some(id);
    }
    let q = query.to_lowercase();
    if q.is_empty() {
        return None;
    }
    windows
        .iter()
        .find(|w| w.app.to_lowercase() == q)
        .or_else(|| windows.iter().find(|w| w.app.to_lowercase().contains(&q)))
        .or_else(|| windows.iter().find(|w| w.title.to_lowercase().contains(&q)))
        .map(|w| w.id)
}

/// Resolve `--window` to a window ID.
pub fn find_window(query: &str) -> anyhow::Result<u32> {
    let windows = list_windows()?;
    match_window(&windows, query).ok_or_else(|| {
        anyhow::anyhow!("No window matches \"{query}\". Run with --list-windows to see the windows you can capture.")
    })
}

/// Pixels per point at `point` (global coordinates, top-left origin), from the display that
/// contains it. `displays` holds each display's frame (x, y, width, height in points) and its
/// scale. Off every display, use the sharpest scale, so the capture is never blurry.
fn scale_at(point: (f64, f64), displays: &[((f64, f64, f64, f64), f64)]) -> f64 {
    let (px, py) = point;
    displays
        .iter()
        .find(|((x, y, w, h), _)| px >= *x && px < x + w && py >= *y && py < y + h)
        .map(|(_, scale)| *scale)
        .or_else(|| displays.iter().map(|(_, s)| *s).reduce(f64::max))
        .unwrap_or(1.0)
}

/// A window's size in pixels, from its size in points and the scale of the display it's on.
fn window_pixel_size(window: &SCWindow, displays: &[SCDisplay]) -> (u32, u32) {
    let frames: Vec<_> = displays
        .iter()
        .map(|d| {
            let f = d.frame();
            let scale = if d.width() > 0 { display_pixel_size(d).0 as f64 / d.width() as f64 } else { 1.0 };
            ((f.origin.x, f.origin.y, f.size.width, f.size.height), scale)
        })
        .collect();
    let f = window.get_frame();
    let centre = (f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0);
    let scale = scale_at(centre, &frames);
    (
        ((f.size.width * scale).round() as u32).max(1),
        ((f.size.height * scale).round() as u32).max(1),
    )
}

// The window being captured (0 = a display) and its size in pixels when the stream started,
// packed as width << 32 | height. The watcher thread compares against them.
static WATCHED_WINDOW: AtomicU32 = AtomicU32::new(0);
static WATCHED_SIZE: AtomicU64 = AtomicU64::new(0);

const WINDOW_EVENT_NONE: u8 = 0;
const WINDOW_EVENT_RESIZED: u8 = 1;
const WINDOW_EVENT_CLOSED: u8 = 2;
static WINDOW_EVENT: AtomicU8 = AtomicU8::new(WINDOW_EVENT_NONE);

/// Something happened to the captured window that needs the stream restarted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedWindowEvent {
    /// It changed size: restart the capture at the new size.
    Resized,
    /// It was closed: capture something else.
    Closed,
}

/// Take the latest event about the captured window, if any. One atomic swap.
pub fn take_window_event() -> Option<CapturedWindowEvent> {
    match WINDOW_EVENT.swap(WINDOW_EVENT_NONE, Ordering::AcqRel) {
        WINDOW_EVENT_RESIZED => Some(CapturedWindowEvent::Resized),
        WINDOW_EVENT_CLOSED => Some(CapturedWindowEvent::Closed),
        _ => None,
    }
}

static WINDOWS: Mutex<Vec<WindowInfo>> = Mutex::new(Vec::new());
// Incremented when WINDOWS changes. 0 = not listed yet.
static WINDOWS_SEQ: AtomicU64 = AtomicU64::new(0);

/// The windows that can be captured, if the list changed since `last_seq`. Never blocks.
pub fn windows_if_changed(last_seq: &mut u64) -> Option<Vec<WindowInfo>> {
    let seq = WINDOWS_SEQ.load(Ordering::Acquire);
    if seq == *last_seq {
        return None;
    }
    let list = WINDOWS.try_lock().ok()?;
    *last_seq = seq;
    Some(list.clone())
}

/// Start a background thread that, every two seconds, lists the windows that can be captured
/// (for the menu bar) and checks whether the captured window was resized or closed. Listing
/// windows with ScreenCaptureKit waits on another process, so it stays off the render thread.
pub fn start_window_watcher() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new().name("window-watcher".into()).spawn(|| loop {
        if has_screen_recording_permission() {
            if let Ok(content) = SCShareableContent::get() {
                let list = capturable_windows(&content);
                if let Ok(mut current) = WINDOWS.lock() {
                    if *current != list {
                        *current = list;
                        WINDOWS_SEQ.fetch_add(1, Ordering::Release);
                    }
                }
                check_watched_window(&content);
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    });
    if let Err(e) = spawned {
        warn!("Couldn't start the window watcher: {e}");
    }
}

fn check_watched_window(content: &SCShareableContent) {
    let id = WATCHED_WINDOW.load(Ordering::Acquire);
    if id == 0 {
        return;
    }
    // Look at every window, not just on-screen ones: a minimised window or one on another
    // Space still exists.
    let event = match content.windows().iter().find(|w| w.window_id() == id) {
        None => WINDOW_EVENT_CLOSED,
        Some(w) => {
            let (pw, ph) = window_pixel_size(w, &content.displays());
            if pack_size(pw, ph) == WATCHED_SIZE.load(Ordering::Acquire) {
                return;
            }
            WINDOW_EVENT_RESIZED
        }
    };
    // Only report it if that window is still the one being captured.
    if WATCHED_WINDOW.load(Ordering::Acquire) == id {
        WINDOW_EVENT.store(event, Ordering::Release);
    }
}

fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

/// A display that can be captured, as listed by `--list-displays`.
#[derive(Clone, Copy, Debug)]
pub struct DisplayInfo {
    pub id: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub is_main: bool,
}

pub fn list_displays() -> anyhow::Result<Vec<DisplayInfo>> {
    let content = shareable_content()?;
    let main_id = CGDisplay::main().id;
    Ok(content.displays().iter().map(|d| {
        let (pixel_width, pixel_height) = display_pixel_size(d);
        DisplayInfo { id: d.display_id(), pixel_width, pixel_height, is_main: d.display_id() == main_id }
    }).collect())
}

/// Native pixel size of a display. `SCDisplay::width/height` are in points, which is half
/// the real resolution on a Retina display, so ask CoreGraphics for the current mode instead.
fn display_pixel_size(display: &SCDisplay) -> (u32, u32) {
    let points = (display.width(), display.height());
    match CGDisplay::new(display.display_id()).display_mode() {
        Some(mode) if mode.pixel_width() > 0 && mode.pixel_height() > 0 => {
            (mode.pixel_width() as u32, mode.pixel_height() as u32)
        }
        _ => points,
    }
}

fn log_limited(counter: &AtomicUsize, msg: impl FnOnce()) {
    if counter.fetch_add(1, Ordering::Relaxed) < 10 {
        msg();
    }
}

struct ScreenOutput;

impl screencapturekit::stream::output_trait::SCStreamOutputTrait for ScreenOutput {
    fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, _of_type: SCStreamOutputType) {
        use screencapturekit::output::sc_stream_frame_info::{SCFrameStatus, SCStreamFrameInfo};
        static SKIPPED: AtomicUsize = AtomicUsize::new(0);
        static NO_PIXELS: AtomicUsize = AtomicUsize::new(0);
        static FRAMES: AtomicUsize = AtomicUsize::new(0);

        // Only process complete frames. Idle frames (screen unchanged) carry no new pixels.
        match SCStreamFrameInfo::from_sample_buffer(&sample_buffer) {
            Ok(info) => {
                let status = info.status();
                if status != SCFrameStatus::Complete {
                    if status != SCFrameStatus::Idle {
                        log_limited(&SKIPPED, || warn!("Skipping non-complete frame, status={:?}", status));
                    }
                    return;
                }
            }
            Err(e) => {
                log_limited(&SKIPPED, || warn!("SCStreamFrameInfo::from_sample_buffer failed: {:?}", e));
                return;
            }
        }

        let pixel_buffer = match sample_buffer.get_pixel_buffer() {
            Ok(pb) => pb,
            Err(e) => {
                log_limited(&NO_PIXELS, || warn!("Sample buffer has no pixel buffer: {:?}", e));
                return;
            }
        };

        // Use the pixel buffer's own size: it is in pixels and matches the bytes we copy.
        let width = pixel_buffer.get_width();
        let height = pixel_buffer.get_height();
        let stride = pixel_buffer.get_bytes_per_row();
        if width == 0 || height == 0 || stride < width.saturating_mul(4) {
            log_limited(&NO_PIXELS, || warn!("Invalid pixel buffer: {}x{} stride={}", width, height, stride));
            return;
        }

        let guard = match pixel_buffer.lock() {
            Ok(g) => g,
            Err(e) => {
                log_limited(&NO_PIXELS, || warn!("CVPixelBuffer lock failed: {:?}", e));
                return;
            }
        };
        publish_frame(guard.as_slice(), width, height, stride);
        drop(guard);

        log_limited(&FRAMES, || info!("Captured BGRA frame: {}x{} stride={}", width, height, stride));
    }
}

/// Make `pixels` the newest frame: copy it into BACK outside the READY lock, then swap BACK
/// into READY. `clear` + `extend_from_slice` reuses the existing allocation once the buffers
/// have reached frame size, so steady-state capture doesn't allocate.
fn publish_frame(pixels: &[u8], width: u32, height: u32, stride: u32) {
    let mut back = BACK.lock().unwrap_or_else(|e| e.into_inner());
    back.clear();
    back.extend_from_slice(pixels);

    // Publish: O(1) swap under the lock.
    let mut ready = READY.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::swap(&mut ready.bytes, &mut *back);
    ready.width = width;
    ready.height = height;
    ready.stride = stride;
    FRAME_SEQ.fetch_add(1, Ordering::Release);
}

/// Start capturing a display or window.
pub fn start_stream(source: CaptureSource, show_cursor: bool) -> anyhow::Result<()> {
    if RUNNING.load(Ordering::SeqCst) {
        return Ok(());
    }
    let result = start_stream_inner(source, show_cursor);
    let status = match &result {
        Ok(()) => STATUS_RUNNING,
        Err(_) if !has_screen_recording_permission() => STATUS_NO_PERMISSION,
        Err(_) => STATUS_FAILED,
    };
    STATUS.store(status, Ordering::Release);
    result
}

fn start_stream_inner(source: CaptureSource, show_cursor: bool) -> anyhow::Result<()> {
    // Check Screen Recording permission first. If it's missing, ask macOS to show its prompt
    // (it only does so once per app; after that the user has to use System Settings).
    if !has_screen_recording_permission() {
        ScreenCaptureAccess.request();
        anyhow::bail!(
            "Screen Recording permission is not granted. Allow {} in System Settings > \
             Privacy & Security > Screen Recording (press P in the app to open it), then restart {}.",
            crate::macos_app::permission_holder(),
            crate::macos_app::restart_target(),
        );
    }

    // Capture at the native pixel size. Without an explicit size ScreenCaptureKit picks its
    // own default, which does not match the display or window.
    let (filter, (px_w, px_h)) = match source {
        CaptureSource::Display(display_id) => {
            let display = find_display(display_id).map_err(|e| {
                warn!("ShareableContent/Display failed: {e}");
                e
            })?;
            let size = display_pixel_size(&display);
            info!("Capturing display {} at {}x{} px", display.display_id(), size.0, size.1);
            (SCContentFilter::new().with_display_excluding_windows(&display, &[]), size)
        }
        CaptureSource::Window(window_id) => {
            let content = shareable_content()?;
            let window = content
                .windows()
                .into_iter()
                .find(|w| w.window_id() == window_id)
                .ok_or_else(|| anyhow::anyhow!(
                    "Window {window_id} not found (was it closed?). Run with --list-windows to see the windows you can capture."
                ))?;
            let size = window_pixel_size(&window, &content.displays());
            info!("Capturing window {window_id} at {}x{} px", size.0, size.1);
            WATCHED_SIZE.store(pack_size(size.0, size.1), Ordering::Release);
            WATCHED_WINDOW.store(window_id, Ordering::Release);
            // Follows the window across displays and Spaces, and only shows that window.
            (SCContentFilter::new().with_desktop_independent_window(&window), size)
        }
    };

    // Some setters return Result<Self, CFError>, and CFError doesn't implement Into<anyhow::Error>,
    // so map each error to a message.
    let cfg = SCStreamConfiguration::new()
        .set_width(px_w)
        .map_err(|_| anyhow::anyhow!("set_width failed"))?
        .set_height(px_h)
        .map_err(|_| anyhow::anyhow!("set_height failed"))?
        .set_pixel_format(PixelFormat::BGRA)
        .map_err(|_| anyhow::anyhow!("set_pixel_format failed"))?
        .set_captures_audio(false)
        .map_err(|_| anyhow::anyhow!("set_captures_audio failed"))?
        .set_shows_cursor(show_cursor)
        .map_err(|_| anyhow::anyhow!("set_shows_cursor failed"))?;

    let mut stream = SCStream::new(&filter, &cfg);
    let _handler_id = stream.add_output_handler(ScreenOutput, SCStreamOutputType::Screen);

    stream.start_capture()
        .map_err(|e| anyhow::anyhow!("start_capture failed: {e:?}"))?;

    RUNNING.store(true, Ordering::SeqCst);
    // store stream to keep it alive
    STREAM_HANDLE.get_or_init(|| Mutex::new(None))
        .lock().unwrap_or_else(|e| e.into_inner())
        .replace(stream);
    info!("Screen capture stream started via screencapturekit.");

    Ok(())
}

/// Capture a different display or window (or the same window again at its new size): stop
/// the current stream and start a new one. Blocks briefly while ScreenCaptureKit lists the
/// displays and windows, so don't call it per frame.
pub fn switch_source(source: CaptureSource, show_cursor: bool) -> anyhow::Result<()> {
    stop_stream();
    STATUS.store(STATUS_STARTING, Ordering::Release);
    start_stream(source, show_cursor)
}

/// Turn a display ID from the command line into a real one (0 means the main display).
pub fn resolve_display_id(display_id: u32) -> u32 {
    if display_id == 0 { CGDisplay::main().id } else { display_id }
}

/// Take the newest captured frame if one arrived since `last_seq`.
///
/// Swaps the frame into `buf` (no copy, no allocation) and updates `last_seq`. Returns `None`
/// without locking when there is no new frame, and never blocks: if the capture thread is
/// publishing at that instant, the frame is picked up on the next call instead.
pub fn take_latest_bgra(buf: &mut Vec<u8>, last_seq: &mut u64) -> Option<FrameInfo> {
    if FRAME_SEQ.load(Ordering::Acquire) == *last_seq {
        return None;
    }
    let mut ready = match READY.try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => return None,
    };
    // Read the sequence under the lock so it matches the frame we take.
    *last_seq = FRAME_SEQ.load(Ordering::Acquire);
    if ready.width == 0 || ready.height == 0 || ready.bytes.is_empty() {
        return None;
    }
    std::mem::swap(&mut ready.bytes, buf);
    Some(FrameInfo { width: ready.width, height: ready.height, bytes_per_row: ready.stride })
}

// Stop stream (optional)
pub fn stop_stream() {
    WATCHED_WINDOW.store(0, Ordering::Release);
    WINDOW_EVENT.store(WINDOW_EVENT_NONE, Ordering::Release);
    if RUNNING.swap(false, Ordering::SeqCst) {
        if let Some(m) = STREAM_HANDLE.get() {
            if let Ok(mut guard) = m.lock() {
                if let Some(stream) = guard.take() {
                    if let Err(e) = stream.stop_capture() {
                        warn!("stop_capture failed: {e:?}");
                    }
                }
            }
        }
        info!("Screen capture stream stopped.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u32, app: &str, title: &str) -> WindowInfo {
        WindowInfo { id, app: app.into(), title: title.into(), width: 800, height: 600 }
    }

    #[test]
    fn window_queries_match_ids_apps_then_titles() {
        let windows = [
            window(10, "Safari", "Rust docs"),
            window(11, "Safari Technology Preview", "Start"),
            window(12, "Terminal", "cargo run — safari notes"),
            window(13, "Preview", "safari.png"),
        ];
        assert_eq!(match_window(&windows, "4242"), Some(4242), "a number is an ID as it is");
        assert_eq!(match_window(&windows, "safari"), Some(10), "exact app name first");
        assert_eq!(match_window(&windows, "technology"), Some(11), "then part of an app name");
        assert_eq!(match_window(&windows, "Preview"), Some(13), "an exact name beats a partial one");
        assert_eq!(match_window(&windows, "NOTES"), Some(12), "then part of a title, ignoring case");
        assert_eq!(match_window(&windows, "mail"), None);
        assert_eq!(match_window(&windows, "  "), None);
    }

    #[test]
    fn window_labels() {
        assert_eq!(window(1, "Safari", "Docs").label(), "Safari: Docs");
        assert_eq!(window(1, "Finder", "").label(), "Finder");
    }

    #[test]
    fn window_scale_comes_from_the_display_it_is_on() {
        // A Retina display with a 1x display to its right.
        let displays = [((0.0, 0.0, 1512.0, 982.0), 2.0), ((1512.0, 0.0, 1920.0, 1080.0), 1.0)];
        assert_eq!(scale_at((100.0, 100.0), &displays), 2.0);
        assert_eq!(scale_at((2000.0, 500.0), &displays), 1.0);
        // Off every display (dragged mostly off screen): the sharpest scale.
        assert_eq!(scale_at((-500.0, -500.0), &displays), 2.0);
        assert_eq!(scale_at((0.0, 0.0), &[]), 1.0);
    }

    // One test, because the handoff state is global and tests run in parallel.
    #[test]
    fn frame_handoff_swaps_newest_frame_without_copies() {
        let mut buf = Vec::new();
        let mut seq = FRAME_SEQ.load(Ordering::Acquire);

        // Nothing new yet.
        assert!(take_latest_bgra(&mut buf, &mut seq).is_none());

        // Only the newest of two frames is handed over.
        publish_frame(&[1; 16], 2, 2, 8);
        publish_frame(&[2; 32], 2, 4, 8);
        let info = take_latest_bgra(&mut buf, &mut seq).expect("a new frame");
        assert_eq!((info.width, info.height, info.bytes_per_row), (2, 4, 8));
        assert_eq!(buf, vec![2; 32]);

        // The same frame isn't handed over twice.
        assert!(take_latest_bgra(&mut buf, &mut seq).is_none());

        // Once the buffers have reached frame size, publishing and taking reuse them.
        publish_frame(&[3; 32], 2, 4, 8);
        let ptr_before = buf.as_ptr();
        take_latest_bgra(&mut buf, &mut seq).expect("a new frame");
        publish_frame(&[4; 32], 2, 4, 8);
        take_latest_bgra(&mut buf, &mut seq).expect("a new frame");
        publish_frame(&[5; 32], 2, 4, 8);
        take_latest_bgra(&mut buf, &mut seq).expect("a new frame");
        assert_eq!(buf, vec![5; 32]);
        // Three buffers rotate, so after three handoffs the renderer has its first one back.
        assert_eq!(buf.as_ptr(), ptr_before);

        // An empty frame isn't handed over, but its sequence number is consumed.
        publish_frame(&[], 0, 0, 0);
        assert!(take_latest_bgra(&mut buf, &mut seq).is_none());
        assert!(take_latest_bgra(&mut buf, &mut seq).is_none());
    }
}
