// SPDX-License-Identifier: MPL-2.0
//! Tile images of the 2D mode (`docs/specs/86-2d-toolkit.md` § 6): the map document drawn into the
//! front pane through egui meshes, one mesh per layer and tileset, so the viewport shows the
//! edited tiles without a running game.
//!
//! [`view_quads`] is pure: it culls the visible cells and yields pane-pixel rectangles and the
//! same texture coordinates the runtime sprite pass samples (`sprite2d.wgsl`: flip bits, layer
//! offset, oversized tiles anchored bottom-left). Animated tiles show the frame of the pane's clock
//! (`Mode2d::clock`, `regia_2d::batch::animated_tile`, spec § 13): a paused clock at 0 shows the
//! first animation frame, which can differ from the cell's base tile. Past
//! [`MAX_VIEW_QUADS`] the view samples every Nth cell and draws it N times larger: a far zoom
//! stays cheap and still shows the map's shape. A tileset whose image did not load is drawn in flat
//! colors, so the map stays readable.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::epaint::{Mesh, Vertex};
use regia_2d::batch::animated_tile;
use regia_2d::tilemap::{
    FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, MapLayer, TileMapAsset, TilesetDef, split_gid,
};
use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;

use super::Mode2d;
use crate::views::ViewCamera;

/// Most tile quads one pane draws; past it the view samples cells.
pub const MAX_VIEW_QUADS: u64 = 200_000;
/// Largest tileset image the mode decodes (bytes on disk).
const MAX_IMAGE_BYTES: u64 = 256 << 20;

/// One tileset's image as the 2D mode sees it.
#[derive(Debug, Clone)]
pub struct TilesetImage {
    /// The tileset's name.
    pub tileset: String,
    /// The image file (the tileset's `image`, relative to the map file).
    pub path: PathBuf,
    /// The decoded image, when it loaded.
    pub pixels: Option<Arc<egui::ColorImage>>,
    /// Why it did not load.
    pub error: Option<String>,
}

impl TilesetImage {
    /// Size in pixels when it loaded.
    #[must_use]
    pub fn size(&self) -> Option<[usize; 2]> {
        self.pixels.as_ref().map(|p| p.size)
    }
}

fn decode(path: &Path) -> Result<egui::ColorImage, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{} is larger than {MAX_IMAGE_BYTES} bytes",
            path.display()
        ));
    }
    let image = crate::mesh_source::load_texture_file(path)?;
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [image.width as usize, image.height as usize],
        &image.rgba,
    ))
}

/// Decodes the tileset images of `map`, which lives at `map_path`. A missing or broken image is
/// recorded on its entry, never an error: the mode still paints.
#[must_use]
pub fn load_images(map: &TileMapAsset, map_path: &Path) -> Vec<TilesetImage> {
    let dir = map_path.parent().unwrap_or_else(|| Path::new(""));
    map.tilesets
        .iter()
        .map(|set| {
            let path = dir.join(&set.image);
            let (pixels, error) = match decode(&path) {
                Ok(image) => (Some(Arc::new(image)), None),
                Err(e) => {
                    tracing::warn!(target: "regia_editor::mode2d", "tileset {}: {e}", set.name);
                    (None, Some(e))
                }
            };
            TilesetImage {
                tileset: set.name.clone(),
                path,
                pixels,
                error,
            }
        })
        .collect()
}

/// The GPU copies of the document's tileset images (a world resource, rebuilt when the mode's
/// `image_generation` moves). `None` where an image did not load or does not fit the GPU.
#[derive(Resource, Default)]
pub struct TileTextures {
    generation: u64,
    handles: Vec<Option<egui::TextureHandle>>,
}

impl TileTextures {
    /// The texture of tileset `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&egui::TextureHandle> {
        self.handles.get(index).and_then(Option::as_ref)
    }
}

