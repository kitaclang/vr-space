use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use env_logger::Env;
use log::{info, warn};

use glam::{Mat4, Quat, Vec3};
use wgpu::util::DeviceExt;
use winit::event::{ElementState, Event, KeyboardInput, MouseButton, VirtualKeyCode, WindowEvent};
use winit::event::StartCause;
use winit::event_loop::{EventLoop, EventLoopBuilder};
use winit::window::Window;
#[cfg(not(target_os = "macos"))]
use winit::window::Fullscreen;

// Text overlay
use ab_glyph::FontArc;
use wgpu_glyph::{GlyphBrush, GlyphBrushBuilder, HorizontalAlign, Layout, Section, Text, VerticalAlign};

// On macOS, pull capture helpers into scope (now using screencapturekit crate under the hood)
#[cfg(target_os = "macos")]
use crate::macos_capture::{
    capture_status, list_displays, open_screen_recording_settings, resolve_display_id, start_stream, stop_stream,
    switch_display, take_latest_bgra, CaptureStatus,
};
#[cfg(target_os = "macos")]
use crate::menu_bar::MenuBar;

// Headset fusion API (path dependency to headset-utils)
use ar_drivers::{any_fusion, any_glasses};

// macOS screen capture (virtual monitor)
#[cfg(target_os = "macos")]
mod macos_capture;

// Menu bar icon and shortcuts that work from any app
#[cfg(target_os = "macos")]
mod menu_bar;

/// Something the user asked for, from a key in the window, the menu bar or a global shortcut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Recenter,
    Further,
    Closer,
    Bigger,
    Smaller,
    ResetScreen,
    ToggleFullscreen,
    /// Go fullscreen on this display, or back to a window with `None`.
    FullscreenOn(Option<u32>),
    /// Capture this display instead.
    ShowDisplay(u32),
    ToggleHud,
    OpenScreenSettings,
    Quit,
}

// ================= CLI =================

#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Headset pose visualizer (wgpu + winit, Metal on macOS)")]
struct Cli {
    /// Enable vsync (off by default for lowest latency)
    #[arg(long, default_value_t = false)]
    vsync: bool,

    /// Invert X (roll)
    #[arg(long, default_value_t = false)]
    invert_x: bool,

    /// Invert Y (pitch)
    #[arg(long, default_value_t = false)]
    invert_y: bool,

    /// Invert Z (yaw)
    #[arg(long, default_value_t = false)]
    invert_z: bool,

    /// Poll interval for headset fusion update in milliseconds (0 = as fast as possible)
    #[arg(long, default_value_t = 0)]
    poll_ms: u64,

    /// Disable HUD text to save GPU/CPU
    #[arg(long, default_value_t = false)]
    no_hud: bool,

    /// HUD font size in logical pixels
    #[arg(long, default_value_t = 16.0)]
    hud_size: f32,

    /// When vsync is off, target max FPS by pacing the CPU (0 = unlimited)
    #[arg(long, default_value_t = 0)]
    max_fps: u32,

    /// Display to show on the virtual screen, by ID from --list-displays (0 = main display)
    #[arg(long, default_value_t = 0)]
    display: u32,

    /// List the displays that can be captured, then exit
    #[arg(long, default_value_t = false)]
    list_displays: bool,

    /// Height of the virtual screen in scene units (one grid square = 1 unit)
    #[arg(long, default_value_t = 1.4)]
    screen_height: f32,

    /// Distance from your eyes to the virtual screen in scene units
    #[arg(long, default_value_t = 4.0)]
    screen_distance: f32,

    /// Vertical field of view in degrees
    #[arg(long, default_value_t = 60.0)]
    fov: f32,

    /// Open fullscreen on this display, by ID from --list-displays (use the glasses' display).
    /// 0 = main display. Press F in the app to toggle fullscreen.
    #[arg(long, value_name = "ID")]
    fullscreen: Option<u32>,

    /// Neck model strength: moves the eyes as if the head turns around the neck
    /// (1 = average head, 0 = off)
    #[arg(long, default_value_t = 1.0)]
    neck_model: f32,

    /// Hide the mouse cursor on the virtual screen
    #[arg(long)]
    no_cursor: bool,

    /// Recenter only the left-right direction (yaw), keeping the horizon level even if
    /// your head is tilted when you recenter
    #[arg(long)]
    recenter_yaw_only: bool,
}

impl Cli {
    fn validate(&self) -> anyhow::Result<()> {
        if !(self.screen_height > 0.0 && self.screen_height.is_finite()) {
            anyhow::bail!("--screen-height must be greater than 0");
        }
        if !(self.screen_distance > 0.0 && self.screen_distance.is_finite()) {
            anyhow::bail!("--screen-distance must be greater than 0");
        }
        if !(self.fov > 1.0 && self.fov < 179.0) {
            anyhow::bail!("--fov must be between 1 and 179 degrees");
        }
        if !(0.0..=3.0).contains(&self.neck_model) {
            anyhow::bail!("--neck-model must be between 0 and 3");
        }
        Ok(())
    }
}

// Camera (eye) position in the scene when looking straight ahead. Scene units are roughly
// metres: the grid floor is at y = 0 and one grid square is 1 unit.
const CAM_POS: Vec3 = Vec3::new(0.0, 1.5, 4.0);

// Neck model: the eyes sit about 7.5 cm above and 8 cm in front of the point the head turns
// around, so turning or tilting the head also moves the eyes a little. Recreating that adds
// natural parallax, even though 3DoF glasses can't track position.
const NECK_TO_EYES: Vec3 = Vec3::new(0.0, 0.075, -0.08);

// ================= Low-latency Pose (Atomics) =================

#[inline]
fn f32_to_u32(x: f32) -> u32 { x.to_bits() }
#[inline]
fn u32_to_f32(x: u32) -> f32 { f32::from_bits(x) }

#[derive(Debug)]
struct PoseAtomic {
    roll_bits: AtomicU32,
    pitch_bits: AtomicU32,
    yaw_bits: AtomicU32,
    has_device: AtomicU32,     // 0/1
    sample_time_ns: AtomicU64, // monotonic timestamp of last sample (Instant::now() - START), as nanoseconds
}

impl PoseAtomic {
    fn new() -> Self {
        Self {
            roll_bits: AtomicU32::new(f32_to_u32(0.0)),
            pitch_bits: AtomicU32::new(f32_to_u32(0.0)),
            yaw_bits: AtomicU32::new(f32_to_u32(0.0)),
            has_device: AtomicU32::new(0),
            sample_time_ns: AtomicU64::new(0),
        }
    }
    fn write(&self, roll: f32, pitch: f32, yaw: f32, has: bool, now_ns: u64) {
        self.roll_bits.store(f32_to_u32(roll), Ordering::Relaxed);
        self.pitch_bits.store(f32_to_u32(pitch), Ordering::Relaxed);
        self.yaw_bits.store(f32_to_u32(yaw), Ordering::Relaxed);
        self.has_device.store(if has { 1 } else { 0 }, Ordering::Relaxed);
        self.sample_time_ns.store(now_ns, Ordering::Relaxed);
    }
    fn read(&self) -> (f32, f32, f32, bool, u64) {
        let r = u32_to_f32(self.roll_bits.load(Ordering::Relaxed));
        let p = u32_to_f32(self.pitch_bits.load(Ordering::Relaxed));
        let y = u32_to_f32(self.yaw_bits.load(Ordering::Relaxed));
        let has = self.has_device.load(Ordering::Relaxed) != 0;
        let t = self.sample_time_ns.load(Ordering::Relaxed);
        (r, p, y, has, t)
    }
}

