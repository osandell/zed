use std::{collections::BTreeMap, path::Path, sync::Arc};

use gpui::{
    AnyElement, App, BorderStyle, Bounds, BoxShadow, ContentMask, Corners, Edges, Global, Hsla,
    Pixels,
    RenderImage, Rgba, Window, canvas, point, px, quad, size,
};
use image::DynamicImage;
use serde::Deserialize;

use crate::prelude::*;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct ThemeBinding {
    pub theme: Option<String>,
    pub chrome: Option<String>,
    pub skin: Option<SkinDefinition>,
    /// Whether arcoscope's bar draws its marks as pixel sprites in this theme, so
    /// the terminal tabs match it. Every theme but flat does, unless it says no.
    #[serde(default)]
    pub pixel_art: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct SkinDefinition {
    /// Only needed when a surface samples the atlas (`fill` or `frame`); a skin
    /// made only of vector `layers` has none.
    #[serde(default)]
    image: Option<String>,
    #[serde(default = "default_reference_width")]
    reference_width: u32,
    #[serde(default = "default_scale")]
    scale: f32,
    surfaces: BTreeMap<String, SurfaceDefinition>,
}

fn default_reference_width() -> u32 {
    1
}

fn default_scale() -> f32 {
    1.
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
    /// Rounded rectangles painted in order over any bitmap parts.
    #[serde(default)]
    layers: Vec<LayerDefinition>,
    #[serde(default)]
    padding: [f32; 4],
}

/// One vector layer: a rounded rectangle `inset` from the surface's bounds
/// (top, right, bottom, left, in points), with an optional fill, a border of
/// `border_widths` (same order; 1 pt all round when omitted) and an `etch`, a
/// 1 pt line of its own just outside the border. `radius` is top-left,
/// top-right, bottom-right, bottom-left. Colours are `#rrggbb` or `#rrggbbaa`.
/// A `shadow` is painted under the layer and follows its shape, so on a
/// transparent window it falls only round the drawn parts.
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct LayerDefinition {
    #[serde(default)]
    inset: [f32; 4],
    fill: Option<String>,
    border: Option<String>,
    border_widths: Option<[f32; 4]>,
    #[serde(default)]
    radius: [f32; 4],
    etch: Option<String>,
    shadow: Option<ShadowDefinition>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct ShadowDefinition {
    color: String,
    #[serde(default)]
    offset: [f32; 2],
    #[serde(default)]
    blur: f32,
    #[serde(default)]
    spread: f32,
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

struct Layer {
    inset: [f32; 4],
    fill: Option<Hsla>,
    border: Option<(Hsla, [f32; 4])>,
    radius: [f32; 4],
    etch: Option<Hsla>,
    shadow: Option<BoxShadow>,
}

fn parse_color(value: &str) -> Result<Hsla, String> {
    Rgba::try_from(value)
        .map(Hsla::from)
        .map_err(|error| error.to_string())
}

fn load_layer(layer: &LayerDefinition) -> Result<Layer, String> {
    let finite = |values: &[f32]| values.iter().all(|value| value.is_finite() && *value >= 0.);
    let widths = layer.border_widths.unwrap_or([1.; 4]);
    if !finite(&layer.inset) || !finite(&layer.radius) || !finite(&widths) {
        return Err("layer insets, radii and border widths must be non-negative".into());
    }
    Ok(Layer {
        inset: layer.inset,
        fill: layer.fill.as_deref().map(parse_color).transpose()?,
        border: layer
            .border
            .as_deref()
            .map(parse_color)
            .transpose()?
            .map(|color| (color, widths)),
        radius: layer.radius,
        etch: layer.etch.as_deref().map(parse_color).transpose()?,
        shadow: layer
            .shadow
            .as_ref()
            .map(|shadow| -> Result<BoxShadow, String> {
                if !shadow.blur.is_finite()
                    || shadow.blur < 0.
                    || !shadow.spread.is_finite()
                    || !shadow.offset.iter().all(|value| value.is_finite())
                {
                    return Err("shadow blur, spread and offset must be finite".into());
                }
                Ok(BoxShadow {
                    color: parse_color(&shadow.color)?,
                    offset: point(px(shadow.offset[0]), px(shadow.offset[1])),
                    blur_radius: px(shadow.blur),
                    spread_radius: px(shadow.spread),
                    inset: false,
                })
            })
            .transpose()?,
    })
}

struct LoadedSurface {
    layers: Vec<Layer>,
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
    let needs_image = definition
        .surfaces
        .values()
        .any(|surface| surface.fill.is_some() || surface.frame.is_some());
    let image = if needs_image {
        let image = definition
            .image
            .as_deref()
            .ok_or("a surface samples the bitmap, but the skin has no image")?;
        Some(load_image(image, cx)?)
    } else {
        None
    };
    definition
        .surfaces
        .iter()
        .map(|(name, surface)| load_surface(name, surface, definition, image.as_ref()))
        .collect()
}

fn load_image(image: &str, cx: &App) -> Result<DynamicImage, String> {
    let bytes = if let Some(asset) = image.strip_prefix("asset:") {
        cx.asset_source()
            .load(asset)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("missing bitmap asset {asset}"))?
            .into_owned()
    } else {
        let path = Path::new(image);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
            Path::new(&home).join(".config/zed").join(path)
        };
        std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?
    };
    image::load_from_memory(&bytes).map_err(|error| error.to_string())
}

fn load_surface(
    name: &String,
    surface: &SurfaceDefinition,
    definition: &SkinDefinition,
    image: Option<&DynamicImage>,
) -> Result<(String, Arc<LoadedSurface>), String> {
    if surface
        .padding
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(format!("invalid padding for {name}"));
    }
    let layers = surface
        .layers
        .iter()
        .map(load_layer)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{name}: {error}"))?;
    // Checked by `load_skin`: a surface with bitmap parts has an image.
    let image = match image {
        Some(image) => image,
        None => {
            return Ok((
                name.clone(),
                Arc::new(LoadedSurface {
                    layers,
                    fill: None,
                    fill_mode: surface.fill_mode,
                    pieces: Vec::new(),
                    borders: [0.; 4],
                    edge_mode: None,
                    padding: surface.padding,
                }),
            ));
        }
    };
    let fill = surface
        .fill
        .map(|rect| crop(image, rect, definition.reference_width, definition.scale))
        .transpose()?;
    let mut pieces = Vec::new();
    let mut borders = [0.0; 4];
    if let Some(frame) = &surface.frame {
        let [top, right, bottom, left] = frame.borders;
        let rect = frame.rect;
        if left.saturating_add(right) >= rect.width || top.saturating_add(bottom) >= rect.height {
            return Err(format!("frame borders overlap in {name}"));
        }
        borders = frame.borders.map(|value| value as f32 * definition.scale);
        // Validate before subdivision to reject overflowing source coordinates.
        crop(image, rect, definition.reference_width, definition.scale)?;
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
                        image,
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
            layers,
            fill,
            fill_mode: surface.fill_mode,
            pieces,
            borders,
            edge_mode: surface.frame.as_ref().and_then(|frame| frame.edge_mode),
            padding: surface.padding,
        }),
    ))
}

