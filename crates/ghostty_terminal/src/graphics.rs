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
        // Rasterize the symbol once, upright, and turn that bitmap. Drawing the
        // symbol itself under a rotated transform lets AppKit pick its raster
        // size from the rotated bounds, so a turning gear grows and shrinks.
        let upright = new_bitmap_rep(symbol_size.width, symbol_size.height, scale)?;
        draw_into_rep(upright, |_| {
            let _: () =
                msg_send![image, drawInRect: NSRect::new(NSPoint::new(0., 0.), symbol_size)];
        });
        let upright_image: id = msg_send![class!(NSImage), alloc];
        let upright_image: id = msg_send![upright_image, initWithSize: symbol_size];
        let _: () = msg_send![upright_image, addRepresentation: upright];
        let _: () = msg_send![upright, release];
        let bitmap = draw_into_bitmap(canvas_width, canvas_height, scale, |context| {
            let _: () = msg_send![context, setImageInterpolation: 3u64];
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
            let _: () = msg_send![upright_image, drawInRect: rect];
        });
        let _: () = msg_send![upright_image, release];
        bitmap
    })
}

/// A transparent bitmap covering `width` x `height` points at `scale`; the
/// caller releases it.
unsafe fn new_bitmap_rep(width: f64, height: f64, scale: f32) -> Option<id> {
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
        Some(rep)
    }
}

unsafe fn draw_into_rep(rep: id, draw: impl FnOnce(id)) {
    unsafe {
        let context: id =
            msg_send![class!(NSGraphicsContext), graphicsContextWithBitmapImageRep: rep];
        let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
        let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: context];
        draw(context);
        let _: () = msg_send![context, flushGraphics];
        let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];
    }
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
        let rep = new_bitmap_rep(width, height, scale)?;
        draw_into_rep(rep, draw);
        let pixels_wide: i64 = msg_send![rep, pixelsWide];
        let pixels_high: i64 = msg_send![rep, pixelsHigh];

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

/// winman's hourglass glass, 9x10 and symmetric top to bottom so it reads the
/// same turned over. `.` inside the glass is where sand can lie.
const HOURGLASS_GLASS: [&str; 10] = [
    "CCCCCCCCC",
    ".C.....C.",
    ".C.....C.",
    "..C...C..",
    "...C.C...",
    "...C.C...",
    "..C...C..",
    ".C.....C.",
    ".C.....C.",
    "CCCCCCCCC",
];

/// Where the grains land in the bottom bulb, in order. The top bulb empties in
/// the same order turned over, which is what makes the turn seamless.
const HOURGLASS_LANDING: [(usize, usize); 9] = [
    (4, 8),
    (3, 8),
    (5, 8),
    (2, 8),
    (6, 8),
    (4, 7),
    (3, 7),
    (5, 7),
    (4, 6),
];

const HOURGLASS_GRAIN_SECONDS: f64 = 0.4;
const HOURGLASS_TURN_SECONDS: f64 = 0.12;
const HOURGLASS_TURNS: [f64; 3] = [0.125, 0.25, 0.375];

/// Which hourglass frame (grains fallen) and rotation (in turns) to show now.
/// Same timing and clock as winman's `HourglassSpinner`, so the tab's glass and
/// the bar's run in step: the sand runs grain by grain, then the glass holds its
/// last frame through three rotation steps and starts over.
pub fn hourglass_phase() -> (usize, f64) {
    let frames = HOURGLASS_LANDING.len() + 1;
    let sand = HOURGLASS_GRAIN_SECONDS * frames as f64;
    let total = sand + HOURGLASS_TURN_SECONDS * HOURGLASS_TURNS.len() as f64;
    unsafe extern "C" {
        fn CACurrentMediaTime() -> f64;
    }
    let time = unsafe { CACurrentMediaTime() } % total;
    if time < sand {
        let frame = (time / HOURGLASS_GRAIN_SECONDS) as usize;
        (frame.min(frames - 1), 0.)
    } else {
        let step = ((time - sand) / HOURGLASS_TURN_SECONDS) as usize;
        let rotation = HOURGLASS_TURNS[step.min(HOURGLASS_TURNS.len() - 1)];
        (frames - 1, rotation)
    }
}

