use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use env_logger::Env;
use log::{info, warn};

use glam::{Mat4, Quat, Vec3};
use wgpu::util::DeviceExt;
use winit::event::{ElementState, Event, KeyboardInput, MouseButton, VirtualKeyCode, WindowEvent};
use winit::event_loop::EventLoop;
use winit::window::Window;

// Text overlay
use ab_glyph::FontArc;
use wgpu_glyph::{Section, Text, GlyphBrushBuilder, GlyphBrush};

// On macOS, pull capture helpers into scope (now using screencapturekit crate under the hood)
#[cfg(target_os = "macos")]
use crate::macos_capture::{take_latest_bgra, start_stream};

// Headset fusion API (path dependency to headset-utils)
use ar_drivers::any_fusion;

// macOS screen capture (virtual monitor)
#[cfg(target_os = "macos")]
mod macos_capture;

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
}

// ================= Low-latency Pose (Atomics) =================

#[cfg(target_os = "macos")]
const DEFAULT_DISPLAY_ID: u32 = 0; // 0 = main display

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

// Monotonic app start Instant to derive cheap ns timestamps
static mut START_INSTANT: Option<Instant> = None;
#[inline]
fn now_ns_from_start() -> u64 {
    unsafe {
        let start = START_INSTANT.unwrap();
        (Instant::now() - start).as_nanos() as u64
    }
}

// Just for convenience when building view/proj and printing
#[derive(Debug, Clone, Copy)]
struct PoseCopy {
    roll: f32,
    pitch: f32,
    yaw: f32,
    has_device: bool,
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
    show_calibrated_toast_until_ns: u64,

    // HUD caching
    cached_hud_text: Vec<String>,
    cached_scale_factor: f32,

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
}

// Virtual monitor height in world units; width follows the captured screen's aspect ratio.
const QUAD_HALF_HEIGHT: f32 = 0.7;