// Monotonic app start Instant to derive cheap ns timestamps. Set at the top of main();
// get_or_init makes any earlier call safe too.
static START_INSTANT: OnceLock<Instant> = OnceLock::new();
#[inline]
fn now_ns_from_start() -> u64 {
    let start = *START_INSTANT.get_or_init(Instant::now);
    (Instant::now() - start).as_nanos() as u64
}

// How often the HUD's text is re-formatted (it is still drawn every frame).
const HUD_REFRESH_NS: u64 = 100_000_000;

// Head orientation sample, as published by the tracking thread.
#[derive(Debug, Clone, Copy)]
struct PoseCopy {
    roll: f32,
    pitch: f32,
    yaw: f32,
}

// ================= GPU Types =================

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 3],
    color: [f32; 3],
}

impl Vertex {
    const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];

    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
}

// Textured quad vertex
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TVertex {
    pos: [f32; 3],
    uv: [f32; 2],
}
impl TVertex {
    const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2];
    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

impl Uniforms {
    fn new() -> Self {
        Self { view_proj: Mat4::IDENTITY.to_cols_array_2d() }
    }
    fn update(&mut self, vp: Mat4) {
        self.view_proj = vp.to_cols_array_2d();
    }
}

// Simple rectangle hitbox
#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}
impl Rect {
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
}

struct Gpu {
    surface: wgpu::Surface,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    // Grid
    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    vertex_count: u32,

    // Camera uniforms
    uniform: Uniforms,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,

    // Optional pre-recorded render bundle for static scene
    render_bundle: Option<wgpu::RenderBundle>,

    // HUD
    glyph_brush: Option<GlyphBrush<()>>,
    staging_belt: Option<wgpu::util::StagingBelt>,
    hud_enabled: bool,
    hud_font_size: f32,
    hud_padding: f32,
    calibrate_btn: Rect,
    // "Screen Recording settings" button; only shown while capture has no permission or failed.
    settings_btn: Option<Rect>,
    show_calibrated_toast_until_ns: u64,

    // HUD caching
    hud_text: Vec<String>,
    hud_text_updated_ns: u64,

    // Virtual monitor: textured quad and texture/sampler
    quad_pipeline: wgpu::RenderPipeline,
    quad_vb: wgpu::Buffer,
    quad_ib: wgpu::Buffer,
    quad_index_count: u32,
    quad_bgl: wgpu::BindGroupLayout,
    quad_sampler: wgpu::Sampler,
    // Screen texture; recreated (rarely) when the captured frame size changes.
    tex: wgpu::Texture,
    tex_size: (u32, u32),
    quad_bind_group: wgpu::BindGroup,
    // CPU-side frame buffer swapped with the capture module (no per-frame copy or allocation).
    frame_buf: Vec<u8>,
    frame_seq: u64,
    screen: ScreenPlacement,
}

/// Where the virtual screen sits: straight ahead of the eye at the calibrated "forward".
/// Its width follows the captured screen's aspect ratio.
#[derive(Clone, Copy, Debug)]
struct ScreenPlacement {
    height: f32,
    distance: f32,
}

/// Each press of a distance or size key changes it by this factor, so steps feel even
/// whether the screen is near or far.
const SCREEN_STEP: f32 = 1.1;
const SCREEN_DISTANCE_RANGE: (f32, f32) = (0.5, 50.0);
const SCREEN_HEIGHT_RANGE: (f32, f32) = (0.1, 30.0);

impl ScreenPlacement {
    fn from_cli(cli: &Cli) -> Self {
        Self { height: cli.screen_height, distance: cli.screen_distance }
    }

    /// Scale the distance (`factor` > 1 moves the screen away). The size stays the same,
    /// so a closer screen looks bigger, as a real one would.
    fn scale_distance(&mut self, factor: f32) {
        let (lo, hi) = SCREEN_DISTANCE_RANGE;
        self.distance = (self.distance * factor).clamp(lo.min(self.distance), hi.max(self.distance));
    }

    fn scale_height(&mut self, factor: f32) {
        let (lo, hi) = SCREEN_HEIGHT_RANGE;
        self.height = (self.height * factor).clamp(lo.min(self.height), hi.max(self.height));
    }
}

fn quad_vertices(screen: ScreenPlacement, aspect: f32) -> [TVertex; 4] {
    let hh = screen.height * 0.5;
    let hw = hh * aspect;
    let c = CAM_POS - Vec3::Z * screen.distance;
    [
        TVertex { pos: [c.x - hw, c.y + hh, c.z], uv: [0.0, 0.0] },
        TVertex { pos: [c.x + hw, c.y + hh, c.z], uv: [1.0, 0.0] },
        TVertex { pos: [c.x + hw, c.y - hh, c.z], uv: [1.0, 1.0] },
        TVertex { pos: [c.x - hw, c.y - hh, c.z], uv: [0.0, 1.0] },
    ]
}

/// Rewrite the screen quad after its placement or the captured frame's aspect ratio changed.
fn write_quad(gpu: &Gpu) {
    let (w, h) = gpu.tex_size;
    let aspect = if w > 0 && h > 0 { w as f32 / h as f32 } else { 1.6 };
    let verts = quad_vertices(gpu.screen, aspect);
    gpu.queue.write_buffer(&gpu.quad_vb, 0, bytemuck::cast_slice(&verts));
}

/// Create the BGRA screen texture and its bind group. BGRA8Unorm matches ScreenCaptureKit
/// output, so no swizzle is needed.
fn create_screen_texture(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::BindGroup) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("capture-texture"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("quad-bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
        ],
    });
    (tex, bind_group)
}

async fn init_gpu(window: &Window, cli: &Cli, vertices: &[Vertex]) -> anyhow::Result<Gpu> {
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
    surface.configure(&device, &config);

    // Shaders
    let shader_src = include_str!("shader.wgsl");
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("grid-shader"),
        source: wgpu::ShaderSource::Wgsl(shader_src.into()),
    });

    // Uniforms
    let uniform = Uniforms::new();
    let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("uniform-buffer"),
        contents: bytemuck::bytes_of(&uniform),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });

    let uniform_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("uniform-bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });

    let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("uniform-bg"),
        layout: &uniform_bind_group_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: uniform_buffer.as_entire_binding(),
        }],
    });

    // Pipeline
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pipeline-layout"),
        bind_group_layouts: &[&uniform_bind_group_layout],
        push_constant_ranges: &[],
    });

    let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("grid-vertices"),
        contents: bytemuck::cast_slice(vertices),
        usage: wgpu::BufferUsages::VERTEX,
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("grid-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            buffers: &[Vertex::desc()],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::LineList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState { count: 1, mask: !0, alpha_to_coverage_enabled: false },
        multiview: None,
    });

    // Textured quad pipeline (uses same uniform_bgl)
    let quad_shader_src = r#"
struct Uniforms { view_proj: mat4x4<f32>, };
@group(0) @binding(0) var<uniform> ubo: Uniforms;

@group(1) @binding(0) var samp: sampler;
@group(1) @binding(1) var tex0: texture_2d<f32>;

struct VsIn { @location(0) pos: vec3<f32>, @location(1) uv: vec2<f32>, };
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, };

@vertex
fn vs_main_q(in: VsIn) -> VsOut {
 var o: VsOut;
 o.pos = ubo.view_proj * vec4<f32>(in.pos, 1.0);
 o.uv = in.uv; // v = 0 is the top row, matching quad_vertices
 return o;
}

