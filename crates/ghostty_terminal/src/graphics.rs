//! Bitmaps for the terminal tab bar, drawn at device resolution so GPUI
//! samples them 1:1.
//!
//! The Ghostty fork's tab bar uses SF Symbols and, in winman's Amiga theme,
//! nearest-neighbour pixel art and Bayer-dithered ramps. GPUI has neither, so
//! they are rasterized here: symbols through AppKit, pixel art by hand.

use std::{cell::RefCell, collections::HashMap, f64::consts::TAU, sync::Arc};

use cocoa::{
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use gpui::{RenderImage, Rgba};
use objc::{class, msg_send, sel, sel_impl};
use smallvec::SmallVec;

use crate::ns_string;

/// Images are cached by a descriptive key; the cache is dropped wholesale
/// once it grows past this, like the fork's gradient cache.
const CACHE_LIMIT: usize = 400;

/// A rasterized image and its size in points.
#[derive(Clone)]
pub struct Bitmap {
    pub image: Arc<RenderImage>,
    pub width: f32,
    pub height: f32,
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Bitmap>> = RefCell::new(HashMap::new());
}

fn cached(key: String, build: impl FnOnce() -> Option<Bitmap>) -> Option<Bitmap> {
    if let Some(image) = CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return Some(image);
    }
    let image = build()?;
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() > CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, image.clone());
    });
    Some(image)
}

fn color_key(color: Rgba) -> u32 {
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u32;
    (channel(color.r) << 16) | (channel(color.g) << 8) | channel(color.b)
}

/// Wraps straight-alpha RGBA pixels as a GPUI image (which stores BGRA).
fn render_image(width: u32, height: u32, mut pixels: Vec<u8>) -> Option<Arc<RenderImage>> {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, pixels)?;
    let frames: SmallVec<[image::Frame; 1]> = SmallVec::from_elem(image::Frame::new(buffer), 1);
    Some(Arc::new(RenderImage::new(frames)))
}

/// SwiftUI font weights, as `NSFontWeight` values.
#[derive(Clone, Copy)]
pub enum SymbolWeight {
    Medium,
    Semibold,
    Bold,
}

impl SymbolWeight {
    fn ns_font_weight(self) -> f64 {
        match self {
            SymbolWeight::Medium => 0.23,
            SymbolWeight::Semibold => 0.3,
            SymbolWeight::Bold => 0.4,
        }
    }
}

/// An SF Symbol tinted `color`, rotated clockwise by `rotation` turns. It is
/// centered in a `canvas`-sized square (points), or keeps the symbol's own
/// size like a SwiftUI `Image(systemName:)` when `canvas` is `None`.
pub fn sf_symbol(
    name: &str,
    point_size: f64,
    weight: SymbolWeight,
    color: Rgba,
    canvas: Option<f64>,
    rotation: f64,
    scale: f32,
) -> Option<Bitmap> {
    let key = format!(
        "sf:{name}:{point_size}:{}:{:06x}:{canvas:?}:{rotation:.4}:{scale}",
        weight.ns_font_weight(),
        color_key(color)
    );
    cached(key, || unsafe {
        let image: id = msg_send![class!(NSImage),
            imageWithSystemSymbolName: ns_string(name)
            accessibilityDescription: nil];
        if image == nil {
            return None;
        }
        let size_configuration: id = msg_send![class!(NSImageSymbolConfiguration),
            configurationWithPointSize: point_size
            weight: weight.ns_font_weight()];
        let ns_color: id = msg_send![class!(NSColor),
            colorWithSRGBRed: color.r as f64
            green: color.g as f64
            blue: color.b as f64
            alpha: 1.0f64];
        let colors: id = msg_send![class!(NSArray), arrayWithObject: ns_color];
        let color_configuration: id =
            msg_send![class!(NSImageSymbolConfiguration), configurationWithPaletteColors: colors];
        let configuration: id = msg_send![size_configuration, configurationByApplyingConfiguration: color_configuration];
        let image: id = msg_send![image, imageWithSymbolConfiguration: configuration];
        if image == nil {
            return None;
        }
        let symbol_size: NSSize = msg_send![image, size];
        let (canvas_width, canvas_height) = match canvas {
            Some(canvas) => (canvas, canvas),
            None => (symbol_size.width.ceil(), symbol_size.height.ceil()),
        };
        draw_into_bitmap(canvas_width, canvas_height, scale, |_| {
            let transform: id = msg_send![class!(NSAffineTransform), transform];
            let _: () =
                msg_send![transform, translateXBy: canvas_width / 2. yBy: canvas_height / 2.];
            // AppKit's y axis points up, so a negative angle turns clockwise.
            let _: () = msg_send![transform, rotateByRadians: -rotation * TAU];
            let _: () =
                msg_send![transform, translateXBy: -canvas_width / 2. yBy: -canvas_height / 2.];
            let _: () = msg_send![transform, concat];
            let rect = NSRect::new(
                NSPoint::new(
                    (canvas_width - symbol_size.width) / 2.,
                    (canvas_height - symbol_size.height) / 2.,
                ),
                symbol_size,
            );
            let _: () = msg_send![image, drawInRect: rect];
        })
    })
}

