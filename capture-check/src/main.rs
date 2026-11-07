#![cfg(target_os = "macos")]

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use env_logger::Env;
use log::{info};

use core_graphics::display::CGDirectDisplayID;
use screencapturekit::{
    output::CMSampleBuffer,
    output::sc_stream_frame_info::{SCFrameStatus, SCStreamFrameInfo},
    shareable_content::{SCDisplay, SCShareableContent},
    stream::{
        configuration::{pixel_format::PixelFormat, SCStreamConfiguration},
        content_filter::SCContentFilter,
        output_trait::SCStreamOutputTrait,
        output_type::SCStreamOutputType,
        SCStream,
    },
};

#[derive(Parser, Debug)]
#[command(author, version, about = "Validate ScreencaptureKit and record raw BGRA frames to a file")]
struct Cli {
    /// Capture duration in seconds (upper bound with max-frames)
    #[arg(long, default_value_t = 5)]
    seconds: u64,

    /// Maximum frames to record (upper bound with seconds)
    #[arg(long, default_value_t = 300)]
    max_frames: usize,

    /// Display ID to capture (0 = primary)
    #[arg(long, default_value_t = 0)]
    display: u32,

    /// Output file path (raw BGRA with small header)
    #[arg(long, default_value = "./capture.bgra")]
    out: String,

    /// Optional: encode captured frames to this MP4 path using ffmpeg (H.264 yuv420p)
    #[arg(long)]
    mp4_out: Option<String>,

    /// Target framerate for MP4 encoding (used for ffmpeg -r)
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    log: String,
}

#[derive(Clone)]
struct Frame {
    width: u32,
    height: u32,
    bytes_per_row: u32,
    data: Vec<u8>,
}

struct RecorderState {
    header_written: bool,
    frames_written: usize,
    writer: BufWriter<File>,
    width: u32,
    height: u32,
    bytes_per_row: u32,

    // ffmpeg encoding support
    ffmpeg_stdin: Option<std::process::ChildStdin>,
    ffmpeg_started: bool,
}

fn find_display(display_id: u32) -> anyhow::Result<SCDisplay> {
    // Avoid anyhow::Context on CFError (not StdError/Send/Sync); map manually.
    let content = SCShareableContent::get().map_err(|_| anyhow::anyhow!("SCShareableContent::get failed"))?;
    if content.displays().is_empty() {
        anyhow::bail!("No displays found via SCShareableContent");
    }
    // Log available displays to aid debugging
    for (idx, d) in content.displays().iter().enumerate() {
        info!("Display[{}]: id={}, width={}, height={}",
              idx,
              d.display_id(),
              d.width(),
              d.height());
    }
    if display_id == 0 {
        return Ok(content.displays()[0].clone());
    }
    let needle: CGDirectDisplayID = display_id as CGDirectDisplayID;
    content
        .displays()
        .into_iter()
        .find(|d| d.display_id() == needle)
        .ok_or_else(|| anyhow::anyhow!("Display id {} not found", display_id))
}

fn write_header(w: &mut BufWriter<File>, w_px: u32, h_px: u32, stride: u32, bpp: u32, frames: u32) -> anyhow::Result<()> {
    // 32-byte header:
    // 0..4  = magic "BGR4"
    // 4..8  = version u32 = 1
    // 8..12 = width u32
    // 12..16= height u32
    // 16..20= stride u32
    // 20..24= bpp u32 (always 4)
    // 24..28= frame_count u32 (0 if unknown up-front)
    // 28..32= reserved u32
    let mut hdr = [0u8; 32];
    hdr[0..4].copy_from_slice(b"BGR4");
    hdr[4..8].copy_from_slice(&1u32.to_le_bytes());
    hdr[8..12].copy_from_slice(&w_px.to_le_bytes());
    hdr[12..16].copy_from_slice(&h_px.to_le_bytes());
    hdr[16..20].copy_from_slice(&stride.to_le_bytes());
    hdr[20..24].copy_from_slice(&bpp.to_le_bytes());
    hdr[24..28].copy_from_slice(&frames.to_le_bytes());
    hdr[28..32].copy_from_slice(&0u32.to_le_bytes());
    w.write_all(&hdr)?;
    Ok(())
}

