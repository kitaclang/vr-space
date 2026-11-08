//! The 3D scene: the floor grid and the virtual screen showing the captured frames.
//!
//! `Scene` only needs a wgpu device, queue and target format, not a window, so the tests at
//! the end of this file render it into an offscreen texture and check the pixels. The window,
//! surface and HUD stay in `main.rs`.
//!
//! Per frame the render loop calls `set_view_proj`, then `show_external` or `upload_frame`
//! when a new frame arrived, and `encode`. `show_external` shows a texture the GPU reads in
//! place (an IOSurface on macOS, see `iosurface.rs`); `upload_frame` copies pixels from the
//! CPU. None of them allocate or block once warmed up: textures are only created when the
//! frame size changes, or the first time each of the capture's few surfaces is seen.

use glam::Mat4;
use log::{info, warn};
use wgpu::util::DeviceExt;

use glam::Vec3;

use crate::camera::CAM_POS;
use crate::cli::Cli;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 3],
    pub color: [f32; 3],
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

/// A vertex of the textured screen quad.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TVertex {
    pub pos: [f32; 3],
    pub uv: [f32; 2],
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

/// What's behind the grid and the screen.
pub const BACKGROUND: wgpu::Color = wgpu::Color { r: 0.02, g: 0.02, b: 0.03, a: 1.0 };

/// Aspect ratio of the screen quad before the first frame arrives.
const PLACEHOLDER_ASPECT: f32 = 1.6;

const QUAD_SHADER: &str = r#"
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

/// What `upload_frame` or `show_external` did with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameUpload {
    /// Copied into the existing screen texture.
    Copied,
    /// Shown in place from a texture imported earlier: nothing copied or created.
    Shared,
    /// Shown in place from a newly imported texture (the first time this surface was seen).
    Imported,
    /// The frame size changed: the texture was recreated (or imported) and the quad resized.
    Resized,
    /// Too large for the GPU, or the buffer is too short for its size: not shown.
    Rejected,
    /// `show_external` couldn't import the texture; nothing changed. Copy the frame instead.
    NotImported,
}

/// How many imported textures to keep. ScreenCaptureKit cycles through a few surfaces (about
/// its queue depth), so this holds them all; going over it means the surfaces changed (a new
/// stream), and the old ones are dropped.
const MAX_EXTERNAL: usize = 8;

/// A texture the GPU reads in place, with the bind group that draws it.
struct External {
    key: u64,
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// Which texture the screen quad draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bound {
    /// `Scene::tex`, filled by `upload_frame`.
    Own,
    /// `Scene::external[i]`.
    External(usize),
}

pub struct Scene {
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,

    // The grid never changes, so it's recorded once. The pipeline and vertex buffer are kept
    // alongside the bundle that uses them.
    grid_bundle: wgpu::RenderBundle,
    _grid_pipeline: wgpu::RenderPipeline,
    _grid_vertices: wgpu::Buffer,

    quad_pipeline: wgpu::RenderPipeline,
    quad_vb: wgpu::Buffer,
    quad_ib: wgpu::Buffer,
    quad_index_count: u32,
    quad_bgl: wgpu::BindGroupLayout,
    quad_sampler: wgpu::Sampler,
    // Screen texture for copied frames; recreated (rarely) when the frame size changes.
    tex: wgpu::Texture,
    tex_own_size: (u32, u32),
    quad_bind_group: wgpu::BindGroup,
    // Imported textures, by key, all of size `tex_size`; and which texture is drawn.
    external: Vec<External>,
    bound: Bound,
    /// Size of the last frame shown, `(1, 1)` before the first, `(0, 0)` after one was rejected.
    pub tex_size: (u32, u32),
    pub screen: ScreenPlacement,
}

