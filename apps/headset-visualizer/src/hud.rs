//! Where the HUD puts its text and buttons, and queuing that text for drawing.
//!
//! `layout` works in physical pixels, the same space the window reports mouse positions in,
//! so `Rect::contains` hit-tests clicks directly. Buttons are sized from their labels as the
//! HUD font draws them (`LabelMetrics`, measured once per font size), so they still cover
//! their text with a large `--hud-size`.
//!
//! `queue_hud` queues everything but the centred capture hint, which `main.rs` adds. The tests
//! draw it offscreen on the real GPU and check that the text lands inside the layout.

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use wgpu_glyph::{GlyphBrush, Section, Text};

/// The HUD font, Noto Sans (SIL OFL 1.1, see `assets/OFL.txt`).
pub static FONT_BYTES: &[u8] = include_bytes!("../assets/NotoSans-Regular.ttf");

pub const CALIBRATE_LABEL: &str = "Calibrate";
pub const SETTINGS_LABEL: &str = "Screen Recording settings (P)";
pub const TOAST_LABEL: &str = "Calibrated";

pub const TEXT_COLOR: [f32; 4] = [0.95, 0.95, 0.97, 1.0];
pub const CALIBRATE_COLOR: [f32; 4] = [0.9, 0.9, 0.2, 1.0];
pub const TOAST_COLOR: [f32; 4] = [0.2, 0.9, 0.3, 1.0];
pub const SETTINGS_COLOR: [f32; 4] = [0.4, 0.75, 1.0, 1.0];

/// In points, multiplied by the window's scale factor: the gap around the HUD, the smallest
/// button, the space between a button's edge and its label, and between the two buttons.
const PADDING: f32 = 10.0;
const MIN_BUTTON: (f32, f32) = (110.0, 28.0);
const BUTTON_INSET: (f32, f32) = (10.0, 6.0);
const BUTTON_GAP: f32 = 6.0;
/// Pixels between lines of HUD text.
const LINE_GAP: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }

    #[cfg(test)]
    fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.x + other.w && other.x < self.x + self.w && self.y < other.y + other.h && other.y < self.y + self.h
    }
}

/// How big the button labels are at one font size, in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabelMetrics {
    pub font_px: f32,
    pub calibrate_w: f32,
    pub settings_w: f32,
    /// Height of a line of text (ascent to descent).
    pub line_h: f32,
}

impl LabelMetrics {
    pub fn measure(font: &FontArc, font_px: f32) -> Self {
        let scaled = font.as_scaled(PxScale::from(font_px));
        Self {
            font_px,
            calibrate_w: text_width(font, CALIBRATE_LABEL, font_px),
            settings_w: text_width(font, SETTINGS_LABEL, font_px),
            line_h: scaled.ascent() - scaled.descent(),
        }
    }
}