/// winman's running-background-job hourglass with `fallen` grains in the bottom
/// bulb, with its hard black shadow one pixel down-right.
pub fn pixel_hourglass(glass: Rgba, fallen: usize, rotation: f64, scale: f32) -> Option<Bitmap> {
    let grains = HOURGLASS_LANDING.len();
    let fallen = fallen.min(grains);
    let mut grid: Vec<Vec<char>> = HOURGLASS_GLASS
        .iter()
        .map(|row| row.chars().collect())
        .collect();
    let mut set = |x: usize, y: usize| {
        if let Some(cell) = grid.get_mut(y).and_then(|row| row.get_mut(x)) {
            *cell = 'S';
        }
    };
    // The top bulb loses its grains nearest the neck first: the landing order
    // reversed and turned over, so the ones still up there are the first
    // `grains - fallen` of the landing order, mirrored.
    for &(x, y) in &HOURGLASS_LANDING[..grains - fallen] {
        set(8 - x, 9 - y);
    }
    for &(x, y) in &HOURGLASS_LANDING[..fallen] {
        set(x, y);
    }
    if fallen < grains {
        set(4, 4);
        set(4, 5);
    }

    let width = grid.first().map_or(0, Vec::len) + 1;
    let mut shadowed = vec![vec!['.'; width]; grid.len() + 1];
    for (y, row) in grid.iter().enumerate() {
        for (x, &character) in row.iter().enumerate() {
            if character != '.' && shadowed[y + 1][x + 1] == '.' {
                shadowed[y + 1][x + 1] = 'K';
            }
        }
    }
    for (y, row) in grid.iter().enumerate() {
        for (x, &character) in row.iter().enumerate() {
            if character != '.' {
                shadowed[y][x] = character;
            }
        }
    }
    let rows: Vec<String> = shadowed.into_iter().map(String::from_iter).collect();
    let mask: Vec<&str> = rows.iter().map(String::as_str).collect();
    pixel_sprite(
        "hourglass",
        &mask,
        |_, _, character| match character {
            'C' => Some(glass),
            'S' => Some(gpui::rgb(0xfabd2f)),
            'K' => Some(gpui::rgb(0x000000)),
            _ => None,
        },
        rotation,
        scale,
        &format!("{:06x}:{fallen}", color_key(glass)),
    )
}

/// winman's hourglass as Mist draws it (`HourglassSpinner`'s vector frames):
/// a 10x10.5 point glass with `fallen` of the grains in the bottom bulb and a
/// stream through the neck while any are left, turned `rotation` turns
/// clockwise, on a canvas that holds it at every step of the turn. `size`
/// scales the glass and its canvas (16 points at 1).
pub fn vector_hourglass(
    glass: Rgba,
    sand: Rgba,
    fallen: usize,
    rotation: f64,
    size: f64,
    scale: f32,
) -> Option<Bitmap> {
    let grains = HOURGLASS_LANDING.len();
    let fallen = fallen.min(grains);
    let key = format!(
        "vector-hourglass:{:06x}:{:06x}:{fallen}:{rotation:.4}:{size}:{scale}",
        color_key(glass),
        color_key(sand)
    );
    cached(key, || unsafe {
        const W: f64 = 10.;
        const H: f64 = 10.5;
        let canvas = (16. * size).ceil();
        draw_into_bitmap(canvas, canvas, scale, |_| {
            let color = |c: Rgba| -> id {
                msg_send![class!(NSColor),
                    colorWithSRGBRed: c.r as f64
                    green: c.g as f64
                    blue: c.b as f64
                    alpha: 1.0f64]
            };
            let transform: id = msg_send![class!(NSAffineTransform), transform];
            let _: () = msg_send![transform, translateXBy: canvas / 2. yBy: canvas / 2.];
            // AppKit's y axis points up, so a negative angle turns clockwise.
            let _: () = msg_send![transform, rotateByRadians: -rotation * TAU];
            let _: () = msg_send![transform, scaleBy: size];
            let _: () = msg_send![transform, translateXBy: -W / 2. yBy: -H / 2.];
            let _: () = msg_send![transform, concat];

            let (cap, inset, neck) = (1.3, 1.6, 0.9);
            let (mid, top, bottom) = (H / 2., H - cap, cap);
            let glass_path: id = msg_send![class!(NSBezierPath), bezierPath];
            for (index, (x, y)) in [
                (inset, top),
                (W / 2. - neck, mid),
                (inset, bottom),
                (W - inset, bottom),
                (W / 2. + neck, mid),
                (W - inset, top),
            ]
            .into_iter()
            .enumerate()
            {
                let point = NSPoint::new(x, y);
                if index == 0 {
                    let _: () = msg_send![glass_path, moveToPoint: point];
                } else {
                    let _: () = msg_send![glass_path, lineToPoint: point];
                }
            }
            let _: () = msg_send![glass_path, closePath];

            // What is left sits at the neck in the top bulb (a triangle, so its
            // height goes with the square root of the amount); what has fallen
            // piles up from the bottom of the lower one.
            let t = fallen as f64 / grains as f64;
            let bulb = top - mid;
            let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
            let _: () = msg_send![glass_path, addClip];
            let _: () = msg_send![color(sand), setFill];
            let fill = |x: f64, y: f64, w: f64, h: f64| {
                let rect: id = msg_send![class!(NSBezierPath), bezierPathWithRect:
                    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))];
                let _: () = msg_send![rect, fill];
            };
            if t < 1. {
                fill(0., mid, W, bulb * (1. - t).sqrt());
                fill(W / 2. - 0.35, bottom, 0.7, mid - bottom);
            }
            if t > 0. {
                fill(0., bottom, W, bulb * (1. - (1. - t).sqrt()));
            }
            let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];

            let _: () = msg_send![color(glass), setStroke];
            let _: () = msg_send![glass_path, setLineWidth: 1.1f64];
            let _: () = msg_send![glass_path, setLineJoinStyle: 1u64];
            let _: () = msg_send![glass_path, stroke];
            let _: () = msg_send![color(glass), setFill];
            for y in [top, 0.] {
                let cap_path: id = msg_send![class!(NSBezierPath),
                    bezierPathWithRoundedRect: NSRect::new(NSPoint::new(0.4, y), NSSize::new(W - 0.8, cap))
                    xRadius: 0.6f64
                    yRadius: 0.6f64];
                let _: () = msg_send![cap_path, fill];
            }
        })
    })
}

