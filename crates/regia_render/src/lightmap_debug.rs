// SPDX-License-Identifier: MPL-2.0
//! Lightmap debug views (`specs/lightmaps.md` § 8 phase 4, ADR-2656): the frame's meshes swapped for
//! debug twins, like the surface modes of [`crate::debug_surface`], so they work on every path and
//! platform with no shader change.
//!
//! - **Density** ([`DebugViewMode::LightmapDensity`]): a checker with one cell per lightmap texel,
//!   coloured by the entry's density scale `k` ([`entry_density_scale`], [`density_color`]).
//! - **Invalid texels** ([`DebugViewMode::LightmapInvalid`]): baked texels grey, the texels the bake
//!   had to fill from their neighbours ([`regia_asset::lightmap::Coverage::DILATED`]) magenta. A
//!   gutter is dilated too, but no triangle samples it.
//! - **Lightmap only** ([`DebugViewMode::LightmapOnly`]): a receiver drawn as a white rough surface on
//!   its usual lightmap path, the sun, the local lights and the sky light off, through the frame's
//!   exposure. The capsule occluders still show, since they multiply the same term.
//!
//! Everything that is not a receiver draws flat [`NON_RECEIVER_COLOR`], so a mesh that lost its
//! lightmap is plain to see. Density and invalid texels read the file (they need
//! [`crate::lightmap::InstalledLightmap`] only); lightmap only needs [`LightmapSettings::enabled`], and without it the
//! receivers draw flat too, because there would be no lightmap light to show.
//!
//! The atlas views are one texture per lightmap and view, up to four times the atlas size so the
//! material's bilinear filter does not blur a cell (at most [`MAX_DEBUG_TEXTURE`] a side when it is
//! upscaled; an atlas over that keeps its own size and its cells blur at their edges).

use std::sync::{Arc, Mutex, OnceLock, Weak};

use regia_app::prelude::Entity;
use regia_asset::lightmap::{Coverage, Lightmap};

use crate::debug::DebugViewMode;
use crate::debug_surface::{Kind, cached_twin, hash_u32s, material_id_twin, twin_of, unlit_material};
use crate::lab_frame::{LabMaterial, LabTexture, LabVertex};
use crate::lightmap::LightmapSettings;
use crate::runtime::{CpuMesh, ExtractedFrame, ExtractedMesh};

/// What is not a receiver draws this flat colour (display-referred RGB).
pub const NON_RECEIVER_COLOR: [f32; 3] = [0.16, 0.16, 0.16];
/// Density view: the atlas outside every entry's rectangle (sRGB bytes).
pub const OUTSIDE_ENTRY: [u8; 3] = [24, 24, 24];
/// Invalid view: a texel the bake traced (sRGB bytes).
pub const BAKED_TEXEL: [u8; 3] = [128, 128, 128];
/// Invalid view: a texel the bake filled from its neighbours (sRGB bytes).
pub const INVALID_TEXEL: [u8; 3] = [235, 30, 200];
/// Invalid view: a texel no surface covers (sRGB bytes).
pub const EMPTY_TEXEL: [u8; 3] = [16, 16, 16];
/// The density palette by `log2(k)` from −2 up: at most 1/4 blue, 1/2 cyan, 1 green, 2 yellow,
/// 4 orange, at least 8 red (sRGB bytes).
pub const DENSITY_PALETTE: [[u8; 3]; 6] = [
    [40, 90, 255],
    [40, 200, 230],
    [60, 200, 70],
    [235, 210, 50],
    [245, 140, 40],
    [235, 50, 40],
];
/// Density view: the darker checker cell, in 1/256 of the cell's colour.
pub const DARK_CELL_SCALE: u16 = 140;
/// Largest side of a debug atlas texture, texels.
pub const MAX_DEBUG_TEXTURE: u32 = 4096;
/// Debug atlas pixels per lightmap texel, at most.
const MAX_UPSCALE: u32 = 4;

/// Whether `mode` is one of the lightmap views.
#[must_use]
pub fn handles(mode: DebugViewMode) -> bool {
    matches!(
        mode,
        DebugViewMode::LightmapDensity | DebugViewMode::LightmapOnly | DebugViewMode::LightmapInvalid
    )
}

/// The `k` of `specs/lightmaps.md` § 3 an entry ended up with: the texels per metre it got over the
/// density the bake asked for. 1 when the file has no requested density.
#[must_use]
pub fn entry_density_scale(map: &Lightmap, entry: usize) -> f32 {
    match map.entries.get(entry) {
        Some(e) if map.density > 0.0 && e.texels_per_metre > 0.0 => e.texels_per_metre / map.density,
        _ => 1.0,
    }
}

