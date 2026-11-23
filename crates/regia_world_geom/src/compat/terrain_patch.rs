// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/terrain_patch.rs, commit d6ed37677), ADR-8012.
// Re-cited by W-02 (ADR-8167) at commit e6c84ba6d (tag regia-absorb-2): the file is unchanged there.
// Lines all, mark: as-is.
// Stage A (spec 121 § 4, ADR-8050): arithmetic unchanged. Edits: module paths, doc comments (DO27
// file and engine names dropped, missing docs added), lint attributes, visibility of helpers the
// sibling compat modules call.

//! Terrain patches: the drawn ground where it differs from the heightfield (TUNNELS_FABLE_1013).
//!
//! The Switchbacks tunnel mouths are drawn as a hole in the terrain filled by the hero portal
//! meshes: a funnel of rock 36 to 46 m wide down to the 20 m ramp. The sim heightfield stayed
//! flat across that hole, so a pilot or a hull on the sim's ground stood in mid-air over the
//! drawn pit, and a hull crossing the hole edge fell through drawn rock onto the ramp. A patch
//! is a small grid of heights baked from those meshes (`art/generators/map/mouth_surface.py`)
//! and stored beside the map (`assets_regia/maps/<map>/mouth_surface.json`). Every peer reads
//! the same file, and [`crate::compat::sampler::TerrainSampler::height_at_world`] answers from the patch inside
//! it, so every consumer of terrain height (ground clamp, hover, pathing, camera, pilots,
//! spawn) sees the drawn surface.
//!
//! A patch only lowers the ground: cells above the heightfield are left out at bake time, so
//! lintels, slabs and kit rocks stay what they were (pilot solids, set-piece decks).
use std::path::Path;

use serde::Deserialize;

/// File name under `assets_regia/maps/<map>/`.
pub const PATCH_FILE: &str = "mouth_surface.json";

/// "No override" cell. A finite sentinel keeps `PartialEq` on the sampler exact.
pub const NONE_M: f32 = -1.0e9;

/// One rectangular grid of ground heights, row-major with `cols` cells per row (a row is a line
/// of constant z). `origin` is the world XZ of cell (0, 0); cell (c, r) is at
/// `origin + (c, r) * spacing`.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainPatch {
    /// Patch id; a `*_skylight` patch is a skylight collapse.
    pub id: String,
    /// World XZ of cell (0, 0).
    pub origin: [f32; 2],
    /// Cell size, metres.
    pub spacing: f32,
    /// Cells per row.
    pub cols: usize,
    /// Rows.
    pub rows: usize,
    /// Heights, row-major; [`NONE_M`] where the patch leaves the grid alone.
    pub heights: Vec<f32>,
}

#[derive(Deserialize)]
struct PatchFile {
    #[serde(default)]
    spacing_m: f32,
    #[serde(default)]
    patches: Vec<PatchRow>,
}

#[derive(Deserialize)]
struct PatchRow {
    id: String,
    origin: [f32; 2],
    cols: usize,
    rows: usize,
    heights: Vec<Option<f32>>,
    #[serde(default)]
    spacing_m: Option<f32>,
}

impl TerrainPatch {
    #[inline]
    fn cell(&self, c: usize, r: usize) -> Option<f32> {
        let h = *self.heights.get(r * self.cols + c)?;
        (h > NONE_M + 1.0 && h.is_finite()).then_some(h)
    }

    /// Patched ground at a world point: bilinear where the four cells round it are set, the
    /// nearest set cell at the patch's ragged edge, `None` outside the patch or over unset cells.
    pub fn sample(&self, x: f32, z: f32) -> Option<f32> {
        if self.cols < 2 || self.rows < 2 || self.spacing <= 0.0 {
            return None;
        }
        let fx = (x - self.origin[0]) / self.spacing;
        let fz = (z - self.origin[1]) / self.spacing;
        if fx < 0.0 || fz < 0.0 || fx > (self.cols - 1) as f32 || fz > (self.rows - 1) as f32 {
            return None;
        }
        let c0 = (fx.floor() as usize).min(self.cols - 2);
        let r0 = (fz.floor() as usize).min(self.rows - 2);
        let tx = fx - c0 as f32;
        let tz = fz - r0 as f32;
        let corners = [
            self.cell(c0, r0),
            self.cell(c0 + 1, r0),
            self.cell(c0, r0 + 1),
            self.cell(c0 + 1, r0 + 1),
        ];
        if let [Some(h00), Some(h10), Some(h01), Some(h11)] = corners {
            let top = h00 + (h10 - h00) * tx;
            let bottom = h01 + (h11 - h01) * tx;
            return Some(top + (bottom - top) * tz);
        }
        // Ragged edge: the nearest set corner, so the drawn funnel wall meets the plain without a
        // gap a body could stand in.
        let nearest = [
            (c0, r0, tx, tz),
            (c0 + 1, r0, 1.0 - tx, tz),
            (c0, r0 + 1, tx, 1.0 - tz),
            (c0 + 1, r0 + 1, 1.0 - tx, 1.0 - tz),
        ];
        let mut best: Option<(f32, f32)> = None;
        for (c, r, dx, dz) in nearest {
            if let Some(h) = self.cell(c, r) {
                let d = dx * dx + dz * dz;
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, h));
                }
            }
        }
        best.filter(|(d, _)| *d <= 0.25).map(|(_, h)| h)
    }

    /// The patch's world rectangle `(min_x, min_z, max_x, max_z)`.
    pub fn bounds(&self) -> (f32, f32, f32, f32) {
        (
            self.origin[0],
            self.origin[1],
            self.origin[0] + (self.cols.saturating_sub(1)) as f32 * self.spacing,
            self.origin[1] + (self.rows.saturating_sub(1)) as f32 * self.spacing,
        )
    }
}

