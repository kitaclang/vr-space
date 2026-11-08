#![cfg(target_os = "macos")]

// ScreenCaptureKit integration via the screencapturekit crate.
// Captures BGRA frames from the selected display or window and hands the newest one to the
// renderer.
//
// Frames aren't copied here: the capture callback keeps a reference to the frame's pixel
// buffer (an IOSurface the GPU can read in place, see `iosurface.rs`) and publishes it in
// LATEST, replacing any frame the renderer hasn't taken. The renderer takes it with
// `take_latest_frame`. Both sides only move a pointer under the lock, and its sequence counter
// lets the renderer see "nothing new" with a single atomic load.
//
// Every frame the app holds keeps ScreenCaptureKit from reusing that surface, so the stream's
// queue depth (QUEUE_DEPTH) leaves room for the ones the renderer keeps while the GPU draws.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};
use core_foundation::base::TCFType;
use std::time::Duration;
use log::{info, warn};

use screencapturekit::{
    output::{CMSampleBuffer, CVPixelBuffer, LockTrait},
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
        STATUS_RUNNING if LATEST.seq() == 0 => CaptureStatus::WaitingForFrames,
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

/// A captured BGRA frame. Holding it keeps its pixels (and its IOSurface) unchanged:
/// ScreenCaptureKit only reuses the memory once every reference is dropped.
pub struct CapturedFrame {
    buffer: CVPixelBuffer,
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: u32,
    /// When this image was on the Mac's display, in mach absolute time (0 if unknown).
    pub displayed_at: u64,
}

// SAFETY: a CVPixelBuffer is a reference-counted CoreFoundation object, safe to retain and
// release from any thread. Its pixels are only read: in place by the GPU, or after a
// read-only lock.
unsafe impl Send for CapturedFrame {}

impl CapturedFrame {
    /// The IOSurface holding the pixels, valid while `self` is.
    pub fn iosurface(&self) -> Option<crate::iosurface::IOSurfaceRef> {
        // SAFETY: `self.buffer` is a live CVPixelBuffer.
        unsafe { crate::iosurface::surface_of_pixel_buffer(self.buffer.as_CFTypeRef()) }
    }

    /// Run `f` on the pixels, rows `bytes_per_row` apart. `None` if they can't be read.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        static FAILED: AtomicUsize = AtomicUsize::new(0);
        match self.buffer.lock() {
            Ok(guard) => Some(f(guard.as_slice())),
            Err(e) => {
                log_limited(&FAILED, || warn!("CVPixelBuffer lock failed: {:?}", e));
                None
            }
        }
    }
}

/// The newest value published, and a count of how many have been.
struct Latest<T> {
    slot: Mutex<Option<T>>,
    // Incremented (while holding `slot`) on every publish. 0 = nothing yet.
    seq: AtomicU64,
}

impl<T> Latest<T> {
    const fn new() -> Self {
        Self { slot: Mutex::new(None), seq: AtomicU64::new(0) }
    }

    fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Make `value` the newest, dropping one that wasn't taken (outside the lock).
    fn publish(&self, value: T) {
        let replaced = {
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            let replaced = slot.replace(value);
            self.seq.fetch_add(1, Ordering::Release);
            replaced
        };
        drop(replaced);
    }

    /// Take the newest value if one was published since `last_seq`, updating it. Never
    /// blocks: if the other side holds the lock at that instant, try again next time.
    fn take(&self, last_seq: &mut u64) -> Option<T> {
        if self.seq() == *last_seq {
            return None;
        }
        let mut slot = match self.slot.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        // Read the sequence under the lock so it matches the value taken.
        *last_seq = self.seq();
        slot.take()
    }
}

static LATEST: Latest<CapturedFrame> = Latest::new();

/// Frames ScreenCaptureKit may have waiting for the app. The default, 3, would stall the
/// stream: the app can hold one waiting here plus the few the renderer keeps until the GPU
/// is done with them (see `main.rs`). 8 is the most ScreenCaptureKit allows.
const QUEUE_DEPTH: u32 = 6;

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