/// Brings [`TileTextures`] up to date with the document. Images larger than the GPU's texture limit
/// are refused here, and the refusal lands on their entry (agents read it in `editor.mode2d_get`).
pub fn sync_textures(ctx: &egui::Context, world: &mut World) {
    let Some(generation) = world.get_resource::<Mode2d>().map(|m| m.image_generation) else {
        return;
    };
    if world
        .get_resource::<TileTextures>()
        .is_some_and(|t| t.generation == generation)
    {
        return;
    }
    let limit = ctx.input(|i| i.max_texture_side);
    let mut mode = world.resource_mut::<Mode2d>();
    let mut handles = Vec::new();
    if let Some(doc) = mode.doc.as_mut() {
        for image in &mut doc.images {
            let handle = image.pixels.clone().and_then(|pixels| {
                if pixels.size[0].max(pixels.size[1]) > limit {
                    image.error = Some(format!(
                        "{} is larger than the GPU's {limit} px texture limit",
                        image.path.display()
                    ));
                    image.pixels = None;
                    return None;
                }
                Some(ctx.load_texture(
                    format!("mode2d-{}", image.tileset),
                    pixels,
                    egui::TextureOptions::NEAREST,
                ))
            });
            handles.push(handle);
        }
    }
    world.insert_resource(TileTextures { generation, handles });
}

/// One tile on screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileQuad {
    /// Index of its layer in the map.
    pub layer: usize,
    /// Index of its tileset in the map.
    pub tileset: usize,
    /// Local tile id.
    pub local: u32,
    /// Top-left corner, pane pixels.
    pub min: Vec2,
    /// Bottom-right corner, pane pixels.
    pub max: Vec2,
    /// Texture coordinates of the corners, in the order top-left, top-right, bottom-right,
    /// bottom-left (image space: v down, in `0.0..=1.0`).
    pub uv: [[f32; 2]; 4],
    /// The layer's opacity.
    pub opacity: f32,
}

/// What a pane draws this frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewQuads {
    /// The tiles, in draw order.
    pub quads: Vec<TileQuad>,
    /// `N` when the view samples every Nth cell, else `1`.
    pub stride: u32,
}

/// Texture coordinates of the corners of a tile (top-left, top-right, bottom-right, bottom-left)
/// with the GID's flip bits, the mapping `sprite2d.wgsl` applies: the image operations are diagonal,
/// then mirrors, so sampling undoes them in the opposite order.
#[must_use]
pub fn corner_uvs(set: &TilesetDef, local: u32, raw: u32) -> Option<[[f32; 2]; 4]> {
    let [sx, sy, sw, sh] = set.tile_rect(local)?;
    let (iw, ih) = (set.image_width.max(1) as f32, set.image_height.max(1) as f32);
    let min = [sx as f32 / iw, sy as f32 / ih];
    let max = [(sx + sw) as f32 / iw, (sy + sh) as f32 / ih];
    let corner = |mut x: f32, mut y: f32| {
        if raw & FLIP_Y_BIT != 0 {
            y = 1.0 - y;
        }
        if raw & FLIP_X_BIT != 0 {
            x = 1.0 - x;
        }
        if raw & FLIP_DIAGONAL_BIT != 0 {
            std::mem::swap(&mut x, &mut y);
        }
        [min[0] + (max[0] - min[0]) * x, min[1] + (max[1] - min[1]) * y]
    };
    Some([
        corner(0.0, 0.0),
        corner(1.0, 0.0),
        corner(1.0, 1.0),
        corner(0.0, 1.0),
    ])
}

/// `v` as a cell index clamped to `0..=n` (f64: every column of a wide map stays exact).
fn clamp_cell(v: f64, n: u32) -> u32 {
    v.clamp(0.0, f64::from(n)) as u32
}