impl Scene {
    /// Build the scene for render targets of `format`. The screen starts black at a 16:10
    /// aspect ratio until the first frame is uploaded.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, grid: &[Vertex], screen: ScreenPlacement) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grid-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let uniforms = Uniforms { view_proj: Mat4::IDENTITY.to_cols_array_2d() };
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("uniform-buffer"),
            contents: bytemuck::bytes_of(&uniforms),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
            layout: &uniform_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform_buffer.as_entire_binding() }],
        });

        // Grid
        let grid_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline-layout"),
            bind_group_layouts: &[&uniform_bgl],
            push_constant_ranges: &[],
        });
        let grid_vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("grid-vertices"),
            contents: bytemuck::cast_slice(grid),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let grid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("grid-pipeline"),
            layout: Some(&grid_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: "vs_main", buffers: &[Vertex::desc()] },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::LineList, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });
        let grid_bundle = {
            let mut encoder = device.create_render_bundle_encoder(&wgpu::RenderBundleEncoderDescriptor {
                label: Some("grid-bundle-encoder"),
                color_formats: &[Some(format)],
                depth_stencil: None,
                sample_count: 1,
                multiview: None,
            });
            encoder.set_pipeline(&grid_pipeline);
            encoder.set_bind_group(0, &uniform_bind_group, &[]);
            encoder.set_vertex_buffer(0, grid_vertices.slice(..));
            encoder.draw(0..grid.len() as u32, 0..1);
            encoder.finish(&wgpu::RenderBundleDescriptor { label: Some("grid-bundle") })
        };

        // Screen quad
        let quad_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad-shader"),
            source: wgpu::ShaderSource::Wgsl(QUAD_SHADER.into()),
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
        let quad_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad-pl"),
            bind_group_layouts: &[&uniform_bgl, &quad_bgl],
            push_constant_ranges: &[],
        });
        let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad-pipeline"),
            layout: Some(&quad_layout),
            vertex: wgpu::VertexState { module: &quad_shader, entry_point: "vs_main_q", buffers: &[TVertex::desc()] },
            fragment: Some(wgpu::FragmentState {
                module: &quad_shader,
                entry_point: "fs_main_q",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        // Placeholder 1x1 screen texture (zeroed, so black) until the first frame sets the size.
        let tex_size = (1u32, 1u32);
        let (tex, quad_bind_group) = create_screen_texture(device, &quad_bgl, &quad_sampler, tex_size.0, tex_size.1);

        let quad_indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let quad_vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-vb"),
            contents: bytemuck::cast_slice(&quad_vertices(screen, PLACEHOLDER_ASPECT)),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });
        let quad_ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-ib"),
            contents: bytemuck::cast_slice(&quad_indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        Self {
            uniform_buffer,
            uniform_bind_group,
            grid_bundle,
            _grid_pipeline: grid_pipeline,
            _grid_vertices: grid_vertices,
            quad_pipeline,
            quad_vb,
            quad_ib,
            quad_index_count: quad_indices.len() as u32,
            quad_bgl,
            quad_sampler,
            tex,
            tex_own_size: tex_size,
            quad_bind_group,
            external: Vec::with_capacity(MAX_EXTERNAL),
            bound: Bound::Own,
            tex_size,
            screen,
        }
    }

    /// Set the camera for the next `encode`.
    pub fn set_view_proj(&self, queue: &wgpu::Queue, view_proj: Mat4) {
        let uniforms = Uniforms { view_proj: view_proj.to_cols_array_2d() };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
    }

    /// Rewrite the screen quad after its placement or the frame's aspect ratio changed.
    pub fn write_quad(&self, queue: &wgpu::Queue) {
        let (w, h) = self.tex_size;
        let aspect = if w > 0 && h > 0 { w as f32 / h as f32 } else { PLACEHOLDER_ASPECT };
        let verts = quad_vertices(self.screen, aspect);
        queue.write_buffer(&self.quad_vb, 0, bytemuck::cast_slice(&verts));
    }

    /// Show a BGRA frame of `width` x `height` pixels whose rows are `bytes_per_row` apart in
    /// `data`. A new size recreates the texture and resizes the quad to the frame's aspect.
    pub fn upload_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        bytes_per_row: u32,
        data: &[u8],
    ) -> FrameUpload {
        let len = bytes_per_row as usize * height as usize;
        let buffer_ok = bytes_per_row as u64 >= width as u64 * 4 && data.len() >= len;
        if !self.check_fits(device, width, height, buffer_ok) {
            return FrameUpload::Rejected;
        }
        if self.tex_own_size != (width, height) {
            // Rare: first frame, or the display or window size changed.
            let (tex, bg) = create_screen_texture(device, &self.quad_bgl, &self.quad_sampler, width, height);
            self.tex = tex;
            self.quad_bind_group = bg;
            self.tex_own_size = (width, height);
        }
        self.bound = Bound::Own;
        let resized = self.set_shown_size(queue, width, height);
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &data[..len],
            wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(height) },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        if resized {
            FrameUpload::Resized
        } else {
            FrameUpload::Copied
        }
    }

    /// Show a `width` x `height` BGRA texture that the GPU reads in place, identified by `key`.
    /// The first time a key is seen (at this size), `import` makes its texture, which is then
    /// kept, so later frames from the same surface just switch to it. If `import` fails,
    /// nothing changes and `NotImported` says to copy the frame instead.
    ///
    /// The texture must stay valid to sample, and its contents unchanged, until the GPU has
    /// drawn the frame: that's the caller's job (on macOS, keeping the frame's pixel buffer).
    pub fn show_external(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: u64,
        width: u32,
        height: u32,
        import: impl FnOnce() -> Option<wgpu::Texture>,
    ) -> FrameUpload {
        if !self.check_fits(device, width, height, true) {
            return FrameUpload::Rejected;
        }
        let same_size = self.tex_size == (width, height);
        let cached = if same_size { self.external.iter().position(|e| e.key == key) } else { None };
        let (index, imported) = match cached {
            Some(i) => (i, false),
            None => {
                let Some(texture) = import() else { return FrameUpload::NotImported };
                if !same_size || self.external.len() >= MAX_EXTERNAL {
                    self.external.clear();
                }
                let bind_group = create_bind_group(device, &self.quad_bgl, &self.quad_sampler, &texture);
                self.external.push(External { key, _texture: texture, bind_group });
                (self.external.len() - 1, true)
            }
        };
        self.bound = Bound::External(index);
        if self.set_shown_size(queue, width, height) {
            FrameUpload::Resized
        } else if imported {
            FrameUpload::Imported
        } else {
            FrameUpload::Shared
        }
    }

    /// Whether a frame fits the GPU's texture limit (and its buffer is long enough). Logs once
    /// when one doesn't, until one fits again.
    fn check_fits(&mut self, device: &wgpu::Device, width: u32, height: u32, buffer_ok: bool) -> bool {
        let max_dim = device.limits().max_texture_dimension_2d;
        let fits = width > 0 && height > 0 && width <= max_dim && height <= max_dim && buffer_ok;
        if !fits && self.tex_size != (0, 0) {
            warn!("Captured frame {width}x{height} doesn't fit the GPU texture limit {max_dim} or its buffer; not displayed");
            self.tex_size = (0, 0);
        }
        fits
    }

    /// Record the size of the frame now shown; resize the quad and return true if it changed.
    /// Imported textures of the old size are dropped then (they can't be shown again).
    fn set_shown_size(&mut self, queue: &wgpu::Queue, width: u32, height: u32) -> bool {
        if self.tex_size == (width, height) {
            return false;
        }
        info!("Screen texture resized to {width}x{height}");
        if self.bound == Bound::Own {
            self.external.clear();
        }
        self.tex_size = (width, height);
        self.write_quad(queue);
        true
    }

    /// Clear `target` and draw the grid, then the screen on top.
    pub fn encode(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView) {
        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(BACKGROUND), store: true },
            })],
            depth_stencil_attachment: None,
        });
        rpass.execute_bundles(std::iter::once(&self.grid_bundle));
        rpass.set_pipeline(&self.quad_pipeline);
        rpass.set_bind_group(0, &self.uniform_bind_group, &[]);
        let bind_group = match self.bound {
            Bound::Own => &self.quad_bind_group,
            Bound::External(i) => &self.external[i].bind_group,
        };
        rpass.set_bind_group(1, bind_group, &[]);
        rpass.set_vertex_buffer(0, self.quad_vb.slice(..));
        rpass.set_index_buffer(self.quad_ib.slice(..), wgpu::IndexFormat::Uint16);
        rpass.draw_indexed(0..self.quad_index_count, 0, 0..1);
    }
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
    let bind_group = create_bind_group(device, bgl, sampler, &tex);
    (tex, bind_group)
}