/// Find the window shown last time: same app and title, or else any window of that app
/// (titles change, for example with the open document).
pub fn match_saved_window(windows: &[WindowInfo], app: &str, title: &str) -> Option<u32> {
    windows
        .iter()
        .find(|w| w.app == app && w.title == title)
        .or_else(|| windows.iter().find(|w| w.app == app))
        .map(|w| w.id)
}

/// The window with this ID from the watcher's latest list, if it's there.
pub fn window_info(id: u32) -> Option<WindowInfo> {
    WINDOWS.lock().ok()?.iter().find(|w| w.id == id).cloned()
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
        let displayed_at = match SCStreamFrameInfo::from_sample_buffer(&sample_buffer) {
            Ok(info) => {
                let status = info.status();
                if status != SCFrameStatus::Complete {
                    if status != SCFrameStatus::Idle {
                        log_limited(&SKIPPED, || warn!("Skipping non-complete frame, status={:?}", status));
                    }
                    return;
                }
                info.display_time()
            }
            Err(e) => {
                log_limited(&SKIPPED, || warn!("SCStreamFrameInfo::from_sample_buffer failed: {:?}", e));
                return;
            }
        };

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

        // Keep the pixel buffer (a retain, not a copy) and hand it over.
        LATEST.publish(CapturedFrame { buffer: pixel_buffer, width, height, bytes_per_row: stride, displayed_at });

        log_limited(&FRAMES, || info!("Captured BGRA frame: {}x{} stride={}", width, height, stride));
    }
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
        .map_err(|_| anyhow::anyhow!("set_shows_cursor failed"))?
        .set_queue_depth(QUEUE_DEPTH)
        .map_err(|_| anyhow::anyhow!("set_queue_depth failed"))?;

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

/// Take the newest captured frame if one arrived since `last_seq`, updating it.
///
/// Returns `None` without locking when there is no new frame, and never blocks: if the
/// capture thread is publishing at that instant, the frame is picked up on the next call.
pub fn take_latest_frame(last_seq: &mut u64) -> Option<CapturedFrame> {
    LATEST.take(last_seq)
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
    fn saved_windows_are_found_by_app_and_title() {
        let windows = [window(10, "Safari", "Start"), window(11, "Safari", "Docs"), window(12, "Notes", "Docs")];
        assert_eq!(match_saved_window(&windows, "Safari", "Docs"), Some(11));
        assert_eq!(match_saved_window(&windows, "Safari", "Old page"), Some(10), "same app, title changed");
        assert_eq!(match_saved_window(&windows, "Mail", "Docs"), None, "app not open");
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

    #[test]
    fn the_renderer_gets_only_the_newest_frame() {
        let latest: Latest<u32> = Latest::new();
        let mut seq = 0;
        assert_eq!(latest.take(&mut seq), None, "nothing published yet");

        latest.publish(1);
        latest.publish(2);
        assert_eq!(latest.take(&mut seq), Some(2), "only the newest of two");
        assert_eq!(seq, 2);
        assert_eq!(latest.take(&mut seq), None, "not handed over twice");

        latest.publish(3);
        assert_eq!(latest.take(&mut seq), Some(3));
    }

    #[test]
    fn frames_that_are_replaced_or_taken_are_not_kept() {
        use std::sync::Arc;
        // Each frame holds the capture's surface until dropped, so the handoff must not keep
        // a replaced frame or a copy of a taken one.
        let latest: Latest<Arc<()>> = Latest::new();
        let (first, second) = (Arc::new(()), Arc::new(()));
        latest.publish(first.clone());
        latest.publish(second.clone());
        assert_eq!(Arc::strong_count(&first), 1, "a replaced frame is released");
        let mut seq = 0;
        let taken = latest.take(&mut seq).expect("a frame");
        assert!(Arc::ptr_eq(&taken, &second));
        drop(taken);
        assert_eq!(Arc::strong_count(&second), 1, "the handoff keeps nothing it handed over");
    }
}