/// Parse a patch file. Rows whose `heights` length does not match `cols * rows` are skipped.
pub fn parse_patches(text: &str) -> Result<Vec<TerrainPatch>, String> {
    let file: PatchFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in file.patches {
        let spacing = row.spacing_m.unwrap_or(file.spacing_m);
        if row.heights.len() != row.cols * row.rows || spacing <= 0.0 {
            return Err(format!(
                "patch {}: {} heights for {}x{} cells, spacing {spacing}",
                row.id,
                row.heights.len(),
                row.cols,
                row.rows
            ));
        }
        out.push(TerrainPatch {
            id: row.id,
            origin: row.origin,
            spacing,
            cols: row.cols,
            rows: row.rows,
            heights: row
                .heights
                .into_iter()
                .map(|h| h.filter(|v| v.is_finite()).unwrap_or(NONE_M))
                .collect(),
        });
    }
    Ok(out)
}

/// The patches for map `id` under `root` (`assets_regia/maps/<id>/mouth_surface.json`,
/// or `maps/<id>/mouth_surface.json` in the dedicated-server package), or an
/// empty list when the file is absent. A file that does not parse is reported on stderr and
/// ignored, so a bad bake never changes the ground under a match.
pub fn load_patches(root: &Path, id: &str) -> Vec<TerrainPatch> {
    let packaged = root.join("maps").join(id).join(PATCH_FILE);
    let path = if packaged.is_file() {
        packaged
    } else {
        root.join("assets_regia/maps").join(id).join(PATCH_FILE)
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match parse_patches(&text) {
        Ok(patches) => patches,
        Err(err) => {
            eprintln!("terrain patches: {} ignored ({err})", path.display());
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn patch() -> TerrainPatch {
        // 3 x 3 cells, 2 m apart, a bowl 10 m deep in the middle; one corner unset.
        TerrainPatch {
            id: "t".into(),
            origin: [10.0, 20.0],
            spacing: 2.0,
            cols: 3,
            rows: 3,
            heights: vec![0.0, 0.0, 0.0, 0.0, -10.0, 0.0, 0.0, 0.0, NONE_M],
        }
    }

    #[test]
    fn samples_bilinear_inside_and_none_outside() {
        let p = patch();
        assert_eq!(p.sample(12.0, 22.0), Some(-10.0));
        assert_eq!(p.sample(11.0, 22.0), Some(-5.0));
        assert_eq!(p.sample(10.0, 20.0), Some(0.0));
        assert_eq!(p.sample(9.9, 20.0), None);
        assert_eq!(p.sample(14.1, 24.0), None);
    }

    #[test]
    fn a_ragged_edge_uses_the_nearest_set_cell_and_a_far_unset_cell_is_none() {
        let p = patch();
        // The unset corner (14, 24): close to the set cell (12, 24) answers, the corner itself does not.
        assert_eq!(p.sample(12.5, 24.0), Some(0.0));
        assert_eq!(p.sample(13.9, 23.9), None);
    }

    #[test]
    fn parses_the_file_format_and_rejects_a_short_row() {
        let text = r#"{"spacing_m": 0.5, "patches": [{"id": "west", "origin": [-1.0, 2.0], "cols": 2, "rows": 2, "heights": [1.0, null, 3.0, 4.0]}]}"#;
        let p = parse_patches(text).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].spacing, 0.5);
        assert_eq!(p[0].heights[1], NONE_M);
        assert_eq!(p[0].sample(-1.0, 2.0), Some(1.0));
        let bad = r#"{"spacing_m": 0.5, "patches": [{"id": "w", "origin": [0, 0], "cols": 2, "rows": 2, "heights": [1.0]}]}"#;
        assert!(parse_patches(bad).is_err());
        assert!(load_patches(Path::new("/nonexistent-do27-root"), "switchbacks").is_empty());
    }
}
