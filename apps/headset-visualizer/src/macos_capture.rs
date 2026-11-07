#![cfg(target_os = "macos")]

// ScreenCaptureKit integration via the screencapturekit crate.
// Captures BGRA frames from the selected display and hands the newest one to the renderer.
//
// Frame handoff uses three rotating buffers so neither side allocates or holds a lock for long:
// - BACK: written by the capture callback (only that thread touches it).
// - READY: the newest complete frame. The capture callback swaps BACK into it; the renderer
//   swaps its own buffer out of it. Both swaps are O(1), so the lock is held only briefly.
// - The renderer's own buffer, passed to `take_latest_bgra`.
// FRAME_SEQ lets the renderer see "nothing new" with a single atomic load and skip the upload.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};
use log::{info, warn};

use screencapturekit::{
    output::{CMSampleBuffer, LockTrait},
    shareable_content::{SCDisplay, SCShareableContent},
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
    /// macOS hasn't granted Screen Recording to this process (in practice, to the terminal app).
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

/// Whether macOS currently allows this process to record the screen. For a command-line app
/// the permission belongs to the terminal it runs in, and a newly granted permission only
/// takes effect after that terminal is restarted.
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

static STARTED: OnceLock<()> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);

// Stream handle kept alive while running; stored behind a Mutex in OnceLock to avoid Sync on SCStream.
static STREAM_HANDLE: OnceLock<Mutex<Option<SCStream>>> = OnceLock::new();

fn shareable_content() -> anyhow::Result<SCShareableContent> {
    SCShareableContent::get().map_err(|e| anyhow::anyhow!(
        "Can't list displays: {e:?}. If this is a permission error, grant Screen Recording to your \
         terminal app in System Settings > Privacy & Security > Screen Recording, then restart it."
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

        // Copy into the back buffer outside the READY lock. `clear` + `extend_from_slice`
        // reuses the existing allocation once the buffers have reached frame size.
        let mut back = BACK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let guard = match pixel_buffer.lock() {
                Ok(g) => g,
                Err(e) => {
                    log_limited(&NO_PIXELS, || warn!("CVPixelBuffer lock failed: {:?}", e));
                    return;
                }
            };
            back.clear();
            back.extend_from_slice(guard.as_slice());
        }

        // Publish: O(1) swap under the lock.
        {
            let mut ready = READY.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::swap(&mut ready.bytes, &mut *back);
            ready.width = width;
            ready.height = height;
            ready.stride = stride;
            FRAME_SEQ.fetch_add(1, Ordering::Release);
        }

        log_limited(&FRAMES, || info!("Captured BGRA frame: {}x{} stride={}", width, height, stride));
    }
}

// Start capture stream
pub fn start_stream(display_id: u32, show_cursor: bool) -> anyhow::Result<()> {
    if STARTED.get().is_some() {
        return Ok(());
    }
    let result = start_stream_inner(display_id, show_cursor);
    let status = match &result {
        Ok(()) => STATUS_RUNNING,
        Err(_) if !has_screen_recording_permission() => STATUS_NO_PERMISSION,
        Err(_) => STATUS_FAILED,
    };
    STATUS.store(status, Ordering::Release);
    result
}

fn start_stream_inner(display_id: u32, show_cursor: bool) -> anyhow::Result<()> {
    // Check Screen Recording permission first. If it's missing, ask macOS to show its prompt
    // (it only does so once per app; after that the user has to use System Settings).
    if !has_screen_recording_permission() {
        ScreenCaptureAccess.request();
        anyhow::bail!(
            "Screen Recording permission is not granted. Allow your terminal app in System Settings > \
             Privacy & Security > Screen Recording (press P in the app to open it), then restart the terminal."
        );
    }

    let display = find_display(display_id).map_err(|e| {
        warn!("ShareableContent/Display failed: {e}");
        e
    })?;

    // Capture at the display's native pixel size. Without an explicit size ScreenCaptureKit
    // picks its own default, which does not match the display.
    let (px_w, px_h) = display_pixel_size(&display);
    info!("Capturing display {} at {}x{} px", display.display_id(), px_w, px_h);

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

    let filter = SCContentFilter::new().with_display_excluding_windows(&display, &[]);
    let mut stream = SCStream::new(&filter, &cfg);
    let _handler_id = stream.add_output_handler(ScreenOutput, SCStreamOutputType::Screen);

    stream.start_capture()
        .map_err(|e| anyhow::anyhow!("start_capture failed: {e:?}"))?;

    RUNNING.store(true, Ordering::SeqCst);
    // store stream to keep it alive
    STREAM_HANDLE.get_or_init(|| Mutex::new(None))
        .lock().unwrap_or_else(|e| e.into_inner())
        .replace(stream);
    STARTED.set(()).ok();
    info!("Screen capture stream started via screencapturekit.");

    Ok(())
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
    if RUNNING.swap(false, Ordering::SeqCst) {
        if let Some(m) = STREAM_HANDLE.get() {
            if let Ok(mut guard) = m.lock() {
                // Drop to stop the stream
                let _ = guard.take();
            }
        }
        info!("Screen capture stream stopped.");
    }
}