/// Draws into a premultiplied bitmap covering `width` x `height` points at
/// `scale`, and returns it as a straight-alpha GPUI image.
unsafe fn draw_into_bitmap(
    width: f64,
    height: f64,
    scale: f32,
    draw: impl FnOnce(id),
) -> Option<Bitmap> {
    unsafe {
        let pixels_wide = (width * scale as f64).round() as i64;
        let pixels_high = (height * scale as f64).round() as i64;
        if pixels_wide <= 0 || pixels_high <= 0 {
            return None;
        }
        let rep: id = msg_send![class!(NSBitmapImageRep), alloc];
        let rep: id = msg_send![rep,
            initWithBitmapDataPlanes: std::ptr::null_mut::<*mut u8>()
            pixelsWide: pixels_wide
            pixelsHigh: pixels_high
            bitsPerSample: 8i64
            samplesPerPixel: 4i64
            hasAlpha: true
            isPlanar: false
            colorSpaceName: ns_string("NSDeviceRGBColorSpace")
            bytesPerRow: pixels_wide * 4
            bitsPerPixel: 32i64];
        if rep == nil {
            return None;
        }
        let _: () = msg_send![rep, setSize: NSSize::new(width, height)];
        let context: id =
            msg_send![class!(NSGraphicsContext), graphicsContextWithBitmapImageRep: rep];
        let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
        let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: context];
        draw(context);
        let _: () = msg_send![context, flushGraphics];
        let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];

        let data: *const u8 = msg_send![rep, bitmapData];
        let length = (pixels_wide * pixels_high * 4) as usize;
        let mut rgba = std::slice::from_raw_parts(data, length).to_vec();
        let _: () = msg_send![rep, release];
        for pixel in rgba.chunks_exact_mut(4) {
            let alpha = pixel[3] as u32;
            if alpha > 0 && alpha < 255 {
                for channel in &mut pixel[..3] {
                    *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
        }
        let image = render_image(pixels_wide as u32, pixels_high as u32, rgba)?;
        Some(Bitmap {
            image,
            width: width as f32,
            height: height as f32,
        })
    }
}

/// The fork's `ProhibitedMark`: a ring with a slash from top-left to
/// bottom-right, `size` points across.
pub fn prohibited_mark(color: Rgba, size: f64, scale: f32) -> Option<Bitmap> {
    let key = format!("prohibited:{:06x}:{size}:{scale}", color_key(color));
    cached(key, || unsafe {
        draw_into_bitmap(size, size, scale, |_| {
            let line_width = (size * 0.16).max(1.2);
            let radius = (size - line_width) / 2.;
            let center = size / 2.;
            let k = radius * std::f64::consts::FRAC_1_SQRT_2;
            let ns_color: id = msg_send![class!(NSColor),
                colorWithSRGBRed: color.r as f64
                green: color.g as f64
                blue: color.b as f64
                alpha: 1.0f64];
            let _: () = msg_send![ns_color, setStroke];
            let ring: id = msg_send![class!(NSBezierPath), bezierPathWithOvalInRect:
                NSRect::new(NSPoint::new(center - radius, center - radius), NSSize::new(2. * radius, 2. * radius))];
            let _: () = msg_send![ring, setLineWidth: line_width];
            let _: () = msg_send![ring, stroke];
            // AppKit's y axis points up: top-left is (-k, +k).
            let slash: id = msg_send![class!(NSBezierPath), bezierPath];
            let _: () = msg_send![slash, moveToPoint: NSPoint::new(center - k, center + k)];
            let _: () = msg_send![slash, lineToPoint: NSPoint::new(center + k, center - k)];
            let _: () = msg_send![slash, setLineWidth: line_width];
            let _: () = msg_send![slash, stroke];
        })
    })
}