/// The density palette's colour of a density scale, snapped to the nearest power of two.
#[must_use]
pub fn density_color(k: f32) -> [u8; 3] {
    let step = if k > 0.0 && k.is_finite() {
        k.log2().round().clamp(-2.0, 3.0) as i32
    } else {
        0
    };
    DENSITY_PALETTE[(step + 2) as usize]
}

fn dark_cell(color: [u8; 3]) -> [u8; 3] {
    color.map(|c| ((u16::from(c) * DARK_CELL_SCALE) >> 8) as u8)
}

/// One colour per atlas texel of a view, row-major (sRGB bytes). Empty unless `mode` is the density
/// or the invalid view.
#[must_use]
pub fn lightmap_debug_atlas(map: &Lightmap, mode: DebugViewMode) -> Vec<[u8; 3]> {
    let (w, h) = (map.width as usize, map.height as usize);
    match mode {
        DebugViewMode::LightmapDensity => {
            // Every page, page after page as the texels (ADR-3300).
            let mut out = vec![OUTSIDE_ENTRY; w * h * map.pages() as usize];
            for (k, entry) in map.entries.iter().enumerate() {
                let color = density_color(entry_density_scale(map, k));
                let dark = dark_cell(color);
                let [x0, y0, ew, eh] = entry.rect.map(|v| v as usize);
                let base = entry.page as usize * w * h;
                for y in y0..(y0 + eh).min(h) {
                    for x in x0..(x0 + ew).min(w) {
                        out[base + y * w + x] = if (x + y).is_multiple_of(2) { color } else { dark };
                    }
                }
            }
            out
        }
        DebugViewMode::LightmapInvalid => map
            .coverage
            .iter()
            .map(|c| match *c {
                Coverage::BAKED => BAKED_TEXEL,
                Coverage::DILATED => INVALID_TEXEL,
                _ => EMPTY_TEXEL,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Page `page` of `colors` (one per atlas texel, page after page) as a texture, each texel a square
/// of pixels.
fn atlas_texture(map: &Lightmap, colors: &[[u8; 3]], page: u32) -> LabTexture {
    let side = map.width.max(map.height).max(1);
    let scale = (MAX_DEBUG_TEXTURE / side).clamp(1, MAX_UPSCALE);
    let (w, h) = (map.width as usize, map.height as usize);
    let s = scale as usize;
    let at = page as usize * w * h;
    let colors = colors.get(at..at + w * h).unwrap_or_default();
    let mut rgba = Vec::with_capacity(w * h * s * s * 4);
    for row in colors.chunks_exact(w.max(1)) {
        for _ in 0..s {
            for color in row {
                for _ in 0..s {
                    rgba.extend_from_slice(&[color[0], color[1], color[2], 255]);
                }
            }
        }
    }
    LabTexture {
        width: map.width * scale,
        height: map.height * scale,
        rgba,
    }
}

/// One view's materials over one page of one lightmap: `[single sided, double sided]`.
struct AtlasMaterials {
    map: Weak<Lightmap>,
    mode: DebugViewMode,
    page: u32,
    materials: [Arc<LabMaterial>; 2],
}

/// The last views built: at most the two atlas views of the current lightmap are kept.
static ATLAS_MATERIALS: Mutex<Vec<AtlasMaterials>> = Mutex::new(Vec::new());

fn atlas_materials(map: &Arc<Lightmap>, mode: DebugViewMode, page: u32) -> [Arc<LabMaterial>; 2] {
    let mut held = ATLAS_MATERIALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    held.retain(|m| m.map.upgrade().is_some_and(|m| Arc::ptr_eq(&m, map)));
    if let Some(found) = held.iter().find(|m| m.mode == mode && m.page == page) {
        return found.materials.clone();
    }
    let _span = tracing::info_span!("lightmap_debug_atlas", page).entered();
    let texture = atlas_texture(map, &lightmap_debug_atlas(map, mode), page);
    let materials = [
        unlit_material(Some(texture.clone()), false),
        unlit_material(Some(texture), true),
    ];
    held.push(AtlasMaterials {
        map: Arc::downgrade(map),
        mode,
        page,
        materials: materials.clone(),
    });
    materials
}

/// A receiver's white surface: every texture and factor the bake's light would be read through
/// taken out (`[single sided, double sided]`).
fn white_materials() -> &'static [Arc<LabMaterial>; 2] {
    static WHITE: OnceLock<[Arc<LabMaterial>; 2]> = OnceLock::new();
    WHITE.get_or_init(|| {
        [false, true].map(|double_sided| {
            Arc::new(LabMaterial {
                base_color: [1.0; 4],
                metallic: 0.0,
                roughness: 1.0,
                double_sided,
                ..LabMaterial::default()
            })
        })
    })
}

fn flat_twin(extracted: &ExtractedMesh) -> ExtractedMesh {
    material_id_twin(extracted, NON_RECEIVER_COLOR)
}

/// The receiver mesh with its UV2 moved to atlas space in the UV0 slot, so the atlas texture reads
/// it on any path. `None` when the UV2 does not fit the vertices.
fn atlas_twin(
    extracted: &ExtractedMesh,
    map: &Arc<Lightmap>,
    entry: usize,
    mode: DebugViewMode,
) -> Option<ExtractedMesh> {
    let e = map.entries.get(entry)?;
    let uv2 = extracted.uv1.as_ref()?;
    if uv2.len() != extracted.mesh.vertices.len() {
        return None;
    }
    let param = hash_u32s(e.scale_offset.map(f32::to_bits));
    let mesh = cached_twin(extracted, Kind::LightmapAtlas, param, || {
        let vertices: Vec<LabVertex> = extracted
            .mesh
            .vertices
            .iter()
            .zip(uv2.iter())
            .map(|(v, uv)| LabVertex {
                uv: [
                    uv[0] * e.scale_offset[0] + e.scale_offset[2],
                    uv[1] * e.scale_offset[1] + e.scale_offset[3],
                ],
                color: [1.0; 4],
                ..*v
            })
            .collect();
        CpuMesh {
            vertices,
            indices: extracted.mesh.indices.clone(),
        }
    });
    let [single, double] = atlas_materials(map, mode, e.page);
    let material = if extracted.material.double_sided {
        double
    } else {
        single
    };
    Some(twin_of(extracted, mesh, material, true))
}

/// Replaces `meshes` with their twins for a lightmap view. `entry_of` names the entry an entity draws
/// with ([`crate::lightmap::AppliedLightmapReceiver::entry`]). `None` when `mode` is not one (the
/// frame draws as is).
#[must_use]
pub fn lightmap_debug_meshes(
    meshes: &[ExtractedMesh],
    mode: DebugViewMode,
    map: Option<&Arc<Lightmap>>,
    settings: LightmapSettings,
    entry_of: &dyn Fn(Entity) -> Option<u32>,
) -> Option<Vec<ExtractedMesh>> {
    if !handles(mode) {
        return None;
    }
    let _span = tracing::info_span!("lightmap_debug_meshes", meshes = meshes.len()).entered();
    let twin = |extracted: &ExtractedMesh| {
        let received = map.zip(entry_of(extracted.entity));
        let drawn = match (mode, received) {
            (DebugViewMode::LightmapOnly, Some(_)) if settings.enabled => {
                let mut white = extracted.clone();
                let [single, double] = white_materials();
                white.material = Arc::clone(if extracted.material.double_sided {
                    double
                } else {
                    single
                });
                Some(white)
            }
            (DebugViewMode::LightmapOnly, _) | (_, None) => None,
            (_, Some((map, entry))) => atlas_twin(extracted, map, entry as usize, mode),
        };
        drawn.unwrap_or_else(|| flat_twin(extracted))
    };
    Some(meshes.iter().map(twin).collect())
}

/// Applies a lightmap view to an extracted frame: the twins, and for lightmap only no sun, no local
/// lights, no sky light (neither the sky's image-based light nor the flat ambient that stands in
/// for it) and no air (fog, the atmosphere's aerial perspective, clouds, mist), so the white surface
/// shows the baked light alone. Does nothing for another mode.
pub fn apply_lightmap_debug_view(
    frame: &mut ExtractedFrame,
    mode: DebugViewMode,
    map: Option<&Arc<Lightmap>>,
    settings: LightmapSettings,
    entry_of: &dyn Fn(Entity) -> Option<u32>,
) {
    let Some(meshes) = lightmap_debug_meshes(&frame.meshes, mode, map, settings, entry_of) else {
        return;
    };
    frame.meshes = meshes;
    if mode == DebugViewMode::LightmapOnly {
        frame.directional = None;
        frame.directional_shadows = false;
        frame.local_lights.clear();
        // The live sky's specular on the white surface would stand beside the baked light: at a
        // daylight sky over a dim bake it is all the view showed (ADR-2840).
        frame.environment.ibl = false;
        frame.environment.ambient_lux = 0.0;
        // Nor the air: the atmosphere's aerial perspective, the distance fog, the froxel fog, the
        // clouds and the mist add daylight in-scatter over every pixel, and over a dim bake they
        // tinted the white receiver (and the flat non-receivers) sky-blue with no baked light at all
        // (ADR-2840).
        frame.fog.enabled = false;
        frame.quality.physical_atmosphere = false;
        frame.quality.volumetric_fog = false;
        frame.quality.volumetric_clouds = false;
        frame.valley_mist = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::lightmap::{LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap};

    /// Two entries side by side in a 4 × 2 atlas, the second at twice the requested density, with
    /// one texel of the first filled by the dilation and one of the second empty.
    fn map() -> Lightmap {
        let entry = |name: &str, x: u32, tpm: f32| LightmapEntry {
            name: name.into(),
            source_vertices: 3,
            rect: [x, 0, 2, 2],
            scale_offset: [0.5, 1.0, x as f32 / 4.0, 0.0],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: 1,
            area_m2: 1.0,
            texels_per_metre: tpm,
            key: None,
            seams: None,
            page: 0,
        };
        let unwrap = MeshUnwrap {
            xref: vec![0, 1, 2],
            uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: vec![0, 1, 2],
        };
        let mut coverage = vec![Coverage::BAKED; 8];
        coverage[1] = Coverage::DILATED;
        coverage[7] = Coverage::EMPTY;
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 2,
                density: 8.0,
                padding: 0,
                entries: vec![entry("a", 0, 8.0), entry("b", 2, 16.0)],
                unwraps: vec![unwrap.clone(), unwrap],
            },
            &[[1.0; 3]; 8],
            coverage,
            LightmapBakeInfo::default(),
        )
        .expect("valid")
    }

    #[test]
    fn the_density_scale_snaps_to_the_palette() {
        assert_eq!(density_color(1.0), DENSITY_PALETTE[2]);
        assert_eq!(density_color(1.3), DENSITY_PALETTE[2], "nearest power of two");
        assert_eq!(density_color(0.5), DENSITY_PALETTE[1]);
        assert_eq!(density_color(0.01), DENSITY_PALETTE[0], "at most 1/4");
        assert_eq!(density_color(8.0), DENSITY_PALETTE[5]);
        assert_eq!(density_color(64.0), DENSITY_PALETTE[5], "at least 8");
        assert_eq!(density_color(f32::NAN), DENSITY_PALETTE[2]);
        let m = map();
        assert_eq!(entry_density_scale(&m, 0), 1.0);
        assert_eq!(entry_density_scale(&m, 1), 2.0);
        assert_eq!(entry_density_scale(&m, 9), 1.0, "no such entry");
    }

    #[test]
    fn the_density_atlas_is_a_checker_coloured_by_k() {
        let m = map();
        let atlas = lightmap_debug_atlas(&m, DebugViewMode::LightmapDensity);
        assert_eq!(atlas.len(), 8);
        // Entry a (k = 1), row 0: light, dark. Entry b (k = 2): yellow.
        assert_eq!(atlas[0], DENSITY_PALETTE[2]);
        assert_eq!(atlas[1], dark_cell(DENSITY_PALETTE[2]));
        assert_eq!(atlas[4], dark_cell(DENSITY_PALETTE[2]), "row 1 starts dark");
        assert_eq!(atlas[2], DENSITY_PALETTE[3]);
        assert_eq!(atlas[3], dark_cell(DENSITY_PALETTE[3]));
        assert_ne!(atlas[0], atlas[1], "the cells differ");
    }

    /// A paged lightmap (ADR-3300): the density view covers every page, and each page's texture
    /// shows that page's entries only.
    #[test]
    fn the_density_view_covers_every_page() {
        let one = map();
        let mut paged = one.clone();
        paged.entries[1].page = 1;
        paged.texels.extend(one.texels.clone());
        paged.coverage.extend(one.coverage.clone());
        paged.validate().expect("two pages");
        let atlas = lightmap_debug_atlas(&paged, DebugViewMode::LightmapDensity);
        assert_eq!(atlas.len(), 16);
        assert_eq!(atlas[2], OUTSIDE_ENTRY, "page 0 no longer holds b");
        assert_eq!(
            atlas[8 + 2],
            DENSITY_PALETTE[3],
            "page 1 holds b at its rectangle"
        );
        let t1 = atlas_texture(&paged, &atlas, 1);
        let t0 = atlas_texture(&paged, &atlas, 0);
        assert_eq!((t1.width, t1.height), (t0.width, t0.height));
        assert_ne!(t0.rgba, t1.rgba);
        let empty = atlas_texture(&paged, &atlas, 2);
        assert!(empty.rgba.is_empty(), "past the last page");
    }

    #[test]
    fn the_invalid_atlas_marks_the_dilated_texels() {
        let atlas = lightmap_debug_atlas(&map(), DebugViewMode::LightmapInvalid);
        assert_eq!(atlas[0], BAKED_TEXEL);
        assert_eq!(atlas[1], INVALID_TEXEL);
        assert_eq!(atlas[7], EMPTY_TEXEL);
    }

    #[test]
    fn the_texture_holds_each_texel_as_a_square_of_pixels() {
        let m = map();
        let colors = lightmap_debug_atlas(&m, DebugViewMode::LightmapInvalid);
        let t = atlas_texture(&m, &colors, 0);
        assert_eq!((t.width, t.height), (16, 8));
        assert_eq!(t.rgba.len(), 16 * 8 * 4);
        let at = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [t.rgba[i], t.rgba[i + 1], t.rgba[i + 2]]
        };
        assert_eq!(at(0, 0), BAKED_TEXEL);
        assert_eq!(at(4, 3), INVALID_TEXEL, "texel 1, row 0, last pixel row");
        assert_eq!(at(7, 0), INVALID_TEXEL);
        assert_eq!(at(8, 0), BAKED_TEXEL);
        assert_eq!(at(15, 7), EMPTY_TEXEL);
    }

    #[test]
    fn a_large_atlas_is_not_upscaled_past_the_limit() {
        let side = (MAX_DEBUG_TEXTURE / MAX_UPSCALE) * 3;
        let scale = (MAX_DEBUG_TEXTURE / side).clamp(1, MAX_UPSCALE);
        assert_eq!(scale, 1);
    }

    #[test]
    fn lightmap_only_takes_every_live_light_out() {
        let mut frame = ExtractedFrame {
            directional: Some(crate::DirectionalLightUniform::new(
                [0.0, -1.0, 0.0],
                [1.0; 3],
                10.0,
            )),
            directional_shadows: true,
            ..ExtractedFrame::default()
        };
        frame.fog.enabled = true;
        frame.quality.physical_atmosphere = true;
        frame.quality.volumetric_fog = true;
        frame.quality.volumetric_clouds = true;
        frame.valley_mist = Some(crate::ValleyMist::default());
        assert!(frame.environment.ibl && frame.environment.ambient_lux > 0.0);
        let mut lit = frame.clone();
        apply_lightmap_debug_view(
            &mut lit,
            DebugViewMode::Lit,
            None,
            LightmapSettings::default(),
            &|_| None,
        );
        assert!(
            lit.directional.is_some() && lit.environment.ibl,
            "another mode keeps its lights"
        );
        assert!(
            lit.fog.enabled && lit.quality.physical_atmosphere && lit.valley_mist.is_some(),
            "another mode keeps its air"
        );
        apply_lightmap_debug_view(
            &mut frame,
            DebugViewMode::LightmapOnly,
            None,
            LightmapSettings::default(),
            &|_| None,
        );
        assert!(frame.directional.is_none() && !frame.directional_shadows);
        assert!(frame.local_lights.is_empty());
        assert!(!frame.environment.ibl, "no sky light");
        assert_eq!(frame.environment.ambient_lux, 0.0, "no flat ambient either");
        assert!(!frame.fog.enabled, "no distance fog");
        assert!(
            !frame.quality.physical_atmosphere
                && !frame.quality.volumetric_fog
                && !frame.quality.volumetric_clouds,
            "no aerial perspective, froxel fog or clouds"
        );
        assert!(frame.valley_mist.is_none(), "no mist");
    }

    #[test]
    fn other_modes_are_left_alone() {
        assert!(handles(DebugViewMode::LightmapDensity));
        assert!(!handles(DebugViewMode::Lit));
        assert!(lightmap_debug_atlas(&map(), DebugViewMode::Lit).is_empty());
        assert!(
            lightmap_debug_meshes(
                &[],
                DebugViewMode::Lit,
                None,
                LightmapSettings::default(),
                &|_| None
            )
            .is_none()
        );
    }
}
