//! The GPU side of the window: device, surface, the scene, the HUD's text renderer, and
//! putting captured frames on the virtual screen.

use ab_glyph::FontArc;
use log::{info, warn};
use wgpu_glyph::{GlyphBrush, GlyphBrushBuilder};
use winit::window::Window;

use crate::cli::Cli;
use crate::hud::{self, LabelMetrics, Rect};
use crate::scene::{self, Scene, ScreenPlacement, Vertex};
#[cfg(target_os = "macos")]
use crate::iosurface;
#[cfg(target_os = "macos")]
use crate::macos_capture::CapturedFrame;

// ================= GPU =================

pub struct Gpu {
    pub surface: wgpu::Surface,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,

    // Grid and virtual screen
    pub scene: Scene,

    // HUD
    pub glyph_brush: Option<GlyphBrush<()>>,
    pub staging_belt: Option<wgpu::util::StagingBelt>,
    pub hud_enabled: bool,
    pub hud_font_size: f32,
    // The HUD font, and its button label sizes at the last font size used (measured again
    // only when that changes, for example on a move to a display with another scale).
    pub hud_font: Option<FontArc>,
    pub hud_metrics: Option<LabelMetrics>,
    pub calibrate_btn: Rect,
    // "Screen Recording settings" button; only shown while capture has no permission or failed.
    pub settings_btn: Option<Rect>,
    pub show_calibrated_toast_until_ns: u64,

    // HUD caching
    pub hud_text: Vec<String>,
    pub hud_text_updated_ns: u64,

    // Captured frames: the last one taken, whether the GPU reads them in place (else they're
    // copied), and the newest few shown that way. Each holds its IOSurface, so the capture
    // can't write into it while the GPU may still be drawing it: the CPU runs at most a
    // couple of frames ahead of the GPU (Metal hands out three drawables), so keeping the
    // current frame and the two before it is enough.
    pub frame_seq: [u64; 2],
    pub zero_copy: bool,
    // Whether the GPU can read frames in place at all (Metal, and no import has failed).
    pub zero_copy_available: bool,
    #[cfg(target_os = "macos")]
    pub shown_frames: [[Option<CapturedFrame>; 3]; 2],
}

/// wgpu 0.17's surface configuration drops pending map callbacks instead of firing them.
/// Drain them first (the HUD's staging belt maps buffers after each submit). This waits
/// only on resize/surface recovery, never on the normal frame path.
pub fn configure_surface(surface: &wgpu::Surface, device: &wgpu::Device, config: &wgpu::SurfaceConfiguration) {
    device.poll(wgpu::Maintain::Wait);
    surface.configure(device, config);
}

pub async fn init_gpu(window: &Window, cli: &Cli, vertices: &[Vertex]) -> anyhow::Result<Gpu> {
    let (vsync, hud_forced_off, hud_size) = (cli.vsync, cli.no_hud, cli.hud_size);
    let screen = ScreenPlacement::from_cli(cli);
    let size = window.inner_size();

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..Default::default()
    });

    // wgpu 0.17: Surface has no lifetime parameter and Instance::create_surface requires raw handles
    let surface = unsafe { instance.create_surface(window)? };

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        })
        .await
        .ok_or_else(|| anyhow::anyhow!("No suitable GPU adapter found"))?;

    let (device, queue) = adapter
        .request_device(
            &wgpu::DeviceDescriptor {
                label: Some("device"),
                // Enable downlevel-friendly features only; avoid costly extras.
                features: wgpu::Features::empty(),
                // Use the adapter's real limits: the screen texture must fit a full-resolution
                // capture (e.g. 3024 px wide on a Retina display), beyond WebGL2's 2048 px.
                limits: adapter.limits(),
            },
            None,
        )
        .await?;

    let caps = surface.get_capabilities(&adapter);
    // Prefer a non-sRGB render format to skip implicit conversions on Metal if available,
    // otherwise fall back to the first supported format.
    let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
    // Present mode selection: for max FPS choose Mailbox > Immediate > Fifo (vsync).
    let present_mode = if vsync {
        wgpu::PresentMode::Fifo
    } else if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
        wgpu::PresentMode::Immediate
    } else {
        // Fallback to Fifo when device doesn't expose low-latency modes
        wgpu::PresentMode::Fifo
    };

    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
    };
    configure_surface(&surface, &device, &config);

    let mut scene = Scene::new(&device, format, vertices, screen);
    scene.curve = cli.curve;

    // HUD: Noto Sans (SIL OFL 1.1, see assets/OFL.txt) is embedded in the binary; if it fails to parse, disable HUD text.
    let font_arc: Option<FontArc> = match FontArc::try_from_slice(hud::FONT_BYTES) {
        Ok(f) => Some(f),
        Err(_) => {
            warn!("Embedded font invalid; HUD text will be disabled.");
            None
        }
    };
    // Setup HUD resources optionally
    let hud_requested = !hud_forced_off;
    let hud_font = font_arc.clone().filter(|_| hud_requested);
    let (glyph_brush, staging_belt, hud_enabled) = if hud_requested {
        if let Some(font) = font_arc {
            let gb = GlyphBrushBuilder::using_font(font).build(&device, format);
            let sb = wgpu::util::StagingBelt::new(1024);
            (Some(gb), Some(sb), true)
        } else {
            (None, None, false)
        }
    } else {
        (None, None, false)
    };

    #[cfg(target_os = "macos")]
    let zero_copy_available = iosurface::device_supports_import(&device);
    #[cfg(not(target_os = "macos"))]
    let zero_copy_available = false;
    let zero_copy = zero_copy_available && !cli.copy_frames;
    info!("Captured frames: {}", if zero_copy { "read in place by the GPU" } else { "copied through the CPU" });

    let hud_font_size = hud_size.max(8.0);
    // Placed on the first frame, once the window size is known.
    let calibrate_btn = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
    let show_calibrated_toast_until_ns = 0;

    Ok(Gpu {
        surface,
        device,
        queue,
        config,
        scene,
        glyph_brush,
        staging_belt,
        hud_enabled,
        hud_font_size,
        hud_font,
        hud_metrics: None,
        calibrate_btn,
        settings_btn: None,
        show_calibrated_toast_until_ns,
        hud_text: Vec::new(),
        hud_text_updated_ns: 0,
        frame_seq: [0; 2],
        zero_copy,
        zero_copy_available,
        #[cfg(target_os = "macos")]
        shown_frames: [[None, None, None], [None, None, None]],
    })
}