/// The bind group that draws `tex` on the screen quad.
fn create_bind_group(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    tex: &wgpu::Texture,
) -> wgpu::BindGroup {
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("quad-bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
        ],
    })
}

// ================= Screen placement and grid =================

/// Where the virtual screen sits: straight ahead of the eye at the calibrated "forward".
/// Its width follows the captured screen's aspect ratio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenPlacement {
    pub height: f32,
    pub distance: f32,
}

/// Each press of a distance or size key changes it by this factor, so steps feel even
/// whether the screen is near or far.
pub const SCREEN_STEP: f32 = 1.1;
pub const SCREEN_DISTANCE_RANGE: (f32, f32) = (0.5, 50.0);
pub const SCREEN_HEIGHT_RANGE: (f32, f32) = (0.1, 30.0);

impl ScreenPlacement {
    pub fn from_cli(cli: &Cli) -> Self {
        Self { height: cli.screen_height, distance: cli.screen_distance }
    }

    /// Scale the distance (`factor` > 1 moves the screen away). The size stays the same,
    /// so a closer screen looks bigger, as a real one would.
    pub fn scale_distance(&mut self, factor: f32) {
        let (lo, hi) = SCREEN_DISTANCE_RANGE;
        self.distance = (self.distance * factor).clamp(lo.min(self.distance), hi.max(self.distance));
    }