/// Blends `a` toward `b` by `amount`.
pub fn mix(a: Rgba, b: Rgba, amount: f32) -> Rgba {
    Rgba {
        r: a.r + (b.r - a.r) * amount,
        g: a.g + (b.g - a.g) * amount,
        b: a.b + (b.b - a.b) * amount,
        a: 1.,
    }
}

pub fn lighten(color: Rgba, amount: f32) -> Rgba {
    mix(color, gpui::rgb(0xffffff), amount)
}

pub fn darken(color: Rgba, amount: f32) -> Rgba {
    mix(color, gpui::rgb(0x000000), amount)
}

fn rgba_bytes(color: Rgba) -> [u8; 4] {
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    [channel(color.r), channel(color.g), channel(color.b), 255]
}

/// A vertical ramp from `top` to `bottom` in `levels` flat colors, blended
/// with a 2x2 Bayer matrix (the fork's `PixelArt.gradient`), one art pixel
/// per point.
pub fn dithered_gradient(
    width: u32,
    height: u32,
    top: Rgba,
    bottom: Rgba,
    levels: u32,
    scale: f32,
) -> Option<Bitmap> {
    if width == 0 || height == 0 || levels < 2 {
        return None;
    }
    let key = format!(
        "ramp:{width}x{height}:{:06x}:{:06x}:{levels}:{scale}",
        color_key(top),
        color_key(bottom)
    );
    cached(key, || {
        const BAYER: [[f32; 2]; 2] = [[0., 2.], [3., 1.]];
        let palette: Vec<[u8; 4]> = (0..levels)
            .map(|index| rgba_bytes(mix(top, bottom, index as f32 / (levels - 1) as f32)))
            .collect();
        let factor = scale.round().max(1.) as u32;
        let device_width = width * factor;
        let device_height = height * factor;
        let mut pixels = Vec::with_capacity((device_width * device_height * 4) as usize);
        for device_y in 0..device_height {
            let y = device_y / factor;
            let t = if height > 1 {
                y as f32 / (height - 1) as f32 * (levels - 1) as f32
            } else {
                0.
            };
            let low = (t.floor() as u32).min(levels - 2);
            for device_x in 0..device_width {
                let x = device_x / factor;
                let threshold = (BAYER[(y % 2) as usize][(x % 2) as usize] + 0.5) / 4.;
                let color = if t - low as f32 > threshold {
                    palette[(low + 1) as usize]
                } else {
                    palette[low as usize]
                };
                pixels.extend_from_slice(&color);
            }
        }
        Some(Bitmap {
            image: render_image(device_width, device_height, pixels)?,
            width: width as f32,
            height: height as f32,
        })
    })
}

