use std::{collections::BTreeMap, path::Path, sync::Arc};

use gpui::{
    AnyElement, App, Bounds, ContentMask, Global, Pixels, RenderImage, Window, canvas, point, px,
    size,
};
use image::DynamicImage;
use serde::Deserialize;

use crate::prelude::*;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct ThemeBinding {
    pub theme: Option<String>,
    pub chrome: Option<String>,
    pub skin: Option<SkinDefinition>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct SkinDefinition {
    image: String,
    reference_width: u32,
    scale: f32,
    surfaces: BTreeMap<String, SurfaceDefinition>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
struct SourceRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum FillMode {
    #[default]
    Stretch,
    Tile,
    Horizontal,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct SurfaceDefinition {
    fill: Option<SourceRect>,
    #[serde(default)]
    fill_mode: FillMode,
    frame: Option<FrameDefinition>,
    #[serde(default)]
    padding: [f32; 4],
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct FrameDefinition {
    rect: SourceRect,
    // CSS order: top, right, bottom, left, in reference-image pixels.
    borders: [u32; 4],
    #[serde(default)]
    edge_mode: Option<FillMode>,
}

struct Sprite {
    image: Arc<RenderImage>,
    width: f32,
    height: f32,
}

struct LoadedSurface {
    fill: Option<Sprite>,
    fill_mode: FillMode,
    pieces: Vec<(usize, usize, Sprite)>,
    borders: [f32; 4],
    edge_mode: Option<FillMode>,
    padding: [f32; 4],
}

#[derive(Default)]
struct ActiveSkin {
    theme: Option<String>,
    surfaces: BTreeMap<String, Arc<LoadedSurface>>,
}

impl Global for ActiveSkin {}

fn crop(
    image: &DynamicImage,
    rect: SourceRect,
    reference_width: u32,
    scale: f32,
) -> Result<Sprite, String> {
    let ratio = image.width() as f64 / reference_width as f64;
    let left = (rect.x as f64 * ratio).floor();
    let top = (rect.y as f64 * ratio).floor();
    let right = ((rect.x as f64 + rect.width as f64) * ratio).ceil();
    let bottom = ((rect.y as f64 + rect.height as f64) * ratio).ceil();
    if rect.width == 0
        || rect.height == 0
        || right > image.width() as f64
        || bottom > image.height() as f64
    {
        return Err(format!("bitmap rectangle {rect:?} is outside the image"));
    }
    let mut pixels = image
        .crop_imm(
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        )
        .into_rgba8();
    // GPUI uploads BGRA, whereas the image decoder returns RGBA.
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Sprite {
        image: Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
            pixels
        )])),
        width: rect.width as f32 * scale,
        height: rect.height as f32 * scale,
    })
}

fn load_skin(
    definition: &SkinDefinition,
    cx: &App,
) -> Result<BTreeMap<String, Arc<LoadedSurface>>, String> {
    if definition.reference_width == 0 || !definition.scale.is_finite() || definition.scale <= 0.0 {
        return Err("reference_width and scale must be positive".into());
    }
    let bytes = if let Some(asset) = definition.image.strip_prefix("asset:") {
        cx.asset_source()
            .load(asset)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("missing bitmap asset {asset}"))?
            .into_owned()
    } else {
        let path = Path::new(&definition.image);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
            Path::new(&home).join(".config/zed").join(path)
        };
        std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?
    };
    let image = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
    definition
        .surfaces
        .iter()
        .map(|(name, surface)| {
            if surface
                .padding
                .iter()
                .any(|value| !value.is_finite() || *value < 0.0)
            {
                return Err(format!("invalid padding for {name}"));
            }
            let fill = surface
                .fill
                .map(|rect| crop(&image, rect, definition.reference_width, definition.scale))
                .transpose()?;
            let mut pieces = Vec::new();
            let mut borders = [0.0; 4];
            if let Some(frame) = &surface.frame {
                let [top, right, bottom, left] = frame.borders;
                let rect = frame.rect;
                if left.saturating_add(right) >= rect.width
                    || top.saturating_add(bottom) >= rect.height
                {
                    return Err(format!("frame borders overlap in {name}"));
                }
                borders = frame.borders.map(|value| value as f32 * definition.scale);
                // Validate before subdivision to reject overflowing source coordinates.
                crop(&image, rect, definition.reference_width, definition.scale)?;
                let columns = [
                    (rect.x, left),
                    (rect.x + left, rect.width - left - right),
                    (rect.x + rect.width - right, right),
                ];
                let rows = [
                    (rect.y, top),
                    (rect.y + top, rect.height - top - bottom),
                    (rect.y + rect.height - bottom, bottom),
                ];
                for (row, (y, height)) in rows.into_iter().enumerate() {
                    for (column, (x, width)) in columns.into_iter().enumerate() {
                        if (row == 1 && column == 1) || width == 0 || height == 0 {
                            continue;
                        }
                        pieces.push((
                            column,
                            row,
                            crop(
                                &image,
                                SourceRect {
                                    x,
                                    y,
                                    width,
                                    height,
                                },
                                definition.reference_width,
                                definition.scale,
                            )?,
                        ));
                    }
                }
            }
            Ok((
                name.clone(),
                Arc::new(LoadedSurface {
                    fill,
                    fill_mode: surface.fill_mode,
                    pieces,
                    borders,
                    edge_mode: surface.frame.as_ref().and_then(|frame| frame.edge_mode),
                    padding: surface.padding,
                }),
            ))
        })
        .collect()
}