pub(crate) fn set_bitmap_skin(binding: Option<&ThemeBinding>, cx: &mut App) {
    let mut skin = ActiveSkin::default();
    if let Some(binding) = binding {
        skin.theme = binding.theme.clone();
        if let Some(definition) = &binding.skin {
            match load_skin(definition, cx) {
                Ok(surfaces) => skin.surfaces = surfaces,
                Err(error) => log::error!("Arcoscope bitmap theme: {error}"),
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

pub fn arcoscope_skin_padding(name: &str, cx: &App) -> Option<[Pixels; 4]> {
    active_surface(name, cx).map(|surface| surface.padding.map(px))
}

pub fn has_arcoscope_skin(name: &str, cx: &App) -> bool {
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
                    log::error!("Arcoscope bitmap paint: {error}");
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
    for layer in &surface.layers {
        paint_layer(layer, bounds, window);
    }
}

fn paint_layer(layer: &Layer, bounds: Bounds<Pixels>, window: &mut Window) {
    let [top, right, bottom, left] = layer.inset.map(px);
    let rect = Bounds::new(
        point(bounds.left() + left, bounds.top() + top),
        size(
            bounds.size.width - left - right,
            bounds.size.height - top - bottom,
        ),
    );
    if rect.size.width <= px(0.) || rect.size.height <= px(0.) {
        return;
    }
    let [top_left, top_right, bottom_right, bottom_left] = layer.radius.map(px);
    let radii = Corners {
        top_left,
        top_right,
        bottom_right,
        bottom_left,
    };
    if let Some(shadow) = &layer.shadow {
        window.paint_drop_shadows(rect, radii, std::slice::from_ref(shadow));
    }
    if let Some(etch) = layer.etch {
        let grow = |radius: Pixels| {
            if radius > px(0.) {
                radius + px(1.)
            } else {
                radius
            }
        };
        window.paint_quad(quad(
            Bounds::new(
                point(rect.left() - px(1.), rect.top() - px(1.)),
                size(rect.size.width + px(2.), rect.size.height + px(2.)),
            ),
            Corners {
                top_left: grow(top_left),
                top_right: grow(top_right),
                bottom_right: grow(bottom_right),
                bottom_left: grow(bottom_left),
            },
            gpui::transparent_black(),
            px(1.),
            etch,
            BorderStyle::Solid,
        ));
    }
    let (border_color, widths) = layer
        .border
        .map(|(color, widths)| (color, widths.map(px)))
        .unwrap_or((gpui::transparent_black(), [px(0.); 4]));
    let [width_top, width_right, width_bottom, width_left] = widths;
    window.paint_quad(quad(
        rect,
        radii,
        layer.fill.unwrap_or(gpui::transparent_black()),
        Edges {
            top: width_top,
            right: width_right,
            bottom: width_bottom,
            left: width_left,
        },
        border_color,
        BorderStyle::Solid,
    ));
}

pub fn paint_arcoscope_skin(name: &str, bounds: Bounds<Pixels>, window: &mut Window, cx: &App) {
    if let Some(surface) = active_surface(name, cx) {
        paint_loaded_surface(&surface, bounds, window);
    }
}

/// `name@<variant>` when the skin defines it, else `name`. Skins use this for
/// per-collection copies of a surface (`tab_active@2`, `bottom_strip@0`): the
/// caller passes the arcoscope page when its side of the window holds focus and
/// `None` otherwise, which draws the neutral bitmap.
pub fn arcoscope_skin_surface_variant(
    name: &str,
    variant: Option<usize>,
    cx: &App,
) -> Option<AnyElement> {
    variant
        .and_then(|variant| arcoscope_skin_surface(&format!("{name}@{variant}"), cx))
        .or_else(|| arcoscope_skin_surface(name, cx))
}

pub fn arcoscope_skin_surface(name: &str, cx: &App) -> Option<AnyElement> {
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

    fn bundled_binding(name: &str) -> ThemeBinding {
        let bindings: serde_json::Value = serde_json::from_str(include_str!(
            "../../../assets/images/window-skins/arcoscope.json"
        ))
        .expect("bundled mappings parse");
        serde_json::from_value(bindings["themes"][name].clone())
            .unwrap_or_else(|error| panic!("{name} binding: {error}"))
    }

    #[test]
    fn bundled_vector_skin_loads_without_an_image() {
        let skin = bundled_binding("mist").skin.expect("mist has a skin");
        assert!(skin.image.is_none());
        for (name, surface) in &skin.surfaces {
            load_surface(name, surface, &skin, None).expect("vector surface loads");
        }
    }

    #[test]
    fn gruvbox_dark_is_mist_in_other_colours() {
        let mist = bundled_binding("mist").skin.expect("mist has a skin");
        let gruvbox = bundled_binding("gruvbox-dark");
        assert_eq!(gruvbox.pixel_art, Some(false));
        let skin = gruvbox.skin.expect("gruvbox-dark has a skin");
        assert!(skin.image.is_none());
        for (name, surface) in &skin.surfaces {
            load_surface(name, surface, &skin, None).expect("vector surface loads");
        }
        // Same surfaces and the same shapes; only the colours differ. The
        // sixth collection has a strip Mist does not define.
        for (name, surface) in &mist.surfaces {
            let twin = skin
                .surfaces
                .get(name)
                .unwrap_or_else(|| panic!("gruvbox-dark lacks {name}"));
            assert_eq!(surface.padding, twin.padding, "{name} padding");
            assert_eq!(surface.layers.len(), twin.layers.len(), "{name} layers");
            for (layer, twin) in surface.layers.iter().zip(&twin.layers) {
                assert_eq!(layer.inset, twin.inset, "{name} inset");
                assert_eq!(layer.radius, twin.radius, "{name} radius");
                assert_eq!(layer.border_widths, twin.border_widths, "{name} borders");
                assert_eq!(layer.fill.is_some(), twin.fill.is_some(), "{name} fill");
                assert_eq!(layer.etch.is_some(), twin.etch.is_some(), "{name} etch");
            }
        }
        assert!(skin.surfaces.contains_key("bottom_strip@5"));
    }

    #[test]
    fn rejects_unparseable_layer_colours() {
        let layer = LayerDefinition {
            inset: [0.; 4],
            fill: Some("teal".into()),
            border: None,
            border_widths: None,
            radius: [0.; 4],
            etch: None,
            shadow: None,
        };
        assert!(load_layer(&layer).is_err());
    }

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
