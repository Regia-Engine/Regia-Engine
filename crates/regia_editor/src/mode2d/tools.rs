// SPDX-License-Identifier: MPL-2.0
//! The 2D mode's paint tools (`docs/specs/86-2d-toolkit.md` § 6, ADR-7853): line, rectangle
//! (filled or outline), bucket fill (contiguous or every matching cell), pick; the random scatter
//! brush; and the auto-tiling "path" mode, where a terrain stroke joins only the cells painted in
//! the same stroke.
//!
//! Everything here is pure cell arithmetic over a `TileMapAsset`, so the pointer path, the 2D tab
//! and `editor.tile_stroke` share it.

use std::collections::{BTreeMap, BTreeSet};

use regia_2d::autotile::{BoundAutoTiles, TerrainBrush, TileChange};
use regia_2d::tilemap::{GID_MASK, MapLayer, TileMapAsset};

/// Most cells one bucket fill, rectangle or line may touch (`MAX_STROKE_CELLS` of the mode).
pub const MAX_TOOL_CELLS: usize = 65_536;
/// Most tiles one random brush holds.
pub const MAX_RANDOM_TILES: usize = 256;

/// How a press in the front pane edits the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool2d {
    /// Freehand: every cell the pointer crosses (the line between samples is filled).
    #[default]
    Paint,
    /// A straight line from the press to the release.
    Line,
    /// A rectangle from the press to the release corner.
    Rect,
    /// Fill the region of the pressed cell (see [`Mode2d::bucket_contiguous`](super::Mode2d)).
    Bucket,
    /// Click picks the cell's tile as the brush; a drag picks the region as the stamp.
    Pick,
}

impl Tool2d {
    /// Every tool, in toolbar order.
    pub const ALL: [Self; 5] = [Self::Paint, Self::Line, Self::Rect, Self::Bucket, Self::Pick];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Paint => "paint",
            Self::Line => "line",
            Self::Rect => "rect",
            Self::Bucket => "bucket",
            Self::Pick => "pick",
        }
    }

    /// The tool of a protocol name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The label the 2D tab shows (US English).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Paint => "Paint",
            Self::Line => "Line",
            Self::Rect => "Rectangle",
            Self::Bucket => "Bucket",
            Self::Pick => "Pick",
        }
    }

    /// Whether the tool paints a shape from the press to the release (the cells land on release).
    #[must_use]
    pub fn is_shape(self) -> bool {
        matches!(self, Self::Line | Self::Rect)
    }
}

/// How a terrain brush joins its neighbors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutotileMode {
    /// Joins every neighbor of the same terrain (the rule set's own behavior).
    #[default]
    Connect,
    /// Joins only the cells painted in the same stroke; the neighbors keep their tiles.
    Path,
}

impl AutotileMode {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Path => "path",
        }
    }

    /// The mode of a protocol name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "connect" => Some(Self::Connect),
            "path" => Some(Self::Path),
            _ => None,
        }
    }
}

/// One tile of the random brush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RandomTile {
    /// Raw GID (flip bits allowed).
    pub raw: u32,
    /// Relative weight, at least 1.
    pub weight: u32,
}

/// The random scatter brush: each painted cell gets one of `tiles` (by weight), or is left alone
/// with probability `nothing`. The pick is a hash of `seed` and the cell, so a stroke repeats
/// exactly for the same seed.
#[derive(Debug, Clone, PartialEq)]
pub struct RandomBrush {
    /// The tiles and their weights.
    pub tiles: Vec<RandomTile>,
    /// Chance of leaving a cell unpainted, `0.0..=1.0`.
    pub nothing: f32,
    /// The hash seed; the 2D tab moves it after every stroke.
    pub seed: u64,
}

impl RandomBrush {
    /// Checks the tiles (1..=256, weights at least 1) and `nothing` (finite, 0..=1).
    pub fn validate(&self) -> Result<(), String> {
        if self.tiles.is_empty() || self.tiles.len() > MAX_RANDOM_TILES {
            return Err(format!("the random brush holds 1..={MAX_RANDOM_TILES} tiles"));
        }
        if self.tiles.iter().any(|t| t.weight == 0) {
            return Err("every random tile weight is at least 1".into());
        }
        if self.tiles.iter().any(|t| t.raw & GID_MASK == 0) {
            return Err("a random tile is a gid, not 0".into());
        }
        if !(self.nothing.is_finite() && (0.0..=1.0).contains(&self.nothing)) {
            return Err("`nothing` is a chance between 0 and 1".into());
        }
        Ok(())
    }

    /// The raw GID for cell `(x, y)`, or `None` to leave it alone.
    #[must_use]
    pub fn pick(&self, x: u32, y: u32) -> Option<u32> {
        let hash = mix(self.seed ^ ((u64::from(x) << 32) | u64::from(y)));
        // The top 24 bits decide "nothing", the rest the tile: two independent draws.
        let roll = (hash >> 40) as f32 / (1u64 << 24) as f32;
        if roll < self.nothing {
            return None;
        }
        let total: u64 = self.tiles.iter().map(|t| u64::from(t.weight)).sum();
        let mut at = (hash & 0xFF_FFFF_FFFF) % total.max(1);
        for tile in &self.tiles {
            if at < u64::from(tile.weight) {
                return Some(tile.raw);
            }
            at -= u64::from(tile.weight);
        }
        self.tiles.last().map(|t| t.raw)
    }
}

