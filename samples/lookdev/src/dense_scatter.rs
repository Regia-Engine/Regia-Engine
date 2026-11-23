// SPDX-License-Identifier: MIT-0
//! Denser rock scatter on DO27's maps for the density bench (lane scatter-density).
//!
//! DO27's environment art is adding much denser ground scatter on Mars after the NASA photo
//! comparison (densely strewn stones, cracked bedrock, basalt). This module builds the
//! variants the bench measures without touching DO27's content:
//!
//! - [`BenchScatter::Scan`]: the scatter the DO27 shell ships, `scatter/<planet>.scatter.ron`
//!   with its CC0 scan-kit meshes (`scatter/<planet>/<class>/*.gltf`, three authored LODs and
//!   their own albedo / normal / AO maps), loaded read-only from the content drop.
//! - [`BenchScatter::Map`]: the map's derived `maps/<map>/scatter.ron` class summary with the
//!   procedural stand-ins (what the bench drew until now).
//! - [`densify`]: a lookdev variant of a set at `factor` × its instance count, the added
//!   instances skewed to many small stones plus some larger rocks.
//!
//! Each scan mesh's maps are decoded at most [`scan_texture_max`] texels on a side for the
//! largest instance its class places: a 0.5 m pebble never shows more than a few hundred
//! texels, a 3 m boulder close to the camera a thousand.

use std::path::Path;
use std::sync::Arc;

use regia_terrain::{
    DensityMaps, ScatterField, ScatterLibrary, ScatterMeshAsset, ScatterMeshLod, ScatterSet,
};

use crate::look_r2::R2Terrain;
use crate::{LookdevError, Planet};

/// Which scatter set the bench draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchScatter {
    /// `maps/<map>/scatter.ron` with procedural stand-ins (the bench's default).
    Map,
    /// `scatter/<planet>.scatter.ron` with DO27's scan-kit meshes (what the shell ships).
    Scan,
}

impl BenchScatter {
    /// `map` or `scan`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Map => "map",
            Self::Scan => "scan",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Map, Self::Scan].into_iter().find(|s| s.name() == name)
    }
}

/// LOD switch sizes of a three-LOD scan mesh (projected radius over the screen half-height),
/// the field's defaults for the first two LODs.
const SCAN_SCREEN_SIZES: [f32; 3] = [0.06, 0.018, 0.0];

/// Texels per metre of instance size the scan maps keep (power of two, 256..=1024 on a side).
const SCAN_TEXELS_PER_M: f32 = 256.0;

/// Largest texture edge a scan mesh keeps when its class places it up to `max_scale_m`
/// metres across: `SCAN_TEXELS_PER_M` per metre, a power of two in 256..=1024.
#[must_use]
pub fn scan_texture_max(max_scale_m: f32) -> u32 {
    let want = (max_scale_m.max(0.0) * SCAN_TEXELS_PER_M).ceil() as u32;
    want.next_power_of_two().clamp(256, 1024)
}

/// DO27's shipped scatter set for `planet` (`scatter/<planet>.scatter.ron`).
///
/// # Errors
///
/// The file is missing or does not parse against the map's layer stack.
pub fn scan_set(content: &Path, planet: Planet, terrain: &R2Terrain) -> Result<ScatterSet, LookdevError> {
    let name = match planet {
        Planet::Mars => "mars",
        Planet::Europa => "europa",
    };
    let path = content.join("scatter").join(format!("{name}.scatter.ron"));
    let text = std::fs::read_to_string(&path).map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(ScatterSet::from_ron(&text, Some(&terrain.layers))?)
}

/// Loads every scan mesh `set` names (paths relative to the content drop) with its authored
/// LOD chain (`_LOD0`, `_LOD1`, `_LOD2` nodes) and material.
///
/// # Errors
///
/// A glTF or one of its maps failed to load, or a mesh has no usable LOD.
pub fn scan_library(content: &Path, set: &ScatterSet) -> Result<ScatterLibrary, LookdevError> {
    let mut library = ScatterLibrary::new();
    // Largest instance per mesh over every class that places it.
    let mut largest: Vec<(&str, f32)> = Vec::new();
    for class in &set.classes {
        for m in &class.meshes {
            match largest.iter_mut().find(|(p, _)| *p == m.mesh) {
                Some((_, s)) => *s = s.max(m.max_scale),
                None => largest.push((&m.mesh, m.max_scale)),
            }
        }
    }
    for (mesh, max_scale) in largest {
        let path = content.join(mesh);
        let (mut meshes, mut materials) = (Vec::new(), Vec::new());
        crate::do27::load_gltf_capped(
            &path,
            false,
            0,
            &mut meshes,
            &mut materials,
            scan_texture_max(max_scale),
        )?;
        let lods: Vec<ScatterMeshLod> = meshes
            .into_iter()
            .take(SCAN_SCREEN_SIZES.len())
            .zip(SCAN_SCREEN_SIZES)
            .map(|(m, screen_size)| ScatterMeshLod {
                vertices: m.vertices,
                indices: m.indices,
                screen_size,
            })
            .collect();
        let mut lods = lods;
        if let Some(last) = lods.last_mut() {
            last.screen_size = 0.0;
        }
        let asset = ScatterMeshAsset {
            lods,
            material: materials.into_iter().next().unwrap_or_default(),
            parts: Vec::new(),
            shadow_lod: 0,
        };
        asset
            .validate(mesh)
            .map_err(|e| LookdevError::Config(e.to_string()))?;
        library.insert(mesh.to_owned(), asset);
    }
    Ok(library)
}