/// The visible tiles of the document in a front pane of `size` pixels. Bottom layer first.
#[must_use]
pub fn view_quads(mode: &Mode2d, view: &ViewCamera, size: Vec2) -> ViewQuads {
    let mut out = ViewQuads {
        quads: Vec::new(),
        stride: 1,
    };
    let Some(doc) = mode.doc.as_ref() else {
        return out;
    };
    let map = &doc.map;
    // Animated tiles show the frame the runtime would show at the pane clock's time.
    let time_ms = mode.clock.time_ms();
    let [w, h] = map.pixel_size().map(|v| v as f32);
    // A plane facing the camera projects affinely: two corners give the map → pane mapping, the
    // same `project` the grid uses, so tiles and grid lines agree to the pixel.
    let (Some(bottom_left), Some(top_right)) = (
        view.project(mode.origin, size),
        view.project(mode.origin + Vec3::new(w, h, 0.0), size),
    ) else {
        return out;
    };
    let (scale_x, scale_y) = (
        (top_right.x - bottom_left.x) / w,
        (top_right.y - bottom_left.y) / h,
    );
    if !(scale_x.is_finite() && scale_y.is_finite()) || scale_x <= 0.0 || scale_y >= 0.0 {
        return out;
    }
    let pane = |x: f32, y: f32| Vec2::new(bottom_left.x + x * scale_x, bottom_left.y + y * scale_y);
    // The pane in map pixels, with a margin for tiles larger than the grid.
    let margin = map
        .tilesets
        .iter()
        .map(|s| s.tile_width.max(s.tile_height))
        .max()
        .unwrap_or(0) as f32;
    let lo = Vec2::new(-bottom_left.x / scale_x, (size.y - bottom_left.y) / scale_y) - Vec2::splat(margin);
    let hi = Vec2::new((size.x - bottom_left.x) / scale_x, -bottom_left.y / scale_y) + Vec2::splat(margin);
    let (tw, th) = (f64::from(map.tile_width), f64::from(map.tile_height));
    // Cells of a layer drawn `offset` away from its grid position: `(x0, x1, y0, y1)`, rows from
    // the top as in the data.
    let cells = |offset: Vec2| {
        let (lo, hi) = (lo - offset, hi - offset);
        let x0 = clamp_cell((f64::from(lo.x) / tw).floor(), map.width);
        let x1 = clamp_cell((f64::from(hi.x) / tw).ceil(), map.width);
        let up0 = clamp_cell((f64::from(lo.y) / th).floor(), map.height);
        let up1 = clamp_cell((f64::from(hi.y) / th).ceil(), map.height);
        (x0, x1, map.height - up1, map.height - up0)
    };
    let shown = || {
        map.layers
            .iter()
            .enumerate()
            .filter_map(|(index, layer)| match layer {
                MapLayer::Tiles(layer) if layer.visible && layer.opacity > 0.0 => Some((index, layer)),
                _ => None,
            })
    };
    if !map.orientation.is_orthogonal() {
        // Isometric, staggered and hexagonal maps: every layer's visible cells in the
        // orientation's back-to-front order (`regia_2d::grid`), no stride sampling.
        let grid = map.grid_size();
        let map_h = map.orientation.pixel_size(grid)[1];
        'layers: for (index, layer) in shown() {
            let offset = Vec2::new(layer.offset[0], -layer.offset[1]);
            let (vlo, vhi) = (lo - offset, hi - offset);
            let (x0, x1, y0, y1) = map.orientation.visible_cells(
                grid,
                [f64::from(vlo.x), map_h - f64::from(vhi.y)],
                [f64::from(vhi.x), map_h - f64::from(vlo.y)],
            );
            let mut full = false;
            map.orientation.draw_order(grid, x0, x1, y0, y1, |x, y| {
                let Some(&raw) = layer.data.get(y as usize * map.width as usize + x as usize) else {
                    return true;
                };
                let (gid, _) = split_gid(raw);
                let Some((tileset, local)) = map.tileset_for(gid) else {
                    return true;
                };
                let set = &map.tilesets[tileset];
                let local = animated_tile(set, local, time_ms);
                let Some(uv) = corner_uvs(set, local, raw) else {
                    return true;
                };
                let ([left, bottom], _) = map.tile_world_rect(x, y);
                let at = Vec2::new(left as f32, bottom as f32) + offset;
                let (qw, qh) = (set.tile_width as f32, set.tile_height as f32);
                out.quads.push(TileQuad {
                    layer: index,
                    tileset,
                    local,
                    min: pane(at.x, at.y + qh),
                    max: pane(at.x + qw, at.y),
                    uv,
                    opacity: layer.opacity.min(1.0),
                });
                full = out.quads.len() >= MAX_VIEW_QUADS as usize;
                !full
            });
            if full {
                break 'layers;
            }
        }
        return out;
    }
    let total: u64 = shown()
        .map(|(_, layer)| {
            let (x0, x1, y0, y1) = cells(Vec2::new(layer.offset[0], -layer.offset[1]));
            u64::from(x1 - x0) * u64::from(y1 - y0)
        })
        .sum();
    let stride = (1u32..)
        .find(|&s| total / (u64::from(s) * u64::from(s)) < MAX_VIEW_QUADS)
        .unwrap_or(1);
    out.stride = stride;
    for (index, layer) in shown() {
        let offset = Vec2::new(layer.offset[0], -layer.offset[1]);
        let (x0, x1, y0, y1) = cells(offset);
        // Sampled cells sit on multiples of the stride, so they do not swim as the view pans.
        for y in ((y0 - y0 % stride)..y1).step_by(stride as usize) {
            for x in ((x0 - x0 % stride)..x1).step_by(stride as usize) {
                let Some(&raw) = layer.data.get(y as usize * map.width as usize + x as usize) else {
                    continue;
                };
                let (gid, _) = split_gid(raw);
                let Some((tileset, local)) = map.tileset_for(gid) else {
                    continue;
                };
                let set = &map.tilesets[tileset];
                let local = animated_tile(set, local, time_ms);
                let Some(uv) = corner_uvs(set, local, raw) else {
                    continue;
                };
                let last_row = (y + stride - 1).min(map.height - 1);
                let ([left, bottom], _) = map.tile_world_rect(x, last_row);
                let scale = stride as f32;
                let at = Vec2::new(left as f32, bottom as f32) + offset;
                let (qw, qh) = (set.tile_width as f32 * scale, set.tile_height as f32 * scale);
                out.quads.push(TileQuad {
                    layer: index,
                    tileset,
                    local,
                    min: pane(at.x, at.y + qh),
                    max: pane(at.x + qw, at.y),
                    uv,
                    opacity: layer.opacity.min(1.0),
                });
            }
        }
    }
    out
}

