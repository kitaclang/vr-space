#![cfg(target_os = "macos")]

// ScreenCaptureKit integration via screencapturekit crate (no ObjC shim).
// Captures BGRA frames from the selected display and exposes a latest-frame copy API.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use log::{info, warn};

use screencapturekit::{
    output::CMSampleBuffer,
    shareable_content::{SCDisplay, SCShareableContent},
    stream::{
        configuration::{pixel_format::PixelFormat, SCStreamConfiguration},
        content_filter::SCContentFilter,
        output_type::SCStreamOutputType,
        SCStream,
    },
};

use core_graphics::display::CGDirectDisplayID;
// Use only the public re-exports that exist in screencapturekit 0.3.5
// Note: older screencapturekit versions may not expose helpers; keep only CVPixelBufferRef if needed.

// Latest BGRA frame container returned to the renderer
#[derive(Clone)]
pub struct BGRAFrame {
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: u32, // stride
    pub bytes: Vec<u8>,
}

// Simple ring buffer for the latest frame
struct LatestFrame {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
}

impl LatestFrame {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            width: 0,
            height: 0,
            stride: 0,
        }
    }
}

// Global state
static LATEST: OnceLock<std::sync::Mutex<LatestFrame>> = OnceLock::new();
static STARTED: OnceLock<()> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);

// Stream handle kept alive while running; stored behind a Mutex in OnceLock to avoid Sync on SCStream.
static STREAM_HANDLE: OnceLock<std::sync::Mutex<Option<SCStream>>> = OnceLock::new();

// Keep only the stream; filter will be owned by stream internally, no need to keep it in static to avoid Send/Sync bounds on SCContentFilter.

// Delegate to receive frames
struct FrameDelegate; // kept in case future delegates are needed (currently unused)

// Initialize global latest buffer holder
fn latest() -> &'static std::sync::Mutex<LatestFrame> {
    LATEST.get_or_init(|| std::sync::Mutex::new(LatestFrame::new()))
}

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