impl Gpu {
    /// Toggle the second screen only at a menu change, not in the render loop.
    pub fn set_secondary(&mut self, enabled: bool) {
        if enabled == self.scene.secondary_enabled() { return; }
        if !enabled { self.device.poll(wgpu::Maintain::Wait); }
        self.scene.set_secondary(&self.device, &self.queue, enabled);
        self.frame_seq[1] = 0;
        #[cfg(target_os = "macos")]
        { self.shown_frames[1] = [None, None, None]; }
    }
}

/// Put a captured frame on one virtual screen: in place from its IOSurface when possible,
/// else by copying its pixels. If sharing fails once, frames are copied from then on.
#[cfg(target_os = "macos")]
pub fn show_captured_frame(gpu: &mut Gpu, index: usize, frame: CapturedFrame) {
    let Gpu { device, queue, scene, zero_copy, zero_copy_available, shown_frames, .. } = gpu;
    assert!(index < 2 && (index == 0 || scene.secondary_enabled()), "screen index is not enabled");
    let shown_frames = &mut shown_frames[index];
    if *zero_copy {
        if let Some(surface) = frame.iosurface() {
            // SAFETY: `surface` belongs to `frame`, which is alive here and, once shown, kept
            // in `shown_frames` while the GPU may draw it.
            let key = u64::from(unsafe { iosurface::id(surface) });
            let shown = scene.show_external_at(index, device, queue, key, frame.width, frame.height, || unsafe {
                iosurface::import(device, surface, frame.width, frame.height)
            });
            if shown != scene::FrameUpload::NotImported {
                shown_frames.rotate_right(1);
                shown_frames[0] = Some(frame);
                return;
            }
        }
        warn!("Can't let the GPU read captured frames in place; copying them instead");
        *zero_copy = false;
        *zero_copy_available = false;
    }
    // Copied frames needn't be kept (`write_texture` takes its own copy), but frames shown in
    // place before may still be in the GPU's queue: let them go one frame at a time.
    shown_frames.rotate_right(1);
    shown_frames[0] = None;
    frame.with_bytes(|bytes| scene.upload_frame_at(index, device, queue, frame.width, frame.height, frame.bytes_per_row, bytes));
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::configure_surface;
    use metal::foreign_types::ForeignType;
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

    #[test]
    fn surface_reconfiguration_fires_pending_map_callbacks() {
        let layer = metal::MetalLayer::new();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        // SAFETY: the CAMetalLayer is valid and stays alive longer than the surface.
        // No drawable is acquired, so this needs neither a window nor a connected display.
        let surface = unsafe { instance.create_surface_from_core_animation_layer(layer.as_ptr().cast()) };
        let Some(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        })) else {
            eprintln!("no Metal adapter; skipping surface reconfiguration test");
            return;
        };
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: None,
                features: wgpu::Features::empty(),
                limits: adapter.limits(),
            },
            None,
        )).expect("test device");
        let caps = surface.get_capabilities(&adapter);
        let mut config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: caps.formats[0],
            width: 64,
            height: 64,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        configure_surface(&surface, &device, &config);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pending HUD-style mapping"),
            size: 256,
            usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        queue.submit(std::iter::empty());
        let called = Arc::new(AtomicBool::new(false));
        let callback_called = called.clone();
        buffer.slice(..).map_async(wgpu::MapMode::Write, move |result| {
            result.expect("map buffer before reconfiguration");
            callback_called.store(true, Ordering::SeqCst);
        });
        config.width = 128;
        configure_surface(&surface, &device, &config);
        assert!(called.load(Ordering::SeqCst), "map callback must fire before configure returns");
        buffer.unmap();
    }
}