/// Content-side levers a density variant may turn on ([`install`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DensityLevers {
    /// Give every mesh with fewer than four LODs one more, vertex-clustered
    /// ([`regia_terrain::with_coarse_lod`], 3³ cells) below [`COARSE_LOD_SCREEN_SIZE`].
    pub coarse_lod: bool,
    /// Place the small-stone classes (largest instance ≤ [`SMALL_CLASS_MAX_M`]) as clumps of
    /// this many pebbles ([`regia_terrain::clump_asset`]) at the class density divided by it
    /// (0 = off): the same pebbles, a fraction of the instances.
    pub clump: u32,
    /// Build the field with one material per mesh path
    /// ([`regia_terrain::ScatterBuildOptions::shared_materials`]): textures resident once.
    pub shared_materials: bool,
}

/// Projected size (radius over the half-height) below which the coarse LOD draws: ~4 px
/// radius at 1440p with the chase's field of view.
pub const COARSE_LOD_SCREEN_SIZE: f32 = 0.006;

/// Applies `levers` to a set and its library: coarse LODs on every mesh, clumps for the small
/// classes (their mesh paths become `<mesh>#clump<n>`).
#[must_use]
pub fn apply_levers(
    set: &ScatterSet,
    library: &ScatterLibrary,
    levers: DensityLevers,
) -> (ScatterSet, ScatterLibrary) {
    let mut library = library.clone();
    if levers.coarse_lod {
        for asset in library.values_mut() {
            *asset = regia_terrain::with_coarse_lod(asset, 3, COARSE_LOD_SCREEN_SIZE);
        }
    }
    let mut set = set.clone();
    if levers.clump > 1 {
        let n = levers.clump;
        for class in &mut set.classes {
            if !class.meshes.iter().all(|m| m.max_scale <= SMALL_CLASS_MAX_M) {
                continue;
            }
            class.density /= n as f32;
            // Members are buried one by one; the base blend spans the clump's height, so it
            // covers about half as much of a typical member.
            class.base_blend *= 0.5;
            for m in &mut class.meshes {
                let Some(member) = library.get(&m.mesh) else {
                    continue;
                };
                let seed = m.mesh.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                    (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
                });
                // Members carry the class's size distribution; the clump as a whole varies
                // by ±10 % around the class's largest size.
                let settings = regia_terrain::ClumpSettings {
                    members: n,
                    radius: 2.0,
                    min_scale: m.min_scale / m.max_scale.max(1e-6),
                    max_scale: 1.0,
                    scale_bias: m.scale_bias,
                    sink: class.sink,
                    seed,
                };
                let clump = regia_terrain::clump_asset(member, &settings, 1);
                let path = format!("{}#clump{n}", m.mesh);
                library.insert(path.clone(), clump);
                m.mesh = path;
                m.min_scale = m.max_scale * 0.9;
                m.max_scale *= 1.1;
                m.scale_bias = 1.0;
            }
        }
    }
    (set, library)
}

/// Classes whose largest instance is at most this many metres across are small-stone classes:
/// they take the full density factor (and the clump lever).
pub const SMALL_CLASS_MAX_M: f32 = 1.0;

/// Share of the increase a larger-rock class keeps for itself ("some larger rocks"); the rest
/// of its share goes to its small-stone fill class.
pub const LARGE_SHARE: f32 = 0.25;

/// Fade range of a fill class (metres): small stones are gone from the photographs' far field.
pub const FILL_FADE: (f32, f32) = (100.0, 140.0);

/// `set` at `factor` × its instance count, as the NASA ground looks (many small stones, some
/// larger rocks):
///
/// - small-stone classes (largest instance ≤ [`SMALL_CLASS_MAX_M`]) take the whole factor;
/// - a larger-rock class grows by `1 + (factor − 1) · LARGE_SHARE`, and the rest of its share
///   of the increase becomes a `<class>_fill` class of the same meshes at its smallest sizes
///   (`min_scale` to twice that, at most [`SMALL_CLASS_MAX_M`]), fading at [`FILL_FADE`];
/// - every class's size distribution steepens by `log2(factor) / 4` of its `scale_bias`.
///
/// The placed candidates come to `factor` × the set's. `factor` ≤ 1 returns the set unchanged.
#[must_use]
pub fn densify(set: &ScatterSet, factor: f32) -> ScatterSet {
    let mut out = set.clone();
    if factor <= 1.0 || !factor.is_finite() {
        return out;
    }
    let skew = 1.0 + factor.log2() * 0.25;
    let mut fills = Vec::new();
    for class in &mut out.classes {
        let small = class.meshes.iter().all(|m| m.max_scale <= SMALL_CLASS_MAX_M);
        if small {
            class.density *= factor;
        } else {
            let grow = 1.0 + (factor - 1.0) * LARGE_SHARE;
            let mut fill = class.clone();
            fill.name = format!("{}_fill", class.name);
            fill.density = class.density * (factor - grow);
            fill.fade_start = class.fade_start.min(FILL_FADE.0);
            fill.fade_end = class.fade_end.min(FILL_FADE.1).max(fill.fade_start + 1.0);
            for m in &mut fill.meshes {
                m.max_scale = (m.min_scale * 2.0).min(SMALL_CLASS_MAX_M).max(m.min_scale);
            }
            fills.push(fill);
            class.density *= grow;
        }
    }
    out.classes.extend(fills);
    for class in &mut out.classes {
        for m in &mut class.meshes {
            m.scale_bias *= skew;
        }
    }
    out
}