@fragment
fn fs_main_q(in: VsOut) -> @location(0) vec4<f32> {
 let c = textureSample(tex0, samp, in.uv);
 return vec4<f32>(c.rgb, 1.0);
}"#;
    let quad_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("quad-shader"),
        source: wgpu::ShaderSource::Wgsl(quad_shader_src.into()),
    });
    let quad_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("quad-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });
    let quad_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("quad-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                },
                count: None,
            },
        ],
    });
    let quad_pl_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("quad-pl"),
        bind_group_layouts: &[&uniform_bind_group_layout, &quad_bgl],
        push_constant_ranges: &[],
    });
    let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("quad-pipeline"),
        layout: Some(&quad_pl_layout),
        vertex: wgpu::VertexState { module: &quad_shader, entry_point: "vs_main_q", buffers: &[TVertex::desc()] },
        fragment: Some(wgpu::FragmentState {
            module: &quad_shader,
            entry_point: "fs_main_q",
            targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
        }),
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState { count: 1, mask: !0, alpha_to_coverage_enabled: false },
        multiview: None,
    });

    // Placeholder 1x1 (black) screen texture until the first captured frame sets the real size.
    let tex_size = (1u32, 1u32);
    let (tex, quad_bind_group) = create_screen_texture(&device, &quad_bgl, &quad_sampler, tex_size.0, tex_size.1);

    // Quad VB/IB centered at origin. Starts at 16:10 and is resized to the capture's aspect ratio.
    let quad_indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
    let quad_vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("quad-vb"),
        contents: bytemuck::cast_slice(&quad_vertices(screen, 1.6)),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
    });
    let quad_ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("quad-ib"),
        contents: bytemuck::cast_slice(&quad_indices),
        usage: wgpu::BufferUsages::INDEX,
    });
    let quad_index_count = quad_indices.len() as u32;

    // HUD: Noto Sans (SIL OFL 1.1, see assets/OFL.txt) is embedded in the binary; if it fails to parse, disable HUD text.
    static EMBED_FONT: &[u8] = include_bytes!("../assets/NotoSans-Regular.ttf");
    let font_arc: Option<FontArc> = match FontArc::try_from_slice(EMBED_FONT) {
        Ok(f) => Some(f),
        Err(_) => {
            warn!("Embedded font invalid; HUD text will be disabled.");
            None
        }
    };
    // Setup HUD resources optionally
    let hud_requested = !hud_forced_off;
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

    let hud_font_size = hud_size.max(8.0);
    let hud_padding = 10.0;
    let calibrate_btn = Rect { x: 0.0, y: 0.0, w: 110.0, h: 28.0 };
    let show_calibrated_toast_until_ns = 0;

    // Record a render bundle for the static grid to reduce per-frame encoder work
    let render_bundle: Option<wgpu::RenderBundle> = {
        let mut encoder = device.create_render_bundle_encoder(&wgpu::RenderBundleEncoderDescriptor {
            label: Some("grid-bundle-encoder"),
            color_formats: &[Some(format)],
            depth_stencil: None,
            sample_count: 1,
            multiview: None,
        });
        encoder.set_pipeline(&pipeline);
        encoder.set_bind_group(0, &uniform_bind_group, &[]);
        encoder.set_vertex_buffer(0, vertex_buffer.slice(..));
        encoder.draw(0..vertices.len() as u32, 0..1);
        Some(encoder.finish(&wgpu::RenderBundleDescriptor {
            label: Some("grid-bundle"),
        }))
    };

    Ok(Gpu {
        surface,
        device,
        queue,
        config,
        pipeline,
        vertex_buffer,
        vertex_count: vertices.len() as u32,
        uniform,
        uniform_buffer,
        uniform_bind_group,
        render_bundle,
        glyph_brush,
        staging_belt,
        hud_enabled,
        hud_font_size,
        hud_padding,
        calibrate_btn,
        settings_btn: None,
        show_calibrated_toast_until_ns,
        hud_text: Vec::new(),
        hud_text_updated_ns: 0,
        quad_pipeline,
        quad_vb,
        quad_ib,
        quad_index_count,
        quad_bgl,
        quad_sampler,
        tex,
        tex_size,
        quad_bind_group,
        frame_buf: Vec::new(),
        frame_seq: 0,
        screen,
    })
}

// ================= Scene =================

fn build_grid_vertices(size: i32, step: i32) -> Vec<Vertex> {
    // Grid on XZ plane (Y-up), axes colored RGB
    let mut verts: Vec<Vertex> = Vec::new();
    let half = size;

    // grid lines (grey)
    for i in (-half..=half).step_by(step as usize) {
        let x = i as f32;
        let z0 = -half as f32;
        let z1 = half as f32;

        // line parallel to Z at X
        verts.push(Vertex { position: [x, 0.0, z0], color: [0.3, 0.3, 0.3] });
        verts.push(Vertex { position: [x, 0.0, z1], color: [0.3, 0.3, 0.3] });

        let z = i as f32;
        let x0 = -half as f32;
        let x1 = half as f32;

        // line parallel to X at Z
        verts.push(Vertex { position: [x0, 0.0, z], color: [0.3, 0.3, 0.3] });
        verts.push(Vertex { position: [x1, 0.0, z], color: [0.3, 0.3, 0.3] });
    }

    // axes: X (red), Y (green), Z (blue)
    let a = -half as f32;
    let b = half as f32;
    // X axis
    verts.push(Vertex { position: [a, 0.0, 0.0], color: [1.0, 0.0, 0.0] });
    verts.push(Vertex { position: [b, 0.0, 0.0], color: [1.0, 0.0, 0.0] });
    // Y axis
    verts.push(Vertex { position: [0.0, a, 0.0], color: [0.0, 1.0, 0.0] });
    verts.push(Vertex { position: [0.0, b, 0.0], color: [0.0, 1.0, 0.0] });
    // Z axis
    verts.push(Vertex { position: [0.0, 0.0, a], color: [0.0, 0.0, 1.0] });
    verts.push(Vertex { position: [0.0, 0.0, b], color: [0.0, 0.0, 1.0] });

    verts
}

// Axis remapping: headset pose -> camera orientation in the scene (before calibration).
fn head_rotation_from_pose(pose: PoseCopy, invert: (bool, bool, bool)) -> Quat {
    // User mapping: y -> x, z -> y, x -> z
    let mapped_roll = pose.pitch; // new X = old Y
    let mapped_pitch = pose.yaw;  // new Y = old Z
    let mapped_yaw = pose.roll;   // new Z = old X

    // Flip X (roll) by default based on user's feedback (x was inverted)
    let mut rx = -mapped_roll;
    let mut ry = mapped_pitch;
    let mut rz = mapped_yaw;

    if invert.0 { rx = -rx; }
    if invert.1 { ry = -ry; }
    if invert.2 { rz = -rz; }

    let q = Quat::from_euler(glam::EulerRot::XYZ, rx, ry, rz);
    q.conjugate()
}

// The calibration for a recenter: the inverse of the head orientation, or with `yaw_only` the
// inverse of just its turn around the vertical axis (scene Y up, -Z forward), so pitch and
// roll stay measured against the real horizon.
fn recenter_calibration(head: Quat, yaw_only: bool) -> Quat {
    if !yaw_only {
        return head.inverse();
    }
    let forward = head * Vec3::NEG_Z;
    // Looking almost straight up or down, the forward direction has no usable heading; the
    // top of the head points where the face would (looking down) or away from it (looking up).
    let dir = if forward.x.hypot(forward.z) > 1e-3 {
        forward
    } else {
        let up = head * Vec3::Y;
        if forward.y < 0.0 { up } else { -up }
    };
    let yaw = (-dir.x).atan2(-dir.z);
    Quat::from_rotation_y(yaw).inverse()
}

