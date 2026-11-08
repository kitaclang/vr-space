//! The app's icons, drawn in code so there's no image asset to license: the glasses shape
//! shared by the menu bar icon and the app icon, and the app icon itself (a virtual screen
//! floating above a pair of glasses), written as a macOS `.iconset` folder for
//! `scripts/bundle-macos.sh`.

use std::path::Path;

/// Signed distance from (x, y) to a rounded rectangle centred at (cx, cy) with half-size
/// (hw, hh) and corner radius r. Negative inside.
pub fn rounded_rect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - (hw - r);
    let qy = (y - cy).abs() - (hh - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// Size of the canvas the glasses are designed on (the menu bar icon's pixels).
pub const GLASSES_SIZE: (f32, f32) = (80.0, 44.0);

/// Distance to the glasses outline (hollow lenses, bridge and top bar) on the 80x44 canvas.
/// Also returns the distance to the lens openings, for filling them.
pub fn glasses(x: f32, y: f32) -> (f32, f32) {
    let lenses = rounded_rect(x, y, 24.0, 24.0, 15.0, 10.0, 7.0).min(rounded_rect(x, y, 56.0, 24.0, 15.0, 10.0, 7.0));
    // Hollow lenses: keep a 3.5 px rim.
    let rims = lenses.max(-(lenses + 3.5));
    let bridge = rounded_rect(x, y, 40.0, 19.0, 3.0, 2.0, 1.0);
    let top_bar = rounded_rect(x, y, 40.0, 13.0, 31.0, 1.75, 1.75);
    (rims.min(bridge).min(top_bar), lenses + 3.5)
}

/// Coverage of a shape at distance `d` for a pixel `pixel` units wide (anti-aliasing).
fn coverage(d: f32, pixel: f32) -> f32 {
    (0.5 - d / pixel).clamp(0.0, 1.0)
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Paint `color` with opacity `alpha` over a premultiplied RGBA pixel.
fn over(px: &mut [f32; 4], color: [f32; 3], alpha: f32) {
    for i in 0..3 {
        px[i] = color[i] * alpha + px[i] * (1.0 - alpha);
    }
    px[3] = alpha + px[3] * (1.0 - alpha);
}

/// The app icon as straight (not premultiplied) RGBA, `size` x `size` pixels. Designed on a
/// 1024 grid following macOS's icon layout: an 824 px rounded square with a transparent margin.
pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    const NAVY_TOP: [f32; 3] = [0.20, 0.36, 0.82];
    const NAVY_BOTTOM: [f32; 3] = [0.07, 0.09, 0.27];
    const SCREEN_TOP: [f32; 3] = [0.62, 0.88, 1.0];
    const SCREEN_BOTTOM: [f32; 3] = [0.33, 0.62, 0.96];
    const GLOW: [f32; 3] = [0.45, 0.78, 1.0];
    const WHITE: [f32; 3] = [1.0, 1.0, 1.0];
    const LENS: [f32; 3] = [0.55, 0.80, 1.0];
    // The glasses: 8x the menu bar design, centred low on the icon.
    const GLASSES_SCALE: f32 = 8.0;
    const GLASSES_CENTRE: (f32, f32) = (512.0, 735.0);

    let pixel = 1024.0 / size as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for px in 0..size {
            let x = (px as f32 + 0.5) * pixel;
            let y = (py as f32 + 0.5) * pixel;
            let mut c = [0.0f32; 4];

            // Background: rounded square with a vertical gradient.
            let bg = rounded_rect(x, y, 512.0, 512.0, 412.0, 412.0, 185.0);
            let t = ((y - 100.0) / 824.0).clamp(0.0, 1.0);
            over(&mut c, mix(NAVY_TOP, NAVY_BOTTOM, t), coverage(bg, pixel));

            // Inside the background only: the screen's glow, the screen, the glasses.
            if bg < pixel {
                let inside = coverage(bg, pixel);
                let screen = rounded_rect(x, y, 512.0, 375.0, 262.0, 150.0, 30.0);
                if screen > 0.0 {
                    over(&mut c, GLOW, (-screen / 45.0).exp() * 0.45 * inside);
                }
                let t = ((y - 225.0) / 300.0).clamp(0.0, 1.0);
                over(&mut c, mix(SCREEN_TOP, SCREEN_BOTTOM, t), coverage(screen, pixel) * inside);

                let gx = (x - GLASSES_CENTRE.0) / GLASSES_SCALE + GLASSES_SIZE.0 / 2.0;
                let gy = (y - GLASSES_CENTRE.1) / GLASSES_SCALE + GLASSES_SIZE.1 / 2.0 + 2.0;
                let (outline, openings) = glasses(gx, gy);
                let g_pixel = pixel / GLASSES_SCALE;
                over(&mut c, LENS, coverage(openings, g_pixel) * 0.35 * inside);
                over(&mut c, WHITE, coverage(outline, g_pixel) * inside);
            }

            // Back to straight alpha for PNG.
            let i = ((py * size + px) * 4) as usize;
            let a = c[3];
            for k in 0..3 {
                let v = if a > 0.0 { c[k] / a } else { 0.0 };
                out[i + k] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
            out[i + 3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    out
}

/// The files macOS's `iconutil` expects in an `.iconset` folder: (file name, pixel size).
pub const ICONSET: [(&str, u32); 10] = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
];

/// Write the app icon at every size into `dir` (created if needed), for
/// `iconutil -c icns <dir>`. Used by `--write-iconset`.
pub fn write_iconset(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, size) in ICONSET {
        let file = std::fs::File::create(dir.join(name))?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size, size);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::High);
        encoder.write_header()?.write_image_data(&app_icon_rgba(size))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(img: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [img[i], img[i + 1], img[i + 2], img[i + 3]]
    }

    #[test]
    fn app_icon_has_a_margin_a_screen_and_glasses() {
        let size = 256;
        let img = app_icon_rgba(size);
        assert_eq!(img.len(), (size * size * 4) as usize);
        // The corners are outside the rounded square: transparent.
        assert_eq!(pixel(&img, size, 2, 2)[3], 0);
        // The middle of the screen is opaque and light blue.
        let screen = pixel(&img, size, 128, 97);
        assert_eq!(screen[3], 255);
        assert!(screen[2] > 200 && screen[0] < screen[2], "{screen:?}");
        // The glasses' top bar is white.
        let bar = pixel(&img, size, 128, 160);
        assert!(bar.iter().all(|&v| v > 230), "{bar:?}");
    }

    #[test]
    fn glasses_outline_and_lenses() {
        // On the rim, in the hollow lens, and well outside.
        assert!(glasses(24.0, 15.5).0 < 0.0);
        assert!(glasses(24.0, 24.0).0 > 0.0 && glasses(24.0, 24.0).1 < 0.0);
        assert!(glasses(2.0, 40.0).0 > 0.0);
    }

    #[test]
    fn writes_every_iconset_size() {
        let dir = std::env::temp_dir().join(format!("vr-space-iconset-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_iconset(&dir).unwrap();
        for (name, size) in ICONSET {
            let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(dir.join(name)).unwrap()));
            let info = decoder.read_info().unwrap().info().clone();
            assert_eq!((info.width, info.height), (size, size), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