/// Expected candidates per square metre of `set` before the filters (density / cell²,
/// summed over the classes).
#[must_use]
pub fn candidates_per_m2(set: &ScatterSet) -> f32 {
    set.classes
        .iter()
        .map(|c| c.density / (c.cell_size * c.cell_size).max(1e-6))
        .sum()
}

/// Replaces `terrain`'s scatter with `source` at `factor` × density, and drops its baked
/// impostor cards (they belong to the old meshes). Returns the set placed.
///
/// # Errors
///
/// The set or its meshes failed to load, or the field rejected them.
pub fn install(
    content: &Path,
    planet: Planet,
    terrain: &mut R2Terrain,
    source: BenchScatter,
    factor: f32,
    levers: DensityLevers,
) -> Result<ScatterSet, LookdevError> {
    let map_dir = content.join("maps").join(crate::do27::map_name(planet));
    let (base, library) = match source {
        BenchScatter::Map => {
            let set = crate::do27::load_scatter_set(&map_dir, &terrain.layers)?;
            let library = crate::do27::rock_library(planet, &set);
            (set, library)
        }
        BenchScatter::Scan => {
            let set = scan_set(content, planet, terrain)?;
            let library = scan_library(content, &set)?;
            (set, library)
        }
    };
    let (set, library) = apply_levers(&densify(&base, factor), &library, levers);
    let holes = terrain.do27.as_ref().map(|d| d.map.holes.clone());
    let field_source = regia_terrain::ScatterSource {
        map: Arc::new(terrain.map.clone()),
        holes: holes.filter(|h| !h.is_empty()),
        layers: Some(terrain.layers.clone()),
        density_maps: DensityMaps::new(),
    };
    let field = ScatterField::new_with(
        &set,
        field_source,
        &library,
        set.seed,
        regia_terrain::ScatterBuildOptions {
            shared_materials: levers.shared_materials,
        },
    )
    .map_err(regia_terrain::TerrainError::from)?;
    terrain.scatter = Some(field);
    terrain.impostor_atlas = None;
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::{ScatterClass, ScatterMeshRef};

    fn class(name: &str, density: f32, max_scale: f32) -> ScatterClass {
        ScatterClass {
            name: name.into(),
            meshes: vec![ScatterMeshRef {
                mesh: format!("{name}.gltf"),
                weight: 1.0,
                min_scale: max_scale * 0.1,
                max_scale,
                scale_bias: 2.0,
            }],
            cell_size: 1.0,
            density,
            ..ScatterClass::default()
        }
    }

    #[test]
    fn densify_puts_the_increase_into_small_stones() {
        let set = ScatterSet {
            classes: vec![class("scree", 0.9, 0.5), class("boulders", 0.1, 3.4)],
            ..ScatterSet::default()
        };
        assert_eq!(densify(&set, 1.0), set);
        let dense = densify(&set, 10.0);
        assert!((dense.classes[0].density - 9.0).abs() < 1e-5);
        // The boulders grow by a quarter of the increase; the rest is small boulder fill.
        assert!((dense.classes[1].density - 0.325).abs() < 1e-5);
        let fill = &dense.classes[2];
        assert_eq!(fill.name, "boulders_fill");
        assert!((fill.density - 0.675).abs() < 1e-5);
        assert!(fill.meshes[0].max_scale <= SMALL_CLASS_MAX_M && fill.fade_end <= FILL_FADE.1);
        // Steeper size distribution: more of the instances small.
        assert!(dense.classes[0].meshes[0].scale_bias > set.classes[0].meshes[0].scale_bias);
        let ratio = candidates_per_m2(&dense) / candidates_per_m2(&set);
        assert!((ratio - 10.0).abs() < 1e-3, "{ratio}");
    }

    #[test]
    fn scan_textures_follow_instance_size() {
        assert_eq!(scan_texture_max(0.5), 256);
        assert_eq!(scan_texture_max(1.8), 512);
        assert_eq!(scan_texture_max(3.4), 1024);
        assert_eq!(scan_texture_max(16.0), 1024);
        assert_eq!(BenchScatter::parse("scan"), Some(BenchScatter::Scan));
        assert_eq!(BenchScatter::parse("x"), None);
    }
}