// Start capture stream
pub fn start_stream(display_id: u32) -> anyhow::Result<()> {
    if STARTED.get().is_some() {
        return Ok(());
    }

    // Permissions: this will implicitly trigger Screen Recording permission if missing
    let display = find_display(display_id).map_err(|e| {
        log::warn!("ShareableContent/Display failed: {e}");
        e
    })?;

    // Configure stream: BGRA, no scaling, no cursor by default
    // set_pixel_format returns Self, but set_shows_cursor returns Result<Self, CFError>.
    // Build step-by-step and propagate errors.
    // set_pixel_format returns Self; set_shows_cursor returns Result<Self, CFError>.
    // Use the ? operator to propagate any CFError as anyhow::Error.
    // Build configuration. Some setters return Result<Self, CFError> but CFError doesn't implement Into<anyhow::Error>.
    // Avoid `?` here; unwrap or map to string to satisfy anyhow::Result without Send/Sync constraints.
    let mut cfg = SCStreamConfiguration::new();
    cfg = cfg
        .set_pixel_format(PixelFormat::BGRA)
        .map_err(|_| anyhow::anyhow!("set_pixel_format failed"))?;
    // Ensure we don't scale so stride/size reflect display pixels
    cfg = cfg
        .set_captures_audio(false)
        .map_err(|_| anyhow::anyhow!("set_captures_audio failed"))?;
    cfg = cfg
        .set_shows_cursor(false)
        .map_err(|_| anyhow::anyhow!("set_shows_cursor failed"))?;

    // Build content filter and stream
    let filter = SCContentFilter::new().with_display_excluding_windows(&display, &[]);

    // Create stream (no delegate needed for frame delivery; we'll use an output handler)
    let mut stream = SCStream::new(&filter, &cfg);

    // Implement SCStreamOutputTrait inline to receive screen frames
    struct ScreenOutput;
    impl screencapturekit::stream::output_trait::SCStreamOutputTrait for ScreenOutput {
        fn did_output_sample_buffer(
            &self,
            sample_buffer: CMSampleBuffer,
            _of_type: SCStreamOutputType,
        ) {
            use screencapturekit::output::sc_stream_frame_info::SCStreamFrameInfo;
            use std::sync::atomic::AtomicUsize;

            // Check data readiness and frame status before touching CVPixelBuffer
            unsafe {
                #[allow(improper_ctypes)]
                extern "C" {
                    fn CMSampleBufferDataIsReady(sb: *const core::ffi::c_void) -> bool;

                    // CoreMedia/Video
                    fn CMSampleBufferGetImageBuffer(sb: *const core::ffi::c_void) -> *mut core::ffi::c_void;
                    fn CVPixelBufferLockBaseAddress(pixelBuffer: *mut core::ffi::c_void, flags: u64) -> i32;
                    fn CVPixelBufferUnlockBaseAddress(pixelBuffer: *mut core::ffi::c_void, flags: u64) -> i32;
                    fn CVPixelBufferGetBaseAddress(pixelBuffer: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
                    fn CVPixelBufferGetBytesPerRow(pixelBuffer: *mut core::ffi::c_void) -> usize;
                    fn CVPixelBufferGetWidth(pixelBuffer: *mut core::ffi::c_void) -> usize;
                    fn CVPixelBufferGetHeight(pixelBuffer: *mut core::ffi::c_void) -> usize;
                }

                let sb_ptr = &sample_buffer as *const _ as *const core::ffi::c_void;
                if !CMSampleBufferDataIsReady(sb_ptr) {
                    // Not ready; skip to avoid null image buffer
                    return;
                }
            }

            // Frame info for dimensions and status
            let info = match SCStreamFrameInfo::from_sample_buffer(&sample_buffer) {
                Ok(i) => i,
                Err(e) => {
                    warn!("SCStreamFrameInfo::from_sample_buffer failed: {:?}", e);
                    return;
                }
            };

            // Only process complete frames
            {
                use screencapturekit::output::sc_stream_frame_info::SCFrameStatus;
                let status = info.status();
                if status != SCFrameStatus::Complete {
                    static SKIP_LOGGED: OnceLock<AtomicUsize> = OnceLock::new();
                    let n = SKIP_LOGGED.get_or_init(|| AtomicUsize::new(0)).fetch_add(1, Ordering::Relaxed);
                    if n < 10 {
                        warn!("Skipping non-complete frame, status={:?}", status);
                    }
                    return;
                }
            }

            // Dimensions from content_rect, fallback to CVPixelBuffer width/height if needed
            let mut width = 0usize;
            let mut height = 0usize;
            if let Some(rect) = info.content_rect() {
                width = rect.size.width.max(0.0) as usize;
                height = rect.size.height.max(0.0) as usize;
            }

            unsafe {
                #[allow(improper_ctypes)]
                extern "C" {
                    fn CMSampleBufferGetImageBuffer(sb: *const core::ffi::c_void) -> *mut core::ffi::c_void;
                    fn CVPixelBufferLockBaseAddress(pixelBuffer: *mut core::ffi::c_void, flags: u64) -> i32;
                    fn CVPixelBufferUnlockBaseAddress(pixelBuffer: *mut core::ffi::c_void, flags: u64) -> i32;
                    fn CVPixelBufferGetBaseAddress(pixelBuffer: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
                    fn CVPixelBufferGetBytesPerRow(pixelBuffer: *mut core::ffi::c_void) -> usize;
                    fn CVPixelBufferGetWidth(pixelBuffer: *mut core::ffi::c_void) -> usize;
                    fn CVPixelBufferGetHeight(pixelBuffer: *mut core::ffi::c_void) -> usize;
                }

                let sb_ptr = &sample_buffer as *const _ as *const core::ffi::c_void;
                let pb = CMSampleBufferGetImageBuffer(sb_ptr);
                if pb.is_null() {
                    static NULL_LOGGED: OnceLock<AtomicUsize> = OnceLock::new();
                    let n = NULL_LOGGED.get_or_init(|| AtomicUsize::new(0)).fetch_add(1, Ordering::Relaxed);
                    if n < 10 {
                        warn!("CMSampleBufferGetImageBuffer returned null (likely metadata-only or not-ready buffer)");
                    }
                    return;
                }

                if CVPixelBufferLockBaseAddress(pb, 1) != 0 {
                    warn!("CVPixelBufferLockBaseAddress failed");
                    return;
                }

                let base = CVPixelBufferGetBaseAddress(pb) as *const u8;
                let bytes_per_row = CVPixelBufferGetBytesPerRow(pb);
                if width == 0 || height == 0 {
                    // Fallback to pixel buffer dims
                    width = CVPixelBufferGetWidth(pb);
                    height = CVPixelBufferGetHeight(pb);
                }

                if base.is_null() || bytes_per_row == 0 || width == 0 || height == 0 {
                    let _ = CVPixelBufferUnlockBaseAddress(pb, 1);
                    warn!("Invalid pixel buffer: base={:?} bpr={} wxh={}x{}", base, bytes_per_row, width, height);
                    return;
                }

                // Log first few frames only
                static FRAME_LOGGED: OnceLock<AtomicUsize> = OnceLock::new();
                let n = FRAME_LOGGED.get_or_init(|| AtomicUsize::new(0)).fetch_add(1, Ordering::Relaxed);
                if n < 10 {
                    info!("did_output_sample_buffer: {}x{} stride={}", width, height, bytes_per_row);
                }

                // Copy out BGRA respecting stride
                let total_needed = height.saturating_mul(bytes_per_row);
                let latest_mutex = latest();
                if let Ok(mut lf) = latest_mutex.lock() {
                    if lf.bytes.len() != total_needed {
                        lf.bytes.resize(total_needed, 0u8);
                    }
                    let bytes_per_pixel = 4usize;
                    let row_visible_bytes = width.saturating_mul(bytes_per_pixel).min(bytes_per_row);
                    for row in 0..height {
                        let src = base.add(row * bytes_per_row);
                        let dst = lf.bytes.as_mut_ptr().add(row * bytes_per_row);
                        std::ptr::copy_nonoverlapping(src, dst, row_visible_bytes);
                        if row_visible_bytes < bytes_per_row {
                            std::ptr::write_bytes(dst.add(row_visible_bytes), 0, bytes_per_row - row_visible_bytes);
                        }
                    }
                    lf.width = width as u32;
                    lf.height = height as u32;
                    lf.stride = bytes_per_row as u32;
                    if n < 10 {
                        info!("Copied BGRA frame: {}x{} stride={}", lf.width, lf.height, lf.stride);
                    }
                }

                let _ = CVPixelBufferUnlockBaseAddress(pb, 1);
            }
        }
    }

    // Attach output handler for Screen frames
    let _handler_id = stream.add_output_handler(ScreenOutput, SCStreamOutputType::Screen);

    // Start stream
    stream.start_capture()
        .map_err(|e| anyhow::anyhow!("start_capture failed: {e:?}"))?;
 
    RUNNING.store(true, Ordering::SeqCst);
    // store stream to keep it alive
    STREAM_HANDLE.get_or_init(|| std::sync::Mutex::new(None))
        .lock().ok().unwrap()
        .replace(stream);
    STARTED.set(()).ok();
    info!("Screen capture stream started via screencapturekit.");
 
    Ok(())
}

// Copy latest BGRA frame into a Vec buffer.
// Returns None if no frame is currently available (e.g., permission not granted yet).
pub fn try_get_latest_bgra() -> Option<BGRAFrame> {
    let m = LATEST.get()?;
    let guard = m.lock().ok()?;
    if guard.width == 0 || guard.height == 0 || guard.stride == 0 || guard.bytes.is_empty() {
        return None;
    }
    Some(BGRAFrame {
        width: guard.width,
        height: guard.height,
        bytes_per_row: guard.stride,
        bytes: guard.bytes.clone(),
    })
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
        log::info!("Screen capture stream stopped.");
    }
}