/// SplitMix64's finalizer (Steele, Lea and Flood 2014): a cheap, well-mixed 64-bit hash.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The cells of the rectangle with corners `a` and `b` (any order): all of them when `filled`,
/// else its border. Sorted by `(y, x)`. `None` when the rectangle has more than
/// [`MAX_TOOL_CELLS`] cells: the tool then writes nothing, as the bucket does (a prefix of the
/// rectangle is never painted; review g17 row 6).
#[must_use]
pub fn rect_cells(a: (u32, u32), b: (u32, u32), filled: bool) -> Option<Vec<(u32, u32)>> {
    let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
    let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
    let (w, h) = (u64::from(x1 - x0) + 1, u64::from(y1 - y0) + 1);
    let count = if filled || w <= 2 || h <= 2 {
        w * h
    } else {
        2 * (w + h) - 4
    };
    if count > MAX_TOOL_CELLS as u64 {
        return None;
    }
    let mut out = Vec::new();
    for y in y0..=y1 {
        for x in x0..=x1 {
            if filled || x == x0 || x == x1 || y == y0 || y == y1 {
                out.push((x, y));
            }
        }
    }
    Some(out)
}

/// The bucket region of `start` on tile layer `layer`: the 4-connected cells holding the same raw
/// GID (flip bits included) when `contiguous`, else every such cell of the layer. Sorted by
/// `(y, x)`; at most [`MAX_TOOL_CELLS`] (`None` when the region is larger, or `start` is outside
/// the map or the layer is not a tile layer).
#[must_use]
pub fn bucket_cells(
    map: &TileMapAsset,
    layer: usize,
    start: (u32, u32),
    contiguous: bool,
) -> Option<Vec<(u32, u32)>> {
    let Some(MapLayer::Tiles(tiles)) = map.layers.get(layer) else {
        return None;
    };
    let (w, h) = (map.width, map.height);
    if start.0 >= w || start.1 >= h || tiles.data.len() as u64 != u64::from(w) * u64::from(h) {
        return None;
    }
    let at = |x: u32, y: u32| tiles.data[y as usize * w as usize + x as usize];
    let target = at(start.0, start.1);
    if !contiguous {
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                if at(x, y) == target {
                    out.push((x, y));
                    if out.len() > MAX_TOOL_CELLS {
                        return None;
                    }
                }
            }
        }
        return Some(out);
    }
    // Scan-free flood fill with an explicit stack (no recursion on large regions).
    let mut seen = BTreeSet::new();
    let mut stack = vec![start];
    seen.insert((start.1, start.0));
    while let Some((x, y)) = stack.pop() {
        let mut push = |nx: u32, ny: u32| {
            if at(nx, ny) == target && seen.insert((ny, nx)) {
                stack.push((nx, ny));
            }
        };
        if x > 0 {
            push(x - 1, y);
        }
        if x + 1 < w {
            push(x + 1, y);
        }
        if y > 0 {
            push(x, y - 1);
        }
        if y + 1 < h {
            push(x, y + 1);
        }
        if seen.len() > MAX_TOOL_CELLS {
            return None;
        }
    }
    Some(seen.into_iter().map(|(y, x)| (x, y)).collect())
}

/// Writes one raw GID per cell into tile layer `layer`; the changed cells sorted by `(y, x)`,
/// each once (the last value wins).
pub fn write_values(map: &mut TileMapAsset, layer: usize, values: &[((u32, u32), u32)]) -> Vec<TileChange> {
    let width = map.width as usize;
    let Some(MapLayer::Tiles(tiles)) = map.layers.get_mut(layer) else {
        return Vec::new();
    };
    let mut changed: BTreeMap<(u32, u32), (u32, u32)> = BTreeMap::new();
    for &((x, y), raw) in values {
        if let Some(cell) = tiles.data.get_mut(y as usize * width + x as usize)
            && *cell != raw
        {
            changed
                .entry((y, x))
                .and_modify(|e| e.1 = raw)
                .or_insert((*cell, raw));
            *cell = raw;
        }
    }
    changed
        .into_iter()
        .filter(|(_, (before, after))| before != after)
        .map(|((y, x), (before, after))| TileChange { x, y, before, after })
        .collect()
}