/// Width of `text` on one line at `font_px`, as glyph_brush lays it out: advances plus kerning.
pub fn text_width(font: &FontArc, text: &str, font_px: f32) -> f32 {
    let scaled = font.as_scaled(PxScale::from(font_px));
    let mut width = 0.0;
    let mut previous = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(prev) = previous {
            width += scaled.kern(prev, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

/// Where everything goes, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HudLayout {
    pub font_px: f32,
    pub pad: f32,
    pub line_step: f32,
    /// Top-right; always there (it's how you recenter with the mouse).
    pub calibrate: Rect,
    pub calibrate_text: (f32, f32),
    /// Under Calibrate, only while capture needs Screen Recording permission or failed.
    pub settings: Option<Rect>,
    pub settings_text: (f32, f32),
}

impl HudLayout {
    /// Top of text line `i` (0 = first).
    pub fn line_y(&self, i: usize) -> f32 {
        self.pad + i as f32 * self.line_step
    }

    /// Top of the "Calibrated" toast, under `lines` lines of text.
    pub fn toast_y(&self, lines: usize) -> f32 {
        self.line_y(lines) + 8.0
    }
}

/// Lay out the HUD for a window `window_w` pixels wide at `scale_factor` pixels per point.
pub fn layout(window_w: u32, scale_factor: f32, metrics: &LabelMetrics, show_settings: bool) -> HudLayout {
    let sf = scale_factor;
    let pad = PADDING * sf;
    let button = |label_w: f32, y: f32| {
        let w = (label_w + 2.0 * BUTTON_INSET.0 * sf).max(MIN_BUTTON.0 * sf);
        let h = (metrics.line_h + 2.0 * BUTTON_INSET.1 * sf).max(MIN_BUTTON.1 * sf);
        let rect = Rect { x: window_w as f32 - w - pad, y, w, h };
        // The label centred in the button.
        let text = (rect.x + (w - label_w) / 2.0, rect.y + (h - metrics.line_h) / 2.0);
        (rect, text)
    };
    let (calibrate, calibrate_text) = button(metrics.calibrate_w, pad);
    let (settings, settings_text) = button(metrics.settings_w, calibrate.y + calibrate.h + BUTTON_GAP * sf);
    HudLayout {
        font_px: metrics.font_px,
        pad,
        line_step: metrics.font_px + LINE_GAP,
        calibrate,
        calibrate_text,
        settings: show_settings.then_some(settings),
        settings_text,
    }
}

/// Queue the HUD's text, the Calibrate button, the toast (if `toast`) and the settings button
/// (if the layout has one) in a `width` x `height` target.
pub fn queue_hud(gb: &mut GlyphBrush<()>, layout: &HudLayout, lines: &[String], toast: bool, width: u32, height: u32) {
    let bounds = (width as f32, height as f32);
    let mut text = |label: &str, at: (f32, f32), color: [f32; 4]| {
        gb.queue(Section {
            screen_position: at,
            bounds,
            text: vec![Text::new(label).with_color(color).with_scale(layout.font_px)],
            ..Section::default()
        });
    };
    for (i, line) in lines.iter().enumerate() {
        text(line, (layout.pad, layout.line_y(i)), TEXT_COLOR);
    }
    text(CALIBRATE_LABEL, layout.calibrate_text, CALIBRATE_COLOR);
    if toast {
        text(TOAST_LABEL, (layout.pad, layout.toast_y(lines.len())), TOAST_COLOR);
    }
    if layout.settings.is_some() {
        text(SETTINGS_LABEL, layout.settings_text, SETTINGS_COLOR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> FontArc {
        FontArc::try_from_slice(FONT_BYTES).expect("embedded font")
    }

    #[test]
    fn rects_contain_their_edges_and_nothing_outside() {
        let r = Rect { x: 10.0, y: 20.0, w: 30.0, h: 40.0 };
        assert!(r.contains(10.0, 20.0) && r.contains(40.0, 60.0) && r.contains(25.0, 40.0));
        assert!(!r.contains(9.9, 30.0) && !r.contains(40.1, 30.0) && !r.contains(20.0, 19.9) && !r.contains(20.0, 60.1));
    }

    #[test]
    fn buttons_cover_their_labels_at_any_size() {
        let font = font();
        for sf in [1.0, 2.0] {
            for hud_size in [8.0, 16.0, 32.0, 64.0] {
                let px = hud_size * sf;
                let m = LabelMetrics::measure(&font, px);
                let l = layout(3000, sf, &m, true);
                for (rect, text, label_w) in [
                    (l.calibrate, l.calibrate_text, m.calibrate_w),
                    (l.settings.unwrap(), l.settings_text, m.settings_w),
                ] {
                    let label = Rect { x: text.0, y: text.1, w: label_w, h: m.line_h };
                    let inside = rect.contains(label.x, label.y) && rect.contains(label.x + label.w, label.y + label.h);
                    assert!(inside, "--hud-size {hud_size} at {sf}x: label {label:?} outside button {rect:?}");
                }
            }
        }
    }

    #[test]
    fn the_calibrate_button_sits_top_right() {
        let m = LabelMetrics::measure(&font(), 16.0);
        let l = layout(800, 1.0, &m, false);
        assert_eq!(l.calibrate.y, 10.0);
        assert_eq!(l.calibrate.x + l.calibrate.w, 790.0, "10 px from the right edge");
        assert_eq!((l.calibrate.w, l.calibrate.h), (110.0, 28.0), "the usual size at the default font");
        assert_eq!(l.settings, None);

        // On a Retina window everything doubles.
        let m2 = LabelMetrics::measure(&font(), 32.0);
        let l2 = layout(1600, 2.0, &m2, false);
        assert_eq!(l2.calibrate.x, l.calibrate.x * 2.0);
        assert_eq!((l2.calibrate.w, l2.calibrate.h), (220.0, 56.0));
    }

    #[test]
    fn the_settings_button_is_under_calibrate_without_overlapping() {
        let m = LabelMetrics::measure(&font(), 16.0);
        let l = layout(800, 1.0, &m, true);
        let s = l.settings.unwrap();
        assert!(s.y >= l.calibrate.y + l.calibrate.h, "{s:?} under {:?}", l.calibrate);
        assert!(!s.overlaps(&l.calibrate));
        assert_eq!(s.x + s.w, l.calibrate.x + l.calibrate.w, "right-aligned with Calibrate");
        // A click on one is never a click on the other.
        let (cx, cy) = (l.calibrate.x + l.calibrate.w / 2.0, l.calibrate.y + l.calibrate.h / 2.0);
        assert!(l.calibrate.contains(cx, cy) && !s.contains(cx, cy));
    }

    #[test]
    fn text_lines_stack_and_the_toast_goes_under_them() {
        let m = LabelMetrics::measure(&font(), 16.0);
        let l = layout(800, 1.0, &m, false);
        assert_eq!((l.line_y(0), l.line_y(1), l.line_y(3)), (10.0, 30.0, 70.0));
        assert_eq!(l.toast_y(3), 78.0);
    }

    #[test]
    fn text_widths_grow_with_text_and_size() {
        let font = font();
        let w16 = text_width(&font, CALIBRATE_LABEL, 16.0);
        assert!(w16 > 40.0 && w16 < 110.0 - 20.0, "Calibrate at 16 px is {w16} px");
        assert!((text_width(&font, CALIBRATE_LABEL, 32.0) - 2.0 * w16).abs() < 0.5, "scales with size");
        assert!(text_width(&font, SETTINGS_LABEL, 16.0) > w16);
        assert_eq!(text_width(&font, "", 16.0), 0.0);
    }

    /// Draw the HUD offscreen and check the pixels: each label is drawn inside the box the
    /// layout gives it. Skipped where there's no GPU.
    #[test]
    fn the_hud_draws_inside_its_layout() {
        const W: u32 = 512; // 512 * 4 bytes per row is a multiple of 256, as readback needs
        const H: u32 = 160;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let Some(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            eprintln!("no GPU adapter; skipping HUD render test");
            return;
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None)).unwrap();
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let font = font();
        let mut gb = wgpu_glyph::GlyphBrushBuilder::using_font(font.clone()).build(&device, format);
        let mut belt = wgpu::util::StagingBelt::new(1024);

        // A large font at 2x, where the old fixed-size button was too small for its label.
        let m = LabelMetrics::measure(&font, 24.0 * 2.0);
        let l = layout(W, 2.0, &m, true);
        let lines = vec!["fps: 120.0".to_string()];
        queue_hud(&mut gb, &l, &lines, true, W, H);

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("hud-test-target"),
            size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("hud-test-readback"),
            size: u64::from(W * H * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("hud-test-clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: true },
            })],
            depth_stencil_attachment: None,
        });
        gb.draw_queued(&device, &mut belt, &mut encoder, &view, W, H).unwrap();
        belt.finish();
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(W * 4), rows_per_image: Some(H) },
            },
            wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        );
        queue.submit(Some(encoder.finish()));
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.unwrap());
        device.poll(wgpu::Maintain::Wait);
        let px: Vec<[u8; 4]> = slice.get_mapped_range().chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]]).collect();

        // Each channel over the brightest one: the same for a colour and its darker,
        // antialiased edges.
        let hue = |c: [f32; 3]| {
            let max = c.iter().copied().fold(f32::MIN, f32::max);
            c.map(|v| v / max)
        };
        // The bounding box of the pixels drawn in `color` (the target is BGRA).
        let drawn_in = |color: [f32; 4]| {
            let want = hue([color[2], color[1], color[0]]);
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for y in 0..H {
                for x in 0..W {
                    let p = px[(y * W + x) as usize];
                    let rgb = [f32::from(p[0]), f32::from(p[1]), f32::from(p[2])];
                    let bright = rgb.iter().copied().fold(0.0, f32::max) > 120.0;
                    let close = bright && hue(rgb).iter().zip(want).all(|(a, b)| (a - b).abs() < 0.15);
                    if close {
                        (x0, y0, x1, y1) = (x0.min(x as f32), y0.min(y as f32), x1.max(x as f32), y1.max(y as f32));
                    }
                }
            }
            assert!(x0 <= x1, "nothing drawn in {color:?}");
            Rect { x: x0, y: y0, w: x1 - x0 + 1.0, h: y1 - y0 + 1.0 }
        };
        let inside = |what: &str, drawn: Rect, rect: Rect| {
            let ok = rect.contains(drawn.x, drawn.y) && rect.contains(drawn.x + drawn.w - 1.0, drawn.y + drawn.h - 1.0);
            assert!(ok, "{what} drawn at {drawn:?}, outside {rect:?}");
        };
        inside("Calibrate", drawn_in(CALIBRATE_COLOR), l.calibrate);
        inside("Screen Recording settings", drawn_in(SETTINGS_COLOR), l.settings.unwrap());
        let line_band = Rect { x: 0.0, y: l.line_y(0), w: l.calibrate.x, h: m.line_h };
        inside("the first line", drawn_in(TEXT_COLOR), line_band);
        let toast_band = Rect { x: 0.0, y: l.toast_y(1), w: l.calibrate.x, h: m.line_h };
        inside("the toast", drawn_in(TOAST_COLOR), toast_band);
    }
}