/// A flat color for a tile whose image is missing, stable per tile id.
fn flat_color(local: u32) -> egui::Color32 {
    let hash = local.wrapping_add(1).wrapping_mul(0x9E37_79B1);
    let channel = |shift: u32| 96 + ((hash >> shift) & 0x7F) as u8;
    egui::Color32::from_rgb(channel(24), channel(16), channel(8))
}

fn push_quad(
    mesh: &mut Mesh,
    quad: &TileQuad,
    origin: egui::Pos2,
    uv: Option<[[f32; 2]; 4]>,
    tint: egui::Color32,
) {
    let base = mesh.vertices.len() as u32;
    let corners = [
        (quad.min.x, quad.min.y),
        (quad.max.x, quad.min.y),
        (quad.max.x, quad.max.y),
        (quad.min.x, quad.max.y),
    ];
    for (corner, (x, y)) in corners.iter().enumerate() {
        mesh.vertices.push(Vertex {
            pos: origin + egui::vec2(*x, *y),
            uv: uv.map_or(egui::epaint::WHITE_UV, |uv| {
                egui::pos2(uv[corner][0], uv[corner][1])
            }),
            color: tint,
        });
    }
    mesh.add_triangle(base, base + 1, base + 2);
    mesh.add_triangle(base, base + 2, base + 3);
}

/// Paints `quads` into `rect`, one mesh per layer and tileset, in layer order.
pub fn paint_quads(
    painter: &egui::Painter,
    rect: egui::Rect,
    quads: &[TileQuad],
    textures: Option<&TileTextures>,
) {
    let mut meshes: Vec<((usize, usize), Mesh)> = Vec::new();
    for quad in quads {
        let key = (quad.layer, quad.tileset);
        let texture = textures.and_then(|t| t.get(quad.tileset));
        let at = match meshes.iter().position(|(k, _)| *k == key) {
            Some(at) => at,
            None => {
                let mesh = texture.map_or_else(Mesh::default, |t| Mesh::with_texture(t.id()));
                meshes.push((key, mesh));
                meshes.len() - 1
            }
        };
        let alpha = (quad.opacity * 255.0).round() as u8;
        let (uv, tint) = match texture {
            Some(_) => (Some(quad.uv), egui::Color32::from_white_alpha(alpha)),
            None => (None, flat_color(quad.local).gamma_multiply_u8(alpha)),
        };
        push_quad(&mut meshes[at].1, quad, rect.left_top(), uv, tint);
    }
    for (_, mesh) in meshes {
        painter.add(egui::Shape::mesh(mesh));
    }
}