/// A terrain paint in path mode: `cells` take terrain `terrain`, and the cells of `joined` (the
/// stroke so far, `cells` included) re-resolve as if they were the only cells of that terrain on
/// the layer. Cells outside `joined` never change. Returns the changed cells sorted by `(y, x)`.
pub fn paint_path(
    bound: &BoundAutoTiles<'_>,
    map: &mut TileMapAsset,
    layer: usize,
    terrain: usize,
    cells: &[(u32, u32)],
    joined: &BTreeSet<(u32, u32)>,
) -> Result<Vec<TileChange>, String> {
    let width = map.width as usize;
    let real = match map.layers.get(layer) {
        Some(MapLayer::Tiles(t)) => t.data.clone(),
        _ => return Err(format!("layer {layer} is not a tile layer")),
    };
    // A scratch layer that shows only the stroke: everything else is empty, so the rules see no
    // other neighbor of the terrain.
    let scratch: Vec<u32> = real
        .iter()
        .enumerate()
        .map(|(i, &raw)| {
            let cell = ((i % width) as u32, (i / width) as u32);
            if joined.contains(&cell) { raw } else { 0 }
        })
        .collect();
    let swap = |map: &mut TileMapAsset, data: Vec<u32>| -> Vec<u32> {
        match map.layers.get_mut(layer) {
            Some(MapLayer::Tiles(t)) => std::mem::replace(&mut t.data, data),
            _ => Vec::new(),
        }
    };
    let _ = swap(map, scratch);
    let painted = bound.paint(map, layer, TerrainBrush::Terrain(terrain), cells);
    let resolved = swap(map, real);
    painted.map_err(|e| e.to_string())?;
    let values: Vec<((u32, u32), u32)> = joined
        .iter()
        .filter_map(|&(x, y)| {
            let i = y as usize * width + x as usize;
            resolved.get(i).map(|&raw| ((x, y), raw))
        })
        .collect();
    Ok(write_values(map, layer, &values))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_fill_or_outline_from_any_corner() {
        assert_eq!(rect_cells((2, 1), (0, 0), true).map(|c| c.len()), Some(6));
        let outline = rect_cells((0, 0), (3, 2), false).expect("small");
        assert_eq!(outline.len(), 10, "a 4x3 border has 10 cells");
        assert!(!outline.contains(&(1, 1)) && !outline.contains(&(2, 1)));
        assert_eq!(rect_cells((5, 5), (5, 5), false), Some(vec![(5, 5)]));
        assert_eq!(
            rect_cells((0, 0), (1, 4), false).map(|c| c.len()),
            Some(10),
            "a 2-wide border is all cells"
        );
    }

    /// Review g17 row 6: a filled rectangle past the cell cap is refused whole (it used to paint a
    /// top-left prefix of 65,536 cells as a successful stroke); its border still fits.
    #[test]
    fn a_rectangle_past_the_cell_cap_is_refused_whole() {
        assert_eq!(rect_cells((0, 0), (299, 299), true), None, "90,000 cells");
        assert_eq!(
            rect_cells((0, 0), (255, 255), true).map(|c| c.len()),
            Some(MAX_TOOL_CELLS)
        );
        assert_eq!(rect_cells((0, 0), (255, 256), true), None, "one row past the cap");
        assert_eq!(rect_cells((299, 299), (0, 0), false).map(|c| c.len()), Some(1196));
    }

    #[test]
    fn the_random_brush_is_deterministic_and_honors_weights_and_nothing() {
        let brush = RandomBrush {
            tiles: vec![RandomTile { raw: 1, weight: 3 }, RandomTile { raw: 2, weight: 1 }],
            nothing: 0.25,
            seed: 7,
        };
        brush.validate().expect("valid");
        let picks: Vec<Option<u32>> = (0..4000).map(|i| brush.pick(i % 64, i / 64)).collect();
        let again: Vec<Option<u32>> = (0..4000).map(|i| brush.pick(i % 64, i / 64)).collect();
        assert_eq!(picks, again, "same seed, same cells, same picks");
        let none = picks.iter().filter(|p| p.is_none()).count() as f32 / 4000.0;
        let ones = picks.iter().filter(|p| **p == Some(1)).count() as f32;
        let twos = picks.iter().filter(|p| **p == Some(2)).count() as f32;
        assert!((none - 0.25).abs() < 0.04, "chance of nothing {none}");
        assert!(
            (ones / twos - 3.0).abs() < 0.6,
            "weights 3:1, got {}",
            ones / twos
        );
        let other = RandomBrush {
            seed: 8,
            ..brush.clone()
        };
        assert_ne!(
            picks,
            (0..4000).map(|i| other.pick(i % 64, i / 64)).collect::<Vec<_>>()
        );
        let always = RandomBrush {
            nothing: 1.0,
            ..brush.clone()
        };
        assert!((0..100).all(|i| always.pick(i, 0).is_none()));
        for bad in [
            RandomBrush {
                tiles: Vec::new(),
                ..brush.clone()
            },
            RandomBrush {
                nothing: 1.5,
                ..brush.clone()
            },
            RandomBrush {
                nothing: f32::NAN,
                ..brush.clone()
            },
            RandomBrush {
                tiles: vec![RandomTile { raw: 1, weight: 0 }],
                ..brush.clone()
            },
            RandomBrush {
                tiles: vec![RandomTile { raw: 0, weight: 1 }],
                ..brush
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tool_and_mode_names_round_trip() {
        for tool in Tool2d::ALL {
            assert_eq!(Tool2d::parse(tool.name()), Some(tool));
        }
        assert_eq!(Tool2d::parse("lasso"), None);
        for mode in [AutotileMode::Connect, AutotileMode::Path] {
            assert_eq!(AutotileMode::parse(mode.name()), Some(mode));
        }
    }
}