// Camera view matrix for a camera orientation (already calibrated). With the neck model on, the
// eye swings around a pivot behind and below it; looking straight ahead it is exactly at CAM_POS.
fn camera_view(rotation: Quat, neck_model: f32) -> Mat4 {
    let eye_offset = NECK_TO_EYES * neck_model;
    let pivot = CAM_POS - eye_offset;
    let eye = pivot + rotation * eye_offset;
    Mat4::from_rotation_translation(rotation, eye).inverse()
}

// Find a monitor by display ID (CGDirectDisplayID, as shown by --list-displays). 0 = main display.
fn find_monitor(window: &Window, id: u32) -> Option<winit::monitor::MonitorHandle> {
    if id == 0 {
        return window.primary_monitor();
    }
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::MonitorHandleExtMacOS;
        window.available_monitors().find(|m| m.native_id() == id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// The display ID of a winit monitor (0 where it isn't known).
fn monitor_id(monitor: &winit::monitor::MonitorHandle) -> u32 {
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::MonitorHandleExtMacOS;
        monitor.native_id()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = monitor;
        0
    }
}

/// Fill display `id` with the window, or go back to a normal window with `None`.
/// Returns the display the window now fills (unchanged if `id` wasn't found).
///
/// On macOS this doesn't use native fullscreen (winit's `Fullscreen`): that needs its own
/// Space, which macOS doesn't reliably give a window on a secondary display such as the
/// glasses, and never gives an app without a Dock icon. Instead the window loses its border,
/// covers the display exactly, sits above the menu bar and stays on every Space.
fn apply_fullscreen(window: &Window, current: Option<u32>, id: Option<u32>) -> Option<u32> {
    match id {
        None => {
            #[cfg(target_os = "macos")]
            {
                set_above_menu_bar(window, false);
                window.set_decorations(true);
                window.set_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
            }
            #[cfg(not(target_os = "macos"))]
            window.set_fullscreen(None);
            None
        }
        Some(id) => match find_monitor(window, id) {
            Some(monitor) => {
                let actual = monitor_id(&monitor);
                #[cfg(target_os = "macos")]
                {
                    // Raise it first: macOS keeps normal-level windows below the menu bar.
                    set_above_menu_bar(window, true);
                    window.set_decorations(false);
                    window.set_outer_position(monitor.position());
                    window.set_inner_size(monitor.size());
                }
                #[cfg(not(target_os = "macos"))]
                window.set_fullscreen(Some(Fullscreen::Borderless(Some(monitor))));
                Some(actual)
            }
            None => {
                warn!("Display {id} not found; staying as it is.");
                current
            }
        },
    }
}

/// Raise the window above the menu bar and keep it on every Space of its display, or put it
/// back to a normal window.
#[cfg(target_os = "macos")]
fn set_above_menu_bar(window: &Window, on: bool) {
    use objc2::runtime::AnyObject;
    use objc2::msg_send;
    use winit::platform::macos::WindowExtMacOS;

    // NSStatusWindowLevel (25) + 1: above the menu bar and its status items, below pop-up menus.
    const ABOVE_MENU_BAR: isize = 26;
    const NORMAL_LEVEL: isize = 0;
    // NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorStationary
    const ALL_SPACES: usize = (1 << 0) | (1 << 4);

    let ns_window = window.ns_window() as *const AnyObject;
    if ns_window.is_null() {
        return;
    }
    // SAFETY: ns_window is the live NSWindow behind `window`, used on the main thread.
    unsafe {
        let ns_window = &*ns_window;
        let level = if on { ABOVE_MENU_BAR } else { NORMAL_LEVEL };
        let _: () = msg_send![ns_window, setLevel: level];
        let behavior: usize = if on { ALL_SPACES } else { 0 };
        let _: () = msg_send![ns_window, setCollectionBehavior: behavior];
    }
}

// HUD line describing the screen capture.
fn screen_status_line(tex_size: (u32, u32)) -> String {
    #[cfg(target_os = "macos")]
    {
        match capture_status() {
            CaptureStatus::Starting => "screen: starting".to_string(),
            CaptureStatus::NoPermission => "screen: no Screen Recording permission".to_string(),
            CaptureStatus::Failed => "screen: capture failed (see log)".to_string(),
            CaptureStatus::WaitingForFrames => "screen: waiting for first frame".to_string(),
            CaptureStatus::Capturing => format!("screen: capturing {}x{}", tex_size.0, tex_size.1),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = tex_size;
        "screen: capture not supported on this platform".to_string()
    }
}

fn make_proj(width: u32, height: u32, fov_deg: f32) -> Mat4 {
    let aspect = (width.max(1) as f32) / (height.max(1) as f32);
    let fovy = fov_deg.to_radians();
    let near = 0.01;
    let far = 1000.0;
    Mat4::perspective_rh(fovy, aspect, near, far)
}

// Simple FPS meter (console HUD)
struct FpsMeter {
    last_instant: Instant,
    accum_time: f32,
    frames: u32,
    fps: f32,
}
impl FpsMeter {
    fn new() -> Self {
        Self { last_instant: Instant::now(), accum_time: 0.0, frames: 0, fps: 0.0 }
    }
    fn on_frame(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_instant).as_secs_f32();
        self.last_instant = now;
        self.accum_time += dt;
        self.frames += 1;
        if self.accum_time >= 0.5 {
            self.fps = self.frames as f32 / self.accum_time;
            self.accum_time = 0.0;
            self.frames = 0;
        }
    }
}

// ================= Main =================

fn main() -> anyhow::Result<()> {
    // Initialize start instant for latency metrics
    START_INSTANT.get_or_init(Instant::now);

    // App logs at info; wgpu/naga logs at warn, since their info level logs every buffer it creates.
    // RUST_LOG overrides this (for example RUST_LOG=debug).
    env_logger::Builder::from_env(
        Env::default().default_filter_or("info,wgpu_core=warn,wgpu_hal=warn,wgpu=warn,naga=warn"),
    )
    .init();

    let cli = Cli::parse();
    cli.validate()?;

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

    info!(
        "vsync={}, invert=({},{},{}), poll_ms={}, no_hud={}, hud_size={}, max_fps={}",
        cli.vsync, cli.invert_x, cli.invert_y, cli.invert_z, cli.poll_ms, cli.no_hud, cli.hud_size, cli.max_fps
    );

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

    // Go fullscreen on the chosen display (normally the glasses).
    let mut fullscreen_on: Option<u32> = None;
    if let Some(id) = cli.fullscreen {
        match find_monitor(&window, id) {
            Some(monitor) => {
                #[cfg(target_os = "macos")]
                {
                    use winit::platform::macos::MonitorHandleExtMacOS;
                    let main_id = window.primary_monitor().map(|m| m.native_id());
                    let resolve = |d: u32| if d == 0 { main_id } else { Some(d) };
                    if resolve(id).is_some() && resolve(id) == resolve(cli.display) {
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
                window.set_fullscreen(Some(Fullscreen::Borderless(Some(monitor))));
            }
            None => warn!("Display {id} not found for --fullscreen; staying windowed. Run with --list-displays to see the IDs."),
        }
    }
    let mut window_scale_factor = window.scale_factor() as f32;

    // Scene vertices
    let grid_vertices = build_grid_vertices(20, 1);

    // Shared pose (atomic, lock-free)
    let pose_shared = Arc::new(PoseAtomic::new());

    // Calibration lives on the render thread only, so no shared state or lock is needed.
    // `calib_inv` is the inverse of the head orientation at the last recenter; applying it as a
    // rotation (not per-axis Euler subtraction) stays correct with pitch/roll and across ±180°.
    let mut calibrate_pending = false;
    let mut calib_inv = Quat::IDENTITY;
    let mut cursor_px: (f32, f32) = (-1.0, -1.0);

    // Fusion thread (max cadence by default; optional rate limit via --poll-ms)
    {
        let pose_ref = pose_shared.clone();
        let poll_interval = if cli.poll_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(cli.poll_ms))
        };

        thread::spawn(move || {
            // macOS: bump QoS to user-interactive
            #[cfg(target_os = "macos")]
            unsafe {
                extern "C" {
                    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
                }
                // QOS_CLASS_USER_INTERACTIVE = 0x21
                let _ = pthread_set_qos_class_self_np(0x21, 0);
            }

            // Connect to fusion, retrying until glasses are plugged in. The renderer keeps
            // running meanwhile and the HUD shows "device: not-found".
            let mut warned = false;
            let mut fusion = loop {
                // Probe with any_glasses() first: ar_drivers::any_fusion() calls
                // any_glasses_or_fail(), which exits the whole process when no glasses are found.
                // any_glasses() returns an error instead. Drop the probe to release the device,
                // then let any_fusion() open it again.
                let probe = any_glasses().map(drop);
                match probe.and_then(|()| any_fusion()) {
                    Ok(f) => {
                        info!("Headset connected");
                        pose_ref.write(0.0, 0.0, 0.0, true, now_ns_from_start());
                        break f;
                    }
                    Err(e) => {
                        if !warned {
                            warn!("No headset device found: {e}. Retrying every second.");
                            warned = true;
                        }
                        pose_ref.write(0.0, 0.0, 0.0, false, now_ns_from_start());
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            };

            let mut last = Instant::now();
            loop {
                fusion.update();
                let frd = fusion.attitude_frd_rad();

                // Publish the raw pose; calibration is applied on the render thread.
                let now_ns = now_ns_from_start();
                pose_ref.write(frd[0], frd[1], frd[2], true, now_ns);

                if let Some(dt) = poll_interval {
                    let now = Instant::now();
                    if now - last < dt {
                        let sleep_for = dt - (now - last);
                        thread::sleep(sleep_for);
                    }
                    last = Instant::now();
                } else {
                    // run as fast as possible, but yield cooperatively
                    thread::yield_now();
                }
            }
        });
    }

    // GPU init
    let mut gpu = pollster::block_on(init_gpu(&window, &cli, &grid_vertices))?;

    // The captured display, as a real ID (0 on the command line means the main display).
    #[cfg(target_os = "macos")]
    let mut shown_display = resolve_display_id(cli.display);

    // Start virtual monitor stream (macOS) via screencapturekit-rs
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = start_stream(shown_display, !cli.no_cursor) {
            warn!("Screen capture failed to start: {e}");
        }
    }

    // FPS meter
    let mut fps = FpsMeter::new();

    // Menu bar icon: created once the event loop runs (macOS requires that). Its menu and the
    // global shortcuts send Commands to this loop through the proxy.
    let proxy = event_loop.create_proxy();
    #[cfg(target_os = "macos")]
    let mut menu_bar: Option<MenuBar> = None;
    let mut last_display_check = Instant::now();

    // Event loop / render (winit 0.28 API)
    event_loop.run(move |event, _, control_flow| {
        use winit::event_loop::ControlFlow;
        let mut command: Option<Command> = None;
        // Frame pacing: only apply WaitUntil when no-vsync. With vsync enabled, let OS/VBlank drive redraws via Wait and explicit request_redraw on events.
        if !cli.vsync && cli.max_fps > 0 {
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_micros((1_000_000u64 / cli.max_fps as u64).max(1)));
        } else if cli.vsync {
            *control_flow = ControlFlow::Wait;
        } else {
            *control_flow = ControlFlow::Poll;
        }

        match event {
            Event::WindowEvent { event, window_id } if window_id == window.id() => {
                match event {
                    WindowEvent::CloseRequested => command = Some(Command::Quit),
                    WindowEvent::KeyboardInput {
                        input: KeyboardInput { state: ElementState::Pressed, virtual_keycode: Some(key), .. },
                        ..
                    } => {
                        // Keys in the window. Held keys repeat, so the screen moves smoothly.
                        command = match key {
                            VirtualKeyCode::Escape => Some(Command::Quit),
                            VirtualKeyCode::C => Some(Command::Recenter),
                            VirtualKeyCode::F => Some(Command::ToggleFullscreen),
                            VirtualKeyCode::H => Some(Command::ToggleHud),
                            VirtualKeyCode::P => Some(Command::OpenScreenSettings),
                            VirtualKeyCode::Up => Some(Command::Further),
                            VirtualKeyCode::Down => Some(Command::Closer),
                            VirtualKeyCode::Equals | VirtualKeyCode::Plus | VirtualKeyCode::NumpadAdd => Some(Command::Bigger),
                            VirtualKeyCode::Minus | VirtualKeyCode::NumpadSubtract => Some(Command::Smaller),
                            VirtualKeyCode::R => Some(Command::ResetScreen),
                            _ => None,
                        };
                    }
                    WindowEvent::Resized(new_size) => {
                        if new_size.width > 0 && new_size.height > 0 {
                            gpu.config.width = new_size.width;
                            gpu.config.height = new_size.height;
                            gpu.surface.configure(&gpu.device, &gpu.config);
                        }
                    }
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        window_scale_factor = scale_factor as f32;
                        let size = window.inner_size();
                        if size.width > 0 && size.height > 0 {
                            gpu.config.width = size.width;
                            gpu.config.height = size.height;
                            gpu.surface.configure(&gpu.device, &gpu.config);
                        }
                    }
                    WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                        // Only a click on the Calibrate button recenters (both are in physical pixels).
                        if gpu.hud_enabled && gpu.calibrate_btn.contains(cursor_px.0, cursor_px.1) {
                            command = Some(Command::Recenter);
                        }
                        if gpu.settings_btn.is_some_and(|b| b.contains(cursor_px.0, cursor_px.1)) {
                            command = Some(Command::OpenScreenSettings);
                        }
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        cursor_px = (position.x as f32, position.y as f32);
                    }
                    WindowEvent::CursorLeft { .. } => {
                        cursor_px = (-1.0, -1.0);
                    }
                    _ => {}
                }
            }
            Event::MainEventsCleared => {
                // Request redraws appropriately:
                // - No vsync: drive redraw every loop to maximize FPS (Poll or WaitUntil pacing).
                // - Vsync: request redraw here so the Window handles VBlank pacing without starving the event loop.
                window.request_redraw();

                // Pick up displays that were connected or removed (the glasses, say).
                #[cfg(target_os = "macos")]
                if last_display_check.elapsed() >= Duration::from_secs(2) {
                    last_display_check = Instant::now();
                    if let Some(bar) = menu_bar.as_mut() {
                        bar.refresh_displays();
                    }
                }
            }
            Event::NewEvents(StartCause::Init) => {
                #[cfg(target_os = "macos")]
                match MenuBar::new(
                    proxy.clone(),
                    shown_display,
                    fullscreen_on,
                    gpu.hud_enabled,
                    gpu.glyph_brush.is_some(),
                ) {
                    Ok(bar) => menu_bar = Some(bar),
                    Err(e) => warn!("Menu bar icon unavailable: {e}"),
                }
                if fullscreen_on.is_some() {
                    fullscreen_on = apply_fullscreen(&window, None, fullscreen_on);
                }
            }
            Event::UserEvent(cmd) => command = Some(cmd),
            Event::LoopDestroyed => {
                // Every way of quitting ends here: the menu bar, Esc, closing the window, and a
                // Quit from macOS itself (Cmd-Q, logout), which skips Command::Quit.
                #[cfg(target_os = "macos")]
                stop_stream();
                info!("Quitting");
            }
            Event::RedrawRequested(id) if id == window.id() => {
                // Acquire frame
                let frame = match gpu.surface.get_current_texture() {
                    Ok(f) => f,
                    Err(wgpu::SurfaceError::Lost) => {
                        // Surface lost; reconfigure then try next frame
                        warn!("Surface lost, reconfiguring.");
                        gpu.surface.configure(&gpu.device, &gpu.config);
                        return;
                    }
                    Err(wgpu::SurfaceError::OutOfMemory) => {
                        // Critical; exit to avoid freeze
                        warn!("Surface out of memory; exiting.");
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    Err(e) => {
                        // Other errors: Timeout/Outdated - reconfigure and skip this frame
                        warn!("Surface acquire error: {e}. Reconfiguring surface.");
                        gpu.surface.configure(&gpu.device, &gpu.config);
                        return;
                    }
                };
                let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

                // Read pose
                let (r, p, y, has, t_ns) = pose_shared.read();
                let pose_copy = PoseCopy { roll: r, pitch: p, yaw: y };
                let head = head_rotation_from_pose(pose_copy, (cli.invert_x, cli.invert_y, cli.invert_z));

                // Recenter: make the current head orientation (or its heading) the new "forward".
                if calibrate_pending {
                    calibrate_pending = false;
                    calib_inv = recenter_calibration(head, cli.recenter_yaw_only);
                    gpu.show_calibrated_toast_until_ns = now_ns_from_start() + 1_000_000_000;
                }

                let vp = {
                    let view_m = camera_view(calib_inv * head, cli.neck_model);
                    let proj = make_proj(gpu.config.width, gpu.config.height, cli.fov);
                    proj * view_m
                };
                gpu.uniform.update(vp);
                gpu.queue.write_buffer(&gpu.uniform_buffer, 0, bytemuck::bytes_of(&gpu.uniform));

                // Upload the newest screen frame (macOS only). Skips entirely when no new frame
                // has arrived; the frame buffer is swapped in, not copied.
                #[cfg(target_os = "macos")]
                {
                    if let Some(info) = take_latest_bgra(&mut gpu.frame_buf, &mut gpu.frame_seq) {
                        let (w, h, bpr) = (info.width, info.height, info.bytes_per_row);
                        let max_dim = gpu.device.limits().max_texture_dimension_2d;
                        let fits = w <= max_dim && h <= max_dim && gpu.frame_buf.len() >= (bpr as usize) * (h as usize);
                        if fits {
                            if gpu.tex_size != (w, h) {
                                // Rare: first frame, or the display resolution changed.
                                info!("Screen texture resized to {w}x{h}");
                                let (tex, bg) = create_screen_texture(&gpu.device, &gpu.quad_bgl, &gpu.quad_sampler, w, h);
                                gpu.tex = tex;
                                gpu.quad_bind_group = bg;
                                gpu.tex_size = (w, h);
                                write_quad(&gpu);
                            }
                            gpu.queue.write_texture(
                                wgpu::ImageCopyTexture {
                                    texture: &gpu.tex,
                                    mip_level: 0,
                                    origin: wgpu::Origin3d::ZERO,
                                    aspect: wgpu::TextureAspect::All,
                                },
                                &gpu.frame_buf[..(bpr as usize) * (h as usize)],
                                wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(h) },
                                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                            );
                        } else if gpu.tex_size != (0, 0) {
                            warn!("Captured frame {w}x{h} exceeds GPU texture limit {max_dim}; not displayed");
                            gpu.tex_size = (0, 0); // log once until a frame fits again
                        }
                    }
                }

                // Render 3D scene
                let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("encoder") });
                {
                    let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("grid-pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.02, g: 0.02, b: 0.03, a: 1.0 }),
                                store: true,
                            },
                        })],
                        depth_stencil_attachment: None,
                    });

                    if let Some(bundle) = &gpu.render_bundle {
                        rpass.execute_bundles(std::iter::once(bundle));
                    } else {
                        rpass.set_pipeline(&gpu.pipeline);
                        rpass.set_bind_group(0, &gpu.uniform_bind_group, &[]);
                        rpass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
                        rpass.draw(0..gpu.vertex_count, 0..1);
                    }
                }

                // Draw virtual monitor quad in front of camera (translate forward)
                {
                    let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("quad-pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: true },
                        })],
                        depth_stencil_attachment: None,
                    });
                    rpass.set_pipeline(&gpu.quad_pipeline);
                    rpass.set_bind_group(0, &gpu.uniform_bind_group, &[]);
                    rpass.set_bind_group(1, &gpu.quad_bind_group, &[]);
                    rpass.set_vertex_buffer(0, gpu.quad_vb.slice(..));
                    rpass.set_index_buffer(gpu.quad_ib.slice(..), wgpu::IndexFormat::Uint16);
                    rpass.draw_indexed(0..gpu.quad_index_count, 0, 0..1);
                }

                // FPS update
                fps.on_frame();
                let now_ns = now_ns_from_start();
                let latency_ms = ((now_ns.saturating_sub(t_ns)) as f64) / 1_000_000.0;

                // Refresh the HUD strings at most every 100 ms: readable, and it avoids formatting
                // (and allocating) strings every frame. Skipped entirely when the HUD is off.
                if gpu.hud_enabled
                    && (gpu.hud_text.is_empty() || now_ns.saturating_sub(gpu.hud_text_updated_ns) >= HUD_REFRESH_NS)
                {
                    let dev = if has { "connected" } else { "not-found" };
                    gpu.hud_text = vec![
                        format!("device: {dev}"),
                        format!("fps: {:.1}", fps.fps.max(0.0)),
                        format!("latency: {:.2} ms", latency_ms),
                        format!("euler(rad) r={:.3} p={:.3} y={:.3}", r, p, y),
                        format!("invert=({},{},{}) vsync={}", cli.invert_x, cli.invert_y, cli.invert_z, cli.vsync),
                        screen_status_line(gpu.tex_size),
                        format!("distance: {:.2}  height: {:.2}", gpu.screen.distance, gpu.screen.height),
                    ];
                    gpu.hud_text_updated_ns = now_ns;
                }

                // Queue HUD text every frame: wgpu_glyph draws only what was queued for this frame,
                // so skipping the queue makes the HUD disappear on that frame.
                let scale = gpu.hud_font_size * window_scale_factor;
                let pad = gpu.hud_padding * window_scale_factor;
                let mut y_cursor = pad;
                if gpu.hud_enabled {
                    if let Some(gb) = gpu.glyph_brush.as_mut() {
                        for line in &gpu.hud_text {
                            gb.queue(Section {
                                screen_position: (pad, y_cursor),
                                bounds: (gpu.config.width as f32, gpu.config.height as f32),
                                text: vec![
                                    Text::new(line)
                                        .with_color([0.95, 0.95, 0.97, 1.0])
                                        .with_scale(scale),
                                ],
                                ..Section::default()
                            });
                            y_cursor += scale + 4.0;
                        }
                    }
                }

                // Calibrate button
                let btn_text = "Calibrate";
                let btn_w = 110.0 * window_scale_factor;
                let btn_h = 28.0 * window_scale_factor;
                let btn_x = (gpu.config.width as f32) - btn_w - pad;
                let btn_y = pad;
                gpu.calibrate_btn = Rect { x: btn_x, y: btn_y, w: btn_w, h: btn_h };

                if gpu.hud_enabled {
                    if let Some(gb) = gpu.glyph_brush.as_mut() {
                        gb.queue(Section {
                            screen_position: (btn_x + 10.0, btn_y + btn_h * 0.25),
                            bounds: (gpu.config.width as f32, gpu.config.height as f32),
                            text: vec![
                                Text::new(btn_text)
                                    .with_color([0.9, 0.9, 0.2, 1.0])
                                    .with_scale(scale),
                            ],
                            ..Section::default()
                        });
                    }
                }

                // Toast after calibration
                if gpu.hud_enabled && now_ns < gpu.show_calibrated_toast_until_ns {
                    if let Some(gb) = gpu.glyph_brush.as_mut() {
                        gb.queue(Section {
                            screen_position: (pad, y_cursor + 8.0),
                            bounds: (gpu.config.width as f32, gpu.config.height as f32),
                            text: vec![
                                Text::new("Calibrated")
                                    .with_color([0.2, 0.9, 0.3, 1.0])
                                    .with_scale(scale),
                            ],
                            ..Section::default()
                        });
                    }
                }

                // Screen capture problems: a "Screen Recording settings" button under Calibrate,
                // and a hint in the middle of the window (over the still-black virtual screen).
                #[cfg(target_os = "macos")]
                {
                    let status = capture_status();
                    let needs_settings = matches!(status, CaptureStatus::NoPermission | CaptureStatus::Failed);
                    gpu.settings_btn = None;
                    if gpu.hud_enabled && needs_settings {
                        let label = "Screen Recording settings (P)";
                        // Rough text width: wide enough to cover the label for click hit-testing.
                        let w = label.len() as f32 * scale * 0.55 + 20.0 * window_scale_factor;
                        let rect = Rect { x: (gpu.config.width as f32) - w - pad, y: btn_y + btn_h + 6.0 * window_scale_factor, w, h: btn_h };
                        gpu.settings_btn = Some(rect);
                        if let Some(gb) = gpu.glyph_brush.as_mut() {
                            gb.queue(Section {
                                screen_position: (rect.x + 10.0, rect.y + rect.h * 0.25),
                                bounds: (gpu.config.width as f32, gpu.config.height as f32),
                                text: vec![Text::new(label).with_color([0.4, 0.75, 1.0, 1.0]).with_scale(scale)],
                                ..Section::default()
                            });
                        }
                    }
                    let hint: Option<&str> = match status {
                        CaptureStatus::NoPermission => Some(
                            "Screen Recording permission needed\n\n\
                             Press P (or click the button at the top right) to open System Settings,\n\
                             allow your terminal app under Screen Recording, then quit and reopen the terminal.",
                        ),
                        CaptureStatus::Failed => Some("Screen capture failed to start. See the terminal log for details."),
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
                // Return the HUD's staging buffers for reuse once the GPU is done with them.
                // Without this the belt creates a new buffer every frame and memory keeps growing.
                if let Some(sb) = gpu.staging_belt.as_mut() {
                    sb.recall();
                }
                // Present the frame (present() does not return a Result on wgpu 0.17)
                frame.present();
            }
            _ => {}
        }

        // Carry out a command from a key, the menu bar or a global shortcut.
        if let Some(cmd) = command {
            match cmd {
                Command::Quit => {
                    // Stop ScreenCaptureKit cleanly, then exit.
                    #[cfg(target_os = "macos")]
                    stop_stream();
                    *control_flow = ControlFlow::Exit;
                }
                Command::Recenter => calibrate_pending = true,
                Command::Further => gpu.screen.scale_distance(SCREEN_STEP),
                Command::Closer => gpu.screen.scale_distance(1.0 / SCREEN_STEP),
                Command::Bigger => gpu.screen.scale_height(SCREEN_STEP),
                Command::Smaller => gpu.screen.scale_height(1.0 / SCREEN_STEP),
                Command::ResetScreen => gpu.screen = ScreenPlacement::from_cli(&cli),
                Command::ToggleFullscreen => {
                    // Fullscreen on whichever display the window is on, or back to a window.
                    let target = if fullscreen_on.is_some() {
                        None
                    } else {
                        window.current_monitor().map(|m| monitor_id(&m))
                    };
                    fullscreen_on = apply_fullscreen(&window, fullscreen_on, target);
                }
                Command::FullscreenOn(id) => fullscreen_on = apply_fullscreen(&window, fullscreen_on, id),
                Command::ShowDisplay(id) => {
                    #[cfg(target_os = "macos")]
                    if id != shown_display {
                        // Restarting the capture blocks briefly; fine for a menu click.
                        match switch_display(id, !cli.no_cursor) {
                            Ok(()) => shown_display = id,
                            Err(e) => warn!("Couldn't switch to display {id}: {e}"),
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
            }
            if matches!(
                cmd,
                Command::Further | Command::Closer | Command::Bigger | Command::Smaller | Command::ResetScreen
            ) {
                write_quad(&gpu);
            }
            gpu.hud_text.clear(); // refresh the HUD's text now
            #[cfg(target_os = "macos")]
            if let Some(bar) = menu_bar.as_mut() {
                bar.set_state(shown_display, fullscreen_on, gpu.hud_enabled);
            }
        }
    });
    // Unreachable with winit 0.28 run loop, but return Ok for signature completeness.
    // Ok(())
}
// ================= Tests =================
// These cover the parts that don't need glasses or a display: the head-rotation maths, screen
// placement and CLI checks. Run them with `cargo test --workspace`.

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-4;

    fn assert_vec_eq(a: Vec3, b: Vec3) {
        assert!(a.abs_diff_eq(b, EPS), "{a:?} != {b:?}");
    }

    fn assert_rot_eq(a: Quat, b: Quat) {
        // q and -q are the same rotation.
        assert!(a.abs_diff_eq(b, EPS) || a.abs_diff_eq(-b, EPS), "{a:?} != {b:?}");
    }

    fn pose(roll: f32, pitch: f32, yaw: f32) -> Quat {
        head_rotation_from_pose(PoseCopy { roll, pitch, yaw }, (false, false, false))
    }

    /// A head turned by `yaw`, then tilted up by `pitch`, then rolled by `roll` (radians).
    fn head(yaw: f32, pitch: f32, roll: f32) -> Quat {
        Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch) * Quat::from_rotation_z(roll)
    }

    // --- Axis mapping. The signs pin the behaviour confirmed on XREAL Air glasses. ---

    #[test]
    fn zero_pose_looks_straight_ahead() {
        assert_rot_eq(pose(0.0, 0.0, 0.0), Quat::IDENTITY);
    }

    #[test]
    fn pose_yaw_turns_around_the_vertical_axis() {
        let q = pose(0.0, 0.0, 0.5);
        assert_vec_eq(q * Vec3::Y, Vec3::Y);
        assert_vec_eq(q * Vec3::NEG_Z, Vec3::new(0.5f32.sin(), 0.0, -0.5f32.cos()));
    }

    #[test]
    fn pose_pitch_looks_up_and_down() {
        let q = pose(0.0, 0.5, 0.0);
        assert_vec_eq(q * Vec3::X, Vec3::X);
        assert_vec_eq(q * Vec3::NEG_Z, Vec3::new(0.0, 0.5f32.sin(), -0.5f32.cos()));
    }

    #[test]
    fn pose_roll_tilts_around_the_view_direction() {
        let q = pose(0.5, 0.0, 0.0);
        assert_vec_eq(q * Vec3::NEG_Z, Vec3::NEG_Z);
        assert_vec_eq(q * Vec3::Y, Vec3::new(0.5f32.sin(), 0.5f32.cos(), 0.0));
    }

    #[test]
    fn invert_flags_reverse_each_axis() {
        let p = PoseCopy { roll: 0.3, pitch: 0.2, yaw: 0.1 };
        let flipped = PoseCopy { roll: -0.3, pitch: -0.2, yaw: -0.1 };
        assert_rot_eq(
            head_rotation_from_pose(p, (true, true, true)),
            head_rotation_from_pose(flipped, (false, false, false)),
        );
        // Each flag on its own changes exactly one axis.
        for (flags, expect) in [
            ((true, false, false), PoseCopy { pitch: -0.2, ..p }),
            ((false, true, false), PoseCopy { yaw: -0.1, ..p }),
            ((false, false, true), PoseCopy { roll: -0.3, ..p }),
        ] {
            assert_rot_eq(head_rotation_from_pose(p, flags), head_rotation_from_pose(expect, (false, false, false)));
        }
    }

    // --- Recentering ---

    #[test]
    fn full_recenter_cancels_the_head_rotation() {
        for h in [head(0.0, 0.0, 0.0), head(1.0, 0.3, -0.2), head(-2.9, -0.8, 0.6), head(3.1, 0.0, 0.0)] {
            assert_rot_eq(recenter_calibration(h, false) * h, Quat::IDENTITY);
        }
    }

    #[test]
    fn yaw_only_recenter_without_tilt_matches_full_recenter() {
        for yaw in [-3.0, -1.0, 0.0, 0.7, 2.5] {
            let h = head(yaw, 0.0, 0.0);
            assert_rot_eq(recenter_calibration(h, true), recenter_calibration(h, false));
        }
    }

    #[test]
    fn yaw_only_recenter_keeps_tilt_and_horizon() {
        for (yaw, pitch, roll) in [(1.0, 0.4, 0.0), (-2.0, -0.6, 0.3), (3.0, 0.2, -0.5), (0.0, 1.2, 0.0)] {
            let calib = recenter_calibration(head(yaw, pitch, roll), true);
            // The correction only turns around the vertical axis, so the horizon stays level...
            assert_vec_eq(calib * Vec3::Y, Vec3::Y);
            // ...and what's left after recentering is exactly the tilt.
            assert_rot_eq(calib * head(yaw, pitch, roll), head(0.0, pitch, roll));
        }
    }

    #[test]
    fn yaw_only_recenter_works_looking_straight_up_or_down() {
        let half_pi = std::f32::consts::FRAC_PI_2;
        for yaw in [-2.0, 0.0, 1.0] {
            for pitch in [-half_pi, half_pi] {
                let calib = recenter_calibration(head(yaw, pitch, 0.0), true);
                assert_rot_eq(calib, Quat::from_rotation_y(-yaw));
            }
        }
    }

    // --- Camera ---

    #[test]
    fn camera_looking_ahead_is_at_cam_pos() {
        for neck in [0.0, 1.0, 3.0] {
            let eye = camera_view(Quat::IDENTITY, neck).inverse().transform_point3(Vec3::ZERO);
            assert_vec_eq(eye, CAM_POS);
        }
    }

    #[test]
    fn neck_model_off_keeps_the_eye_still() {
        let eye = camera_view(head(1.0, 0.5, 0.2), 0.0).inverse().transform_point3(Vec3::ZERO);
        assert_vec_eq(eye, CAM_POS);
    }

    #[test]
    fn neck_model_moves_the_eye_when_turning() {
        let eye = camera_view(head(1.0, 0.0, 0.0), 1.0).inverse().transform_point3(Vec3::ZERO);
        assert!(eye.distance(CAM_POS) > 0.01);
        // The eye swings around the neck, so it stays at the same height when only turning.
        assert!((eye.y - CAM_POS.y).abs() < EPS);
    }

    #[test]
    fn screen_centre_is_straight_ahead_in_view_space() {
        let screen = ScreenPlacement { height: 1.4, distance: 4.0 };
        let centre = CAM_POS - Vec3::Z * screen.distance;
        let v = camera_view(Quat::IDENTITY, 1.0).transform_point3(centre);
        assert_vec_eq(v, Vec3::new(0.0, 0.0, -4.0));
    }

    #[test]
    fn projection_handles_a_zero_sized_window() {
        let m = make_proj(0, 0, 60.0);
        assert!(m.to_cols_array().iter().all(|x| x.is_finite()));
    }

    // --- Screen placement ---

    #[test]
    fn screen_quad_has_the_right_size_and_place() {
        let screen = ScreenPlacement { height: 2.0, distance: 3.0 };
        let v = quad_vertices(screen, 16.0 / 9.0);
        let width = v[1].pos[0] - v[0].pos[0];
        let height = v[0].pos[1] - v[3].pos[1];
        assert!((width - 2.0 * 16.0 / 9.0).abs() < EPS);
        assert!((height - 2.0).abs() < EPS);
        for t in &v {
            assert!((t.pos[2] - (CAM_POS.z - 3.0)).abs() < EPS);
        }
        // Top-left of the image is at the top-left corner (the texture isn't flipped).
        assert_eq!(v[0].uv, [0.0, 0.0]);
        assert!(v[0].pos[0] < v[1].pos[0] && v[0].pos[1] > v[3].pos[1]);
    }

    #[test]
    fn screen_keys_stay_within_their_ranges() {
        let mut s = ScreenPlacement { height: 1.0, distance: 1.0 };
        for _ in 0..200 {
            s.scale_distance(SCREEN_STEP);
            s.scale_height(SCREEN_STEP);
        }
        assert_eq!(s.distance, SCREEN_DISTANCE_RANGE.1);
        assert_eq!(s.height, SCREEN_HEIGHT_RANGE.1);
        for _ in 0..200 {
            s.scale_distance(1.0 / SCREEN_STEP);
            s.scale_height(1.0 / SCREEN_STEP);
        }
        assert_eq!(s.distance, SCREEN_DISTANCE_RANGE.0);
        assert_eq!(s.height, SCREEN_HEIGHT_RANGE.0);
    }

    #[test]
    fn screen_keys_dont_snap_a_flag_value_outside_the_range() {
        // A distance set by flag beyond the keys' range isn't pulled in by a key press...
        let mut s = ScreenPlacement { height: 1.0, distance: 100.0 };
        s.scale_distance(SCREEN_STEP);
        assert_eq!(s.distance, 100.0);
        // ...but can still be moved back towards it.
        s.scale_distance(1.0 / SCREEN_STEP);
        assert!(s.distance < 100.0);
    }

    // --- Pose handoff and CLI ---

    #[test]
    fn pose_atomic_round_trips() {
        let p = PoseAtomic::new();
        assert_eq!(p.read(), (0.0, 0.0, 0.0, false, 0));
        p.write(0.1, -0.2, 3.0, true, 42);
        assert_eq!(p.read(), (0.1, -0.2, 3.0, true, 42));
    }

    fn parse(args: &[&str]) -> anyhow::Result<Cli> {
        let cli = Cli::try_parse_from(std::iter::once("headset-visualizer").chain(args.iter().copied()))?;
        cli.validate()?;
        Ok(cli)
    }

    #[test]
    fn default_flags_are_valid() {
        let cli = parse(&[]).unwrap();
        assert!(!cli.recenter_yaw_only);
    }

    #[test]
    fn bad_flag_values_are_rejected() {
        for args in [
            &["--screen-height", "0"][..],
            &["--screen-distance", "-1"],
            &["--fov", "180"],
            &["--fov", "1"],
            &["--neck-model", "3.5"],
            &["--screen-height", "NaN"],
        ] {
            assert!(parse(args).is_err(), "accepted {args:?}");
        }
    }
}