/// winman's terminal icon under a vector theme (`MistStyle.terminal`): a
/// rounded screen outline with a prompt chevron and a cursor line, `k` points
/// per unit (winman's sprite scale), centered on a `canvas`-point square.
pub fn vector_terminal(color: Rgba, k: f64, canvas: f64, scale: f32) -> Option<Bitmap> {
    let key = format!(
        "vector-terminal:{:06x}:{k}:{canvas}:{scale}",
        color_key(color)
    );
    cached(key, || unsafe {
        draw_into_bitmap(canvas, canvas, scale, |_| {
            let (w, h) = (9.9 * k, 7.6 * k);
            let (min_x, min_y) = ((canvas - w) / 2., (canvas - h) / 2.);
            let mid_y = min_y + h / 2.;
            let ns_color: id = msg_send![class!(NSColor),
                colorWithSRGBRed: color.r as f64
                green: color.g as f64
                blue: color.b as f64
                alpha: 1.0f64];
            let _: () = msg_send![ns_color, setStroke];
            let screen: id = msg_send![class!(NSBezierPath),
                bezierPathWithRoundedRect: NSRect::new(
                    NSPoint::new(min_x + 0.7, min_y + 0.7),
                    NSSize::new(w - 1.4, h - 1.4),
                )
                xRadius: 1.5f64
                yRadius: 1.5f64];
            let _: () = msg_send![screen, setLineWidth: 1.4f64];
            let _: () = msg_send![screen, stroke];
            // winman draws in a flipped view (+y down); AppKit's y axis points
            // up, so the cursor line below the middle is at mid_y - 3u here.
            let u = h / 14.;
            let prompt: id = msg_send![class!(NSBezierPath), bezierPath];
            let _: () =
                msg_send![prompt, moveToPoint: NSPoint::new(min_x + 4. * u, mid_y + 2.5 * u)];
            let _: () = msg_send![prompt, lineToPoint: NSPoint::new(min_x + 7. * u, mid_y)];
            let _: () =
                msg_send![prompt, lineToPoint: NSPoint::new(min_x + 4. * u, mid_y - 2.5 * u)];
            let _: () =
                msg_send![prompt, moveToPoint: NSPoint::new(min_x + 9. * u, mid_y - 3. * u)];
            let _: () =
                msg_send![prompt, lineToPoint: NSPoint::new(min_x + 14. * u, mid_y - 3. * u)];
            let _: () = msg_send![prompt, setLineWidth: 1.4f64];
            // NSLineCapStyleRound, NSLineJoinStyleRound.
            let _: () = msg_send![prompt, setLineCapStyle: 1u64];
            let _: () = msg_send![prompt, setLineJoinStyle: 1u64];
            let _: () = msg_send![prompt, stroke];
        })
    })
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