fn update_header_frame_count(path: &str, frames: u32) -> anyhow::Result<()> {
    use std::io::Seek;
    use std::io::SeekFrom;
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut w = std::io::BufWriter::new(f);
    // Seek to 24
    w.get_mut().seek(SeekFrom::Start(24))?;
    w.write_all(&frames.to_le_bytes())?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    env_logger::Builder::from_env(Env::default().default_filter_or(cli.log)).init();
    info!("Starting capture-check: seconds={}, max_frames={}, display={}, out={}", cli.seconds, cli.max_frames, cli.display, cli.out);

    // Open output file
    let file = File::create(&cli.out).map_err(|e| anyhow::anyhow!("Failed to create output file {}: {}", &cli.out, e))?;
    let mut recorder = RecorderState {
        header_written: false,
        frames_written: 0,
        writer: BufWriter::new(file),
        width: 0,
        height: 0,
        bytes_per_row: 0,
        ffmpeg_stdin: None,
        ffmpeg_started: false,
    };

    // Setup ScreencaptureKit
    let display = find_display(cli.display)?;
    // Build configuration with minimal, safe options (no CFString/CF color space)
    let mut cfg = SCStreamConfiguration::new();
    cfg = cfg.set_pixel_format(PixelFormat::BGRA).map_err(|_| anyhow::anyhow!("set_pixel_format failed"))?;
    cfg = cfg.set_captures_audio(false).map_err(|_| anyhow::anyhow!("set_captures_audio failed"))?;
    cfg = cfg.set_shows_cursor(true).map_err(|_| anyhow::anyhow!("set_shows_cursor failed"))?;
    // Explicitly set source rect to the full display to avoid 0x0 captures on some setups

    // Build content filter and stream
    // Build content filter and stream
    // Use only the primary output type to minimize API surface while stabilizing
    let filter = SCContentFilter::new().with_display_excluding_windows(&display, &[]);
    let mut stream = SCStream::new(&filter, &cfg);
    // Redundant: Add a second output type if the crate delivers frames via .Video on some OS versions
    // We reuse the same handler instance for both types

    let running = Arc::new(AtomicBool::new(true));
    let frames: Arc<Mutex<std::collections::VecDeque<Frame>>> = Arc::new(Mutex::new(std::collections::VecDeque::new()));

    // Register output handler BEFORE starting capture
    struct OutputHandler {
        frames: Arc<Mutex<std::collections::VecDeque<Frame>>>,
    }
    impl SCStreamOutputTrait for OutputHandler {
        fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, _of_type: SCStreamOutputType) {
            // Safety: Call CM/CV functions via FFI
            unsafe {
                #[allow(improper_ctypes)]
                extern "C" {
                    fn CMSampleBufferDataIsReady(sb: *const core::ffi::c_void) -> bool;
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
                    return;
                }

                // Parse frame info and gate on Complete
                let info = match SCStreamFrameInfo::from_sample_buffer(&sample_buffer) {
                    Ok(i) => i,
                    Err(e) => {
                        log::debug!("SCStreamFrameInfo::from_sample_buffer error: {:?}", e);
                        return;
                    }
                };
                let status = info.status();
                let rect_opt = info.content_rect();
                if let Some(r) = rect_opt {
                    log::debug!("Frame status={:?} content_rect=({}, {}) {}x{}", status, r.origin.x, r.origin.y, r.size.width, r.size.height);
                } else {
                    log::debug!("Frame status={:?} content_rect=None", status);
                }
                // Gate: accept only Complete frames to avoid undefined buffer sizes early
                if status != SCFrameStatus::Complete {
                    return;
                }

                // Read CVPixelBuffer
                let pb = CMSampleBufferGetImageBuffer(sb_ptr);
                if pb.is_null() {
                    log::debug!("CMSampleBufferGetImageBuffer returned null");
                    return;
                }
                if CVPixelBufferLockBaseAddress(pb, 1) != 0 {
                    log::debug!("CVPixelBufferLockBaseAddress failed");
                    return;
                }
                let base = CVPixelBufferGetBaseAddress(pb) as *const u8;
                let bpr = CVPixelBufferGetBytesPerRow(pb);
                let mut w = info.content_rect().map(|r| r.size.width.max(0.0) as usize).unwrap_or(0);
                let mut h = info.content_rect().map(|r| r.size.height.max(0.0) as usize).unwrap_or(0);
                if w == 0 || h == 0 {
                    w = CVPixelBufferGetWidth(pb);
                    h = CVPixelBufferGetHeight(pb);
                }
                if base.is_null() || bpr == 0 || w == 0 || h == 0 {
                    log::debug!("Invalid pixel buffer params: base_null={} bpr={} w={} h={}", base.is_null(), bpr, w, h);
                    let _ = CVPixelBufferUnlockBaseAddress(pb, 1);
                    return;
                }

                // Copy with stride
                let mut buf = vec![0u8; h.saturating_mul(bpr)];
                let bytes_per_pixel = 4usize;
                let row_visible = w.saturating_mul(bytes_per_pixel).min(bpr);
                for row in 0..h {
                    let src = base.add(row * bpr);
                    let dst = buf.as_mut_ptr().add(row * bpr);
                    std::ptr::copy_nonoverlapping(src, dst, row_visible);
                    if row_visible < bpr {
                        std::ptr::write_bytes(dst.add(row_visible), 0, bpr - row_visible);
                    }
                }

                let _ = CVPixelBufferUnlockBaseAddress(pb, 1);

                let frame = Frame {
                    width: w as u32,
                    height: h as u32,
                    bytes_per_row: bpr as u32,
                    data: buf,
                };

                if let Ok(mut q) = self.frames.lock() {
                    q.push_back(frame);
                }
            }
        }
    }

    let handler = OutputHandler { frames: frames.clone() };
    let _handler_id = stream.add_output_handler(handler, SCStreamOutputType::Screen);
    // Start capture
    stream
        .start_capture()
        .map_err(|e| anyhow::anyhow!("start_capture failed: {e:?}"))?;
    // Increase delay after start to allow first buffers and privacy acceptance
    std::thread::sleep(Duration::from_millis(150));
    info!("Stream started; waiting for frames...");

    let start = Instant::now();
    let target_end = start + Duration::from_secs(cli.seconds);
    let mut last_log = Instant::now();

    // Actively wait up to 6s for the first frame to arrive (privacy prompt/warm-up)
    // If you see 0 frames, ensure the app is granted Screen Recording permission in:
    // System Settings > Privacy & Security > Screen Recording
    let mut primed = false;
    let warmup_deadline = Instant::now() + Duration::from_millis(6000);
    while Instant::now() < warmup_deadline && !primed {
        {
            let q = frames.lock().unwrap();
            primed = !q.is_empty();
        }
        if !primed {
            std::thread::sleep(Duration::from_millis(33));
        }
    }

    while running.load(std::sync::atomic::Ordering::Relaxed) {
        // Pull frames from queue (FIFO)
        let maybe_frame = {
            let mut q = frames.lock().unwrap();
            q.pop_front()
        };
        if let Some(f) = maybe_frame {
            if !recorder.header_written {
                // Write header on first frame
                write_header(&mut recorder.writer, f.width, f.height, f.bytes_per_row, 4, 0)?;
                recorder.width = f.width;
                recorder.height = f.height;
                recorder.bytes_per_row = f.bytes_per_row;
                recorder.header_written = true;
                info!("Header written: {}x{} stride={}", f.width, f.height, f.bytes_per_row);

                // If MP4 is requested, spawn ffmpeg and prepare to pipe frames
                if let Some(ref mp4_path) = cli.mp4_out {
                    // Build ffmpeg command:
                    // ffmpeg -y -f rawvideo -pix_fmt bgra -s WxH -r FPS -i - -an -c:v libx264 -pix_fmt yuv420p -movflags +faststart out.mp4
                    let size_arg = format!("{}x{}", f.width, f.height);
                    let fps_arg = cli.fps.to_string();
                    info!("Spawning ffmpeg to encode MP4: {}", mp4_path);
                    info!("Equivalent command: ffmpeg -y -f rawvideo -pix_fmt bgra -s {} -r {} -i - -an -c:v libx264 -pix_fmt yuv420p -movflags +faststart {}", size_arg, fps_arg, mp4_path);

                    let mut child = std::process::Command::new("ffmpeg")
                        .arg("-y")
                        .arg("-f").arg("rawvideo")
                        .arg("-pix_fmt").arg("bgra")
                        .arg("-s").arg(&size_arg)
                        .arg("-r").arg(&fps_arg)
                        .arg("-i").arg("-")
                        .arg("-an")
                        .arg("-c:v").arg("libx264")
                        .arg("-pix_fmt").arg("yuv420p")
                        .arg("-movflags").arg("+faststart")
                        .arg(mp4_path)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::inherit())
                        .stderr(std::process::Stdio::inherit())
                        .spawn()
                        .map_err(|e| anyhow::anyhow!("Failed to spawn ffmpeg: {}", e))?;
                    recorder.ffmpeg_stdin = child.stdin.take();
                    recorder.ffmpeg_started = true;
                }
            }
            // Write raw frame data
            recorder.writer.write_all(&f.data)?;
            // Also pipe to ffmpeg if enabled
            if recorder.ffmpeg_started {
                if let Some(stdin) = recorder.ffmpeg_stdin.as_mut() {
                    use std::io::Write as _;
                    if let Err(e) = stdin.write_all(&f.data) {
                        log::warn!("ffmpeg stdin write failed: {}", e);
                    }
                }
            }
            recorder.frames_written += 1;

            if recorder.frames_written % 30 == 0 || last_log.elapsed() >= Duration::from_secs(1) {
                info!("Frames written: {}", recorder.frames_written);
                last_log = Instant::now();
            }

            if recorder.frames_written >= cli.max_frames {
                info!("Reached max-frames={}", cli.max_frames);
                break;
            }
        } else {
            // No frame yet; simple sleep to avoid busy spin and allow privacy prompt interaction
            std::thread::sleep(Duration::from_millis(16));
        }

        if Instant::now() >= target_end {
            info!("Reached time budget of {}s", cli.seconds);
            break;
        }
    }

    // Flush and update header's frame_count
    recorder.writer.flush()?;
    if recorder.header_written {
        update_header_frame_count(&cli.out, recorder.frames_written as u32).ok();
    } else {
        info!("No frames captured. If permissions are granted, try moving the mouse or toggling a window on the target display to generate changes.");
    }

    // Stop stream by dropping it
    drop(stream);
    running.store(false, Ordering::SeqCst);

    // Close ffmpeg stdin to finalize the MP4
    if recorder.ffmpeg_started {
        if let Some(mut stdin) = recorder.ffmpeg_stdin.take() {
            let _ = stdin.flush();
            drop(stdin);
        }
        // Give ffmpeg a moment to finish writing the MP4
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    info!(
        "Done. Wrote {} frames to {}. Inspect with:\n\
         ffplay -f rawvideo -pixel_format bgra -video_size {}x{} -framerate 60 -i {}",
        recorder.frames_written,
        &cli.out,
        recorder.width.max(1),
        recorder.height.max(1),
        &cli.out
    );

    Ok(())
}