pub(crate) fn set_bitmap_skin(binding: Option<&ThemeBinding>, cx: &mut App) {
    let mut skin = ActiveSkin::default();
    if let Some(binding) = binding {
        skin.theme = binding.theme.clone();
        if let Some(definition) = &binding.skin {
            match load_skin(definition, cx) {
                Ok(surfaces) => skin.surfaces = surfaces,
                Err(error) => log::error!("WinMan bitmap theme: {error}"),
            }
        }
    }
    cx.set_global(skin);
}

fn active_surface(name: &str, cx: &App) -> Option<Arc<LoadedSurface>> {
    let skin = cx.try_global::<ActiveSkin>()?;
    // A temporary theme-picker preview must not retain another theme's artwork.
    if skin
        .theme
        .as_ref()
        .is_some_and(|name| name.as_str() != cx.theme().name.as_ref())
    {
        return None;
    }
    skin.surfaces.get(name).cloned()
}

pub fn winman_skin_padding(name: &str, cx: &App) -> Option<[Pixels; 4]> {
    active_surface(name, cx).map(|surface| surface.padding.map(px))
}

pub fn has_winman_skin(name: &str, cx: &App) -> bool {
    active_surface(name, cx).is_some()
}

fn paint_sprite(sprite: &Sprite, bounds: Bounds<Pixels>, mode: FillMode, window: &mut Window) {
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return;
    }
    let tile_width = match mode {
        FillMode::Stretch => bounds.size.width,
        _ => px(sprite.width),
    };
    let tile_height = match mode {
        FillMode::Tile => px(sprite.height),
        _ => bounds.size.height,
    };
    if tile_width <= px(0.) || tile_height <= px(0.) {
        return;
    }
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        let mut y = bounds.top();
        while y < bounds.bottom() {
            let mut x = bounds.left();
            while x < bounds.right() {
                if let Err(error) = window.paint_image(
                    Bounds::new(point(x, y), size(tile_width, tile_height)),
                    Default::default(),
                    sprite.image.clone(),
                    0,
                    false,
                ) {
                    log::error!("WinMan bitmap paint: {error}");
                    return;
                }
                x += tile_width;
            }
            y += tile_height;
        }
    });
}

fn paint_loaded_surface(surface: &LoadedSurface, bounds: Bounds<Pixels>, window: &mut Window) {
    if let Some(fill) = &surface.fill {
        paint_sprite(fill, bounds, surface.fill_mode, window);
    }
    let [top, right, bottom, left] = surface.borders.map(px);
    let horizontal_scale = (bounds.size.width / (left + right).max(px(1.))).min(1.);
    let vertical_scale = (bounds.size.height / (top + bottom).max(px(1.))).min(1.);
    let (left, right) = (left * horizontal_scale, right * horizontal_scale);
    let (top, bottom) = (top * vertical_scale, bottom * vertical_scale);
    let columns = [
        (bounds.left(), left),
        (bounds.left() + left, bounds.size.width - left - right),
        (bounds.right() - right, right),
    ];
    let rows = [
        (bounds.top(), top),
        (bounds.top() + top, bounds.size.height - top - bottom),
        (bounds.bottom() - bottom, bottom),
    ];
    for (column, row, sprite) in &surface.pieces {
        if let Some(((x, width), (y, height))) = columns.get(*column).zip(rows.get(*row)) {
            let mode = surface.edge_mode.unwrap_or_else(|| {
                if *column == 1 {
                    FillMode::Horizontal
                } else if *row == 1 {
                    FillMode::Tile
                } else {
                    FillMode::Stretch
                }
            });
            paint_sprite(
                sprite,
                Bounds::new(point(*x, *y), size(*width, *height)),
                mode,
                window,
            );
        }
    }
}

pub fn paint_winman_skin(name: &str, bounds: Bounds<Pixels>, window: &mut Window, cx: &App) {
    if let Some(surface) = active_surface(name, cx) {
        paint_loaded_surface(&surface, bounds, window);
    }
}

/// `name@<variant>` when the skin defines it, else `name`. Skins use this for
/// per-collection copies of a surface (`tab_active@2`, `bottom_strip@0`): the
/// caller passes the winman page when its side of the window holds focus and
/// `None` otherwise, which draws the neutral bitmap.
pub fn winman_skin_surface_variant(
    name: &str,
    variant: Option<usize>,
    cx: &App,
) -> Option<AnyElement> {
    variant
        .and_then(|variant| winman_skin_surface(&format!("{name}@{variant}"), cx))
        .or_else(|| winman_skin_surface(name, cx))
}

pub fn winman_skin_surface(name: &str, cx: &App) -> Option<AnyElement> {
    let surface = active_surface(name, cx)?;
    Some(
        canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                paint_loaded_surface(&surface, bounds, window);
            },
        )
        .absolute()
        .inset_0()
        .size_full()
        .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_bounds_atlas_regions() {
        let image = DynamicImage::new_rgba8(16, 16);
        assert!(
            crop(
                &image,
                SourceRect {
                    x: 15,
                    y: 0,
                    width: 2,
                    height: 1
                },
                16,
                1.
            )
            .is_err()
        );
        assert!(
            crop(
                &image,
                SourceRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 1
                },
                16,
                1.
            )
            .is_err()
        );
    }

    #[test]
    fn converts_reference_coordinates_and_rgba_to_bgra() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            32,
            32,
            image::Rgba([1, 2, 3, 255]),
        ));
        let sprite = crop(
            &image,
            SourceRect {
                x: 0,
                y: 0,
                width: 4,
                height: 6,
            },
            16,
            0.5,
        )
        .expect("valid crop");
        assert_eq!(sprite.image.size(0).width.0, 8);
        assert_eq!(sprite.image.size(0).height.0, 12);
        assert_eq!(
            sprite.image.as_bytes(0).and_then(|bytes| bytes.get(..4)),
            Some([3, 2, 1, 255].as_slice())
        );
        assert_eq!(sprite.width, 2.);
    }
}