fn quad_vertices(aspect: f32) -> [TVertex; 4] {
    let hh = QUAD_HALF_HEIGHT;
    let hw = hh * aspect;
    [
        TVertex { pos: [-hw,  hh, 0.0], uv: [0.0, 0.0] },
        TVertex { pos: [ hw,  hh, 0.0], uv: [1.0, 0.0] },
        TVertex { pos: [ hw, -hh, 0.0], uv: [1.0, 1.0] },
        TVertex { pos: [-hw, -hh, 0.0], uv: [0.0, 1.0] },
    ]
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

async fn init_gpu(window: &Window, vsync: bool, vertices: &[Vertex], hud_forced_off: bool, hud_size: f32) -> anyhow::Result<Gpu> {
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
 o.uv = vec2<f32>(in.uv.x, 1.0 - in.uv.y);
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
        contents: bytemuck::cast_slice(&quad_vertices(1.6)),
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
    let mut render_bundle: Option<wgpu::RenderBundle> = None;
    {
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
        render_bundle = Some(encoder.finish(&wgpu::RenderBundleDescriptor {
            label: Some("grid-bundle"),
        }));
    }

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
        show_calibrated_toast_until_ns,
        cached_hud_text: Vec::new(),
        cached_scale_factor: 0.0,
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

// Camera view matrix for a camera orientation (already calibrated).
fn camera_view(rotation: Quat) -> Mat4 {
    let cam_pos = Vec3::new(0.0, 1.5, 4.0);
    Mat4::from_rotation_translation(rotation, cam_pos).inverse()
}

fn make_proj(width: u32, height: u32) -> Mat4 {
    let aspect = (width.max(1) as f32) / (height.max(1) as f32);
    let fovy = 60.0f32.to_radians();
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
    unsafe { START_INSTANT = Some(Instant::now()); }

    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    info!(
        "vsync={}, invert=({},{},{}), poll_ms={}, no_hud={}, hud_size={}, max_fps={}",
        cli.vsync, cli.invert_x, cli.invert_y, cli.invert_z, cli.poll_ms, cli.no_hud, cli.hud_size, cli.max_fps
    );

    // Window and event loop (winit 0.28 API)
    let event_loop = EventLoop::new();
    let window = Window::new(&event_loop).expect("create window");
    window.set_title("Headset Visualizer");
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
                match any_fusion() {
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
    let mut gpu = pollster::block_on(init_gpu(&window, cli.vsync, &grid_vertices, cli.no_hud, cli.hud_size))?;

    // Start virtual monitor stream (macOS) via screencapturekit-rs
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = start_stream(DEFAULT_DISPLAY_ID) {
            warn!("virtual monitor stream failed to start: {e}. If this is a permission issue, grant Screen Recording in System Settings > Privacy & Security.");
        }
    }

    // FPS meter
    let mut fps = FpsMeter::new();

    // Event loop / render (winit 0.28 API)
    event_loop.run(move |event, _, control_flow| {
        use winit::event_loop::ControlFlow;
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
                    WindowEvent::CloseRequested => {
                        *control_flow = ControlFlow::Exit;
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
                            calibrate_pending = true;
                        }
                    }
                    WindowEvent::KeyboardInput {
                        input: KeyboardInput { state: ElementState::Pressed, virtual_keycode: Some(VirtualKeyCode::C), .. },
                        ..
                    } => {
                        // Keyboard recenter; also works with --no-hud, where there is no button.
                        calibrate_pending = true;
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
                let pose_copy = PoseCopy { roll: r, pitch: p, yaw: y, has_device: has };
                let head = head_rotation_from_pose(pose_copy, (cli.invert_x, cli.invert_y, cli.invert_z));

                // Recenter: make the current head orientation the new "forward".
                if calibrate_pending {
                    calibrate_pending = false;
                    calib_inv = head.inverse();
                    gpu.show_calibrated_toast_until_ns = now_ns_from_start() + 1_000_000_000;
                    gpu.cached_hud_text.clear(); // force HUD refresh so the toast shows now
                }

                let vp = {
                    let view_m = camera_view(calib_inv * head);
                    let proj = make_proj(gpu.config.width, gpu.config.height);
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
                                let verts = quad_vertices(w as f32 / h as f32);
                                gpu.queue.write_buffer(&gpu.quad_vb, 0, bytemuck::cast_slice(&verts));
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

                // Build HUD strings (prepare but only queue if changed to reduce CPU work)
                let dev = if has { "connected" } else { "not-found" };
                let hud_lines = vec![
                    format!("device: {dev}"),
                    format!("fps: {:.1}", fps.fps.max(0.0)),
                    format!("latency: {:.2} ms", latency_ms),
                    format!("euler(rad) r={:.3} p={:.3} y={:.3}", r, p, y),
                    format!("invert=({},{},{}) vsync={}", cli.invert_x, cli.invert_y, cli.invert_z, cli.vsync),
                ];

                // Prepare glyph sections; only rebuild queue when text or scale factor changed
                let scale = gpu.hud_font_size * window_scale_factor;
                let pad = gpu.hud_padding * window_scale_factor;
                let mut y_cursor = pad;
                let hud_changed = gpu.cached_hud_text != hud_lines || (gpu.cached_scale_factor - scale).abs() > f32::EPSILON;
                if gpu.hud_enabled && hud_changed {
                    if let Some(gb) = gpu.glyph_brush.as_mut() {
                        // Clear by rebuilding queue: wgpu_glyph queues are per-frame; we simply re-queue
                        for line in &hud_lines {
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
                    gpu.cached_hud_text = hud_lines.clone();
                    gpu.cached_scale_factor = scale;
                } else {
                    // Keep y_cursor correct for later elements even if we didn't rebuild
                    y_cursor += (scale + 4.0) * 5.0;
                }

                // Calibrate button
                let btn_text = "Calibrate";
                let btn_w = 110.0 * window_scale_factor;
                let btn_h = 28.0 * window_scale_factor;
                let btn_x = (gpu.config.width as f32) - btn_w - pad;
                let btn_y = pad;
                gpu.calibrate_btn = Rect { x: btn_x, y: btn_y, w: btn_w, h: btn_h };

                if gpu.hud_enabled && hud_changed {
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
                // Present the frame (present() does not return a Result on wgpu 0.17)
                frame.present();
            }
            _ => {}
        }
    });
    // Unreachable with winit 0.28 run loop, but return Ok for signature completeness.
    // Ok(())
}