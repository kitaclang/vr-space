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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

use core_graphics::display::{CGDirectDisplayID, CGDisplay};

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

fn find_display(display_id: u32) -> anyhow::Result<SCDisplay> {
    // 0 means primary. For non-zero, try to match by CGDirectDisplayID.
    let content = SCShareableContent::get()
        .map_err(|e| anyhow::anyhow!("SCShareableContent::get failed: {e:?}"))?;
    if display_id == 0 {
        return content.displays().first().cloned()
            .ok_or_else(|| anyhow::anyhow!("No displays found"));
    }
    let needle: CGDirectDisplayID = display_id as CGDirectDisplayID;
    content.displays().into_iter()
        .find(|d| d.display_id() == needle)
        .ok_or_else(|| anyhow::anyhow!("Display id {} not found", display_id))
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
pub fn start_stream(display_id: u32) -> anyhow::Result<()> {
    if STARTED.get().is_some() {
        return Ok(());
    }

    // Permissions: this will implicitly trigger Screen Recording permission if missing
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
        .set_shows_cursor(false)
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