    pub fn scale_height(&mut self, factor: f32) {
        let (lo, hi) = SCREEN_HEIGHT_RANGE;
        self.height = (self.height * factor).clamp(lo.min(self.height), hi.max(self.height));
    }
}

pub fn quad_vertices(screen: ScreenPlacement, aspect: f32) -> [TVertex; 4] {
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

pub fn build_grid_vertices(size: i32, step: i32) -> Vec<Vertex> {
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

#[cfg(test)]
mod tests {
    //! Render the scene offscreen on the real GPU and check the pixels. Skipped (with a note)
    //! where there's no GPU adapter, such as some CI machines.

    use super::*;
    use crate::camera::{camera_view, make_proj, CAM_POS};
    use glam::Quat;

    const SIZE: u32 = 64; // 64 * 4 bytes = 256, the row alignment copy_texture_to_buffer needs
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
    const RED: [u8; 4] = [0, 0, 255, 255]; // BGRA
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [255, 0, 0, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    // Not used by the grid (its axes are red, green and blue), so it's only ever the screen.
    const MAGENTA: [u8; 4] = [255, 0, 255, 255];

    struct Harness {
        device: wgpu::Device,
        queue: wgpu::Queue,
        scene: Scene,
    }

    impl Harness {
        fn new(screen: ScreenPlacement) -> Option<Self> {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()));
            let Some(adapter) = adapter else {
                eprintln!("no GPU adapter; skipping render test");
                return None;
            };
            let (device, queue) = pollster::block_on(adapter.request_device(
                &wgpu::DeviceDescriptor { label: None, features: wgpu::Features::empty(), limits: adapter.limits() },
                None,
            ))
            .ok()?;
            let scene = Scene::new(&device, FORMAT, &build_grid_vertices(20, 1), screen);
            Some(Self { device, queue, scene })
        }

        fn upload(&mut self, width: u32, height: u32, bytes_per_row: u32, data: &[u8]) -> FrameUpload {
            self.scene.upload_frame(&self.device, &self.queue, width, height, bytes_per_row, data)
        }

        /// Render looking along `head` (no neck model, 60° vertical field of view, square
        /// target) and return the BGRA pixels, top row first.
        fn render(&self, head: Quat) -> Vec<[u8; 4]> {
            let vp = make_proj(SIZE, SIZE, 60.0) * camera_view(head, 0.0);
            self.scene.set_view_proj(&self.queue, vp);
            let target = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("test-target"),
                size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("test-readback"),
                size: (SIZE * SIZE * 4) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let view = target.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            self.scene.encode(&mut encoder, &view);
            encoder.copy_texture_to_buffer(
                target.as_image_copy(),
                wgpu::ImageCopyBuffer {
                    buffer: &readback,
                    layout: wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(SIZE * 4), rows_per_image: Some(SIZE) },
                },
                wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
            );
            self.queue.submit(Some(encoder.finish()));
            let slice = readback.slice(..);
            slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback buffer"));
            self.device.poll(wgpu::Maintain::Wait);
            let pixels = slice.get_mapped_range().chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]]).collect();
            readback.unmap();
            pixels
        }
    }

    fn at(pixels: &[[u8; 4]], x: u32, y: u32) -> [u8; 4] {
        pixels[(y * SIZE + x) as usize]
    }

    fn near(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 8)
    }

    /// A `width` x `height` BGRA frame of one colour, rows packed.
    fn solid(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        color.repeat((width * height) as usize)
    }

    /// The bounding box `(width, height)` in pixels of everything drawn in `color`.
    fn extent_of(pixels: &[[u8; 4]], color: [u8; 4]) -> (u32, u32) {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for y in 0..SIZE {
            for x in 0..SIZE {
                if near(at(pixels, x, y), color) {
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                }
            }
        }
        assert!(x0 <= x1, "nothing drawn in {color:?}");
        (x1 - x0 + 1, y1 - y0 + 1)
    }

    const AHEAD: ScreenPlacement = ScreenPlacement { height: 1.0, distance: 2.0 };

    #[test]
    fn the_screen_is_black_until_the_first_frame() {
        let Some(h) = Harness::new(AHEAD) else { return };
        let px = h.render(Quat::IDENTITY);
        assert_eq!(at(&px, SIZE / 2, SIZE / 2), [0, 0, 0, 255]);
        // The corners show the background, not the screen.
        let bg = [(BACKGROUND.b * 255.0) as u8, (BACKGROUND.g * 255.0) as u8, (BACKGROUND.r * 255.0) as u8, 255];
        assert!(near(at(&px, 0, 0), bg), "{:?}", at(&px, 0, 0));
    }

    #[test]
    fn frames_show_upright_and_not_mirrored() {
        let Some(mut h) = Harness::new(AHEAD) else { return };
        // A 4x4 frame in quarters: red, green on top; blue, white below.
        let row = |l: [u8; 4], r: [u8; 4]| [l, l, r, r].concat();
        let frame = [row(RED, GREEN), row(RED, GREEN), row(BLUE, WHITE), row(BLUE, WHITE)].concat();
        assert_eq!(h.upload(4, 4, 16, &frame), FrameUpload::Resized);
        let px = h.render(Quat::IDENTITY);
        let (c, d) = (SIZE / 2, 8);
        assert!(near(at(&px, c - d, c - d), RED), "top left {:?}", at(&px, c - d, c - d));
        assert!(near(at(&px, c + d, c - d), GREEN), "top right {:?}", at(&px, c + d, c - d));
        assert!(near(at(&px, c - d, c + d), BLUE), "bottom left {:?}", at(&px, c - d, c + d));
        assert!(near(at(&px, c + d, c + d), WHITE), "bottom right {:?}", at(&px, c + d, c + d));
    }

    #[test]
    fn the_screen_takes_the_frames_aspect_ratio() {
        let small = ScreenPlacement { height: 0.5, distance: 2.0 };
        let Some(mut h) = Harness::new(small) else { return };
        assert_eq!(h.upload(64, 16, 256, &solid(64, 16, MAGENTA)), FrameUpload::Resized);
        let (w, ht) = extent_of(&h.render(Quat::IDENTITY), MAGENTA);
        let aspect = w as f32 / ht as f32;
        assert!((aspect - 4.0).abs() < 0.4, "drawn {w}x{ht}, aspect {aspect}");

        // The same size again is a plain copy; a new size resizes the quad.
        assert_eq!(h.upload(64, 16, 256, &solid(64, 16, GREEN)), FrameUpload::Copied);
        assert_eq!(h.upload(16, 16, 64, &solid(16, 16, MAGENTA)), FrameUpload::Resized);
        let (w, ht) = extent_of(&h.render(Quat::IDENTITY), MAGENTA);
        assert!(w.abs_diff(ht) <= 2, "drawn {w}x{ht}, expected square");
    }

    #[test]
    fn padded_rows_are_read_with_their_stride() {
        let Some(mut h) = Harness::new(AHEAD) else { return };
        // 8x2 frame, red on the left and green on the right, with 16 bytes of padding per
        // row (as ScreenCaptureKit often has). Read without the stride, row 2 would shift.
        let row = [[RED; 4], [GREEN; 4], [[9; 4]; 4]].concat().concat();
        let frame = [row.clone(), row].concat();
        assert_eq!(h.upload(8, 2, 48, &frame), FrameUpload::Resized);
        let px = h.render(Quat::IDENTITY);
        let c = SIZE / 2;
        assert!(near(at(&px, c - 8, c), RED), "{:?}", at(&px, c - 8, c));
        assert!(near(at(&px, c + 8, c), GREEN), "{:?}", at(&px, c + 8, c));
    }

    #[test]
    fn turning_left_moves_the_screen_right() {
        let Some(mut h) = Harness::new(AHEAD) else { return };
        h.upload(16, 10, 64, &solid(16, 10, RED));
        let ahead = h.render(Quat::IDENTITY);
        assert!(near(at(&ahead, SIZE / 2, SIZE / 2), RED));

        // Positive rotation about +Y turns the view from -Z towards -X, i.e. to the left. The
        // screen (0.8 m half-width at 2 m, about 22°) is then entirely right of centre.
        let turned = h.render(Quat::from_rotation_y(0.45));
        assert!(!near(at(&turned, SIZE / 2 - 2, SIZE / 2), RED), "screen still ahead");
        assert!(near(at(&turned, SIZE * 3 / 4, SIZE / 2), RED), "screen not on the right");

        // Turned all the way round, it's out of view.
        let behind = h.render(Quat::from_rotation_y(std::f32::consts::PI));
        assert!(behind.iter().all(|&p| !near(p, RED)), "screen visible behind you");
    }

    #[test]
    fn frames_that_dont_fit_are_rejected_and_the_next_one_shows() {
        let Some(mut h) = Harness::new(AHEAD) else { return };
        let too_wide = h.device.limits().max_texture_dimension_2d + 1;
        assert_eq!(h.upload(too_wide, 1, too_wide * 4, &[]), FrameUpload::Rejected);
        assert_eq!(h.scene.tex_size, (0, 0));
        // A buffer shorter than its rows, or rows narrower than the width, are rejected too.
        assert_eq!(h.upload(4, 4, 16, &solid(4, 3, RED)), FrameUpload::Rejected);
        assert_eq!(h.upload(4, 4, 8, &solid(4, 4, RED)), FrameUpload::Rejected);
        assert_eq!(h.upload(0, 4, 16, &[]), FrameUpload::Rejected);

        assert_eq!(h.upload(4, 4, 16, &solid(4, 4, RED)), FrameUpload::Resized);
        assert!(near(at(&h.render(Quat::IDENTITY), SIZE / 2, SIZE / 2), RED));
    }

    #[test]
    fn moving_the_screen_closer_makes_it_bigger() {
        let Some(mut h) = Harness::new(ScreenPlacement { height: 0.5, distance: 4.0 }) else { return };
        h.upload(16, 10, 64, &solid(16, 10, MAGENTA));
        let (far_w, _) = extent_of(&h.render(Quat::IDENTITY), MAGENTA);
        h.scene.screen.scale_distance(0.5);
        h.scene.write_quad(&h.queue);
        let (near_w, _) = extent_of(&h.render(Quat::IDENTITY), MAGENTA);
        assert!(near_w.abs_diff(far_w * 2) <= 2, "{far_w} px at 4 m, {near_w} px at 2 m");
    }

    /// An ordinary texture of one colour, standing in for one the GPU reads in place.
    fn solid_texture(h: &Harness, width: u32, height: u32, color: [u8; 4]) -> wgpu::Texture {
        let tex = h.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test-external"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        h.queue.write_texture(
            tex.as_image_copy(),
            &solid(width, height, color),
            wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(width * 4), rows_per_image: Some(height) },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        tex
    }

    #[test]
    fn external_textures_are_imported_once_per_key() {
        let Some(mut h) = Harness::new(AHEAD) else { return };
        let center = |h: &Harness| at(&h.render(Quat::IDENTITY), SIZE / 2, SIZE / 2);
        let (red, green) = (solid_texture(&h, 16, 10, RED), solid_texture(&h, 16, 10, GREEN));
        let mut red = Some(red);
        let mut green = Some(green);

        assert_eq!(h.scene.show_external(&h.device, &h.queue, 1, 16, 10, || red.take()), FrameUpload::Resized);
        assert!(near(center(&h), RED));
        assert_eq!(h.scene.show_external(&h.device, &h.queue, 2, 16, 10, || green.take()), FrameUpload::Imported);
        assert!(near(center(&h), GREEN));

        // Seen before: shown again without importing anything.
        let not_called = || panic!("imported a texture that was already kept");
        assert_eq!(h.scene.show_external(&h.device, &h.queue, 1, 16, 10, not_called), FrameUpload::Shared);
        assert!(near(center(&h), RED));

        // A failed import changes nothing, and copying the frame instead works.
        assert_eq!(h.scene.show_external(&h.device, &h.queue, 3, 16, 10, || None), FrameUpload::NotImported);
        assert!(near(center(&h), RED));
        // Same size as shown, so the quad keeps its shape, though the copy's texture is new.
        assert_eq!(h.upload(16, 10, 64, &solid(16, 10, BLUE)), FrameUpload::Copied);
        assert!(near(center(&h), BLUE));

        // A new size: the old textures are dropped, so key 1 is imported again.
        let wide = solid_texture(&h, 32, 10, MAGENTA);
        let mut wide = Some(wide);
        assert_eq!(h.scene.show_external(&h.device, &h.queue, 1, 32, 10, || wide.take()), FrameUpload::Resized);
        assert!(near(center(&h), MAGENTA));
        assert_eq!(h.scene.tex_size, (32, 10));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn iosurfaces_are_shown_in_place_without_copying() {
        use crate::iosurface::{self, test_surface::TestSurface};
        let Some(mut h) = Harness::new(AHEAD) else { return };
        if !iosurface::device_supports_import(&h.device) {
            eprintln!("GPU isn't Metal; skipping IOSurface test");
            return;
        }
        let center = |h: &Harness| at(&h.render(Quat::IDENTITY), SIZE / 2, SIZE / 2);
        let surface = TestSurface::bgra(16, 10);
        surface.fill(RED);
        let key = u64::from(surface.id());
        let import = |s: &TestSurface| unsafe { iosurface::import(&h.device, s.surface, s.width, s.height) };

        let tex = import(&surface).expect("import a BGRA IOSurface");
        let mut tex = Some(tex);
        assert_eq!(h.scene.show_external(&h.device, &h.queue, key, 16, 10, || tex.take()), FrameUpload::Resized);
        assert!(near(center(&h), RED), "{:?}", center(&h));

        // The capture writes its next frame into the same surface. Showing that surface again
        // reuses the texture, and the GPU sees the new pixels: nothing was copied.
        surface.fill(GREEN);
        assert_eq!(h.scene.show_external(&h.device, &h.queue, key, 16, 10, || None), FrameUpload::Shared);
        assert!(near(center(&h), GREEN), "{:?}", center(&h));

        // Surfaces that don't match are refused rather than shown wrongly.
        assert!(unsafe { iosurface::import(&h.device, surface.surface, 8, 10) }.is_none(), "wrong size");
        let yuv = TestSurface::new(16, 10, u32::from_be_bytes(*b"420v"));
        assert!(import(&yuv).is_none(), "not BGRA");
    }

    // --- Screen placement ---

    const EPS: f32 = 1e-4;

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
}