/// A pixel-art sprite: each character of `mask` maps to a color (or none),
/// one art pixel per point, optionally rotated clockwise by `rotation` turns
/// with nearest-neighbour sampling (as Core Animation does for the fork's
/// spinning pixel gear).
pub fn pixel_sprite(
    name: &str,
    mask: &[&str],
    color_of: impl Fn(usize, usize, char) -> Option<Rgba>,
    rotation: f64,
    scale: f32,
    key_extra: &str,
) -> Option<Bitmap> {
    let height = mask.len();
    let width = mask.first().map_or(0, |row| row.chars().count());
    if width == 0 {
        return None;
    }
    let key = format!("sprite:{name}:{key_extra}:{rotation:.4}:{scale}");
    cached(key, || {
        let cells: Vec<Vec<Option<[u8; 4]>>> = mask
            .iter()
            .enumerate()
            .map(|(y, row)| {
                row.chars()
                    .enumerate()
                    .map(|(x, character)| color_of(x, y, character).map(rgba_bytes))
                    .collect()
            })
            .collect();
        let factor = scale.round().max(1.) as usize;
        let device_width = width * factor;
        let device_height = height * factor;
        let center_x = device_width as f64 / 2.;
        let center_y = device_height as f64 / 2.;
        let (sin, cos) = (rotation * TAU).sin_cos();
        let mut pixels = Vec::with_capacity(device_width * device_height * 4);
        for device_y in 0..device_height {
            for device_x in 0..device_width {
                // Inverse-rotate the pixel center back into the sprite.
                let dx = device_x as f64 + 0.5 - center_x;
                let dy = device_y as f64 + 0.5 - center_y;
                let source_x = cos * dx + sin * dy + center_x;
                let source_y = -sin * dx + cos * dy + center_y;
                let cell = if source_x >= 0. && source_y >= 0. {
                    let x = source_x as usize / factor;
                    let y = source_y as usize / factor;
                    cells.get(y).and_then(|row| row.get(x)).copied().flatten()
                } else {
                    None
                };
                pixels.extend_from_slice(&cell.unwrap_or([0, 0, 0, 0]));
            }
        }
        Some(Bitmap {
            image: render_image(device_width as u32, device_height as u32, pixels)?,
            width: width as f32,
            height: height as f32,
        })
    })
}

pub const GEAR_MASK: [&str; 11] = [
    "....KKK....",
    ".KK.KMK.KK.",
    ".KMKKMKKMK.",
    ".KKMMMMMKK.",
    "KKMMMKMMMKK",
    "KMMMK.KMMMK",
    "KKMMMKMMMKK",
    ".KKMMMMMKK.",
    ".KMKKMKKMK.",
    ".KK.KMK.KK.",
    "....KKK....",
];

pub const NO_ENTRY_MASK: [&str; 11] = [
    "...KKKKK...",
    "..KRRRRRK..",
    ".KRRKKKRRK.",
    "KRRRRK.KRRK",
    "KRK.RRK.KRK",
    "KRK.KRRK.RK",
    "KRK..KRRKRK",
    "KRRK.KKRRRK",
    ".KRRKKKRRK.",
    "..KRRRRRK..",
    "...KKKKK...",
];

/// The fork's pixel gear in `base`: outline, and a light/dark bevel on the
/// teeth depending on the diagonal.
pub fn pixel_gear(base: Rgba, rotation: f64, scale: f32) -> Option<Bitmap> {
    let light = lighten(base, 0.4);
    let dark = darken(base, 0.35);
    pixel_sprite(
        "gear",
        &GEAR_MASK,
        |x, y, character| match character {
            'K' => Some(gpui::rgb(0x0c0a0a)),
            'M' if x + y < 9 => Some(light),
            'M' if x + y > 11 => Some(dark),
            'M' => Some(base),
            _ => None,
        },
        rotation,
        scale,
        &format!("{:06x}", color_key(base)),
    )
}

pub fn pixel_no_entry(scale: f32) -> Option<Bitmap> {
    pixel_sprite(
        "no-entry",
        &NO_ENTRY_MASK,
        |_, _, character| match character {
            'K' => Some(gpui::rgb(0x280604)),
            'R' => Some(gpui::rgb(0xfb4934)),
            _ => None,
        },
        0.,
        scale,
        "",
    )
}

/// The spinning gear's phase: one clockwise turn per 4 s, locked to Core
/// Animation's clock (host uptime, shared by every process), like the fork's
/// `SpinningGear`, so every gear and winman's bar gear turn in step.
pub fn gear_phase() -> f64 {
    const PERIOD_SECONDS: f64 = 4.;
    unsafe extern "C" {
        fn CACurrentMediaTime() -> f64;
    }
    let now = unsafe { CACurrentMediaTime() };
    (now % PERIOD_SECONDS) / PERIOD_SECONDS
}

/// Quantizes a phase so the gear frames stay a bounded set in the cache.
pub fn quantize_phase(phase: f64, steps: u32) -> f64 {
    (phase * steps as f64).floor() / steps as f64
}
