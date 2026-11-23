// SPDX-License-Identifier: MPL-2.0
//! Cook a fracture set (`.fracture.ron`, ADR-1567) into the runtime chunk asset
//! (`.destructible.ron` v1, `regia_physics::destruction`, ADR-1597).
//!
//! Each fracture chunk becomes a chunk named `frac_<kit>_<n>` (the kit id, or the asset name) with
//! mass `volume * density`, its welded vertices about the centroid as the collision hull (reduced to at most `MAX_HULL_POINTS` by farthest-point
//! selection when larger), its bounds, and a support flag from [`SupportRule`]. Each fracture joint
//! becomes a bond whose break impulse is the joint's break force held for `bond_seconds`
//! (`impulse = force * time`). The same fracture set and settings give the same bytes.

use regia_physics::destruction::{
    DESTRUCTIBLE_FORMAT_VERSION, DestructibleAsset, DestructibleBond, DestructibleChunk, DestructibleError,
    DestructionMaterial, MAX_HULL_POINTS, SourceKit,
};

use crate::fracture::{FractureChunk, FractureSet};

/// Which chunks the world holds up.
#[derive(Clone, Debug, PartialEq)]
pub enum SupportRule {
    /// None: a free object.
    None,
    /// Chunks whose lowest point is at or below this height (asset space): resting on the ground.
    /// The sentinel `f32::NEG_INFINITY` is special: it means the lowest 1 cm of the whole set, not
    /// "nothing qualifies" (use [`SupportRule::None`] for that).
    BelowY(f32),
    /// These chunk indices.
    Explicit(Vec<u32>),
}

/// Settings of [`cook_destructible`].
#[derive(Clone, Debug, PartialEq)]
pub struct CookSettings {
    /// Asset name.
    pub name: String,
    /// Path of the `.fracture.ron` holding the render meshes (written into `meshes`).
    pub meshes: Option<String>,
    /// Kilograms per cubic metre.
    pub density: f32,
    /// Seconds a joint's break force must act to break the bond (break impulse = force x time).
    pub bond_seconds: f32,
    /// Support chunks.
    pub support: SupportRule,
    /// Material.
    pub material: DestructionMaterial,
    /// The kit this set fractures (names the chunks `frac_<kit>_<n>`).
    pub source: Option<SourceKit>,
    /// The fracture seed the set was made with.
    pub fracture_seed: Option<u64>,
    /// The pre-scored pattern the set was made with.
    pub pattern_id: Option<String>,
    /// Outer (original surface) material of the chunks.
    pub outer_material: Option<String>,
    /// Material of the cut faces.
    pub inner_material: Option<String>,
}

impl CookSettings {
    /// Defaults for `name`: concrete density (2400 kg/m^3), 0.01 s bonds, chunks touching the
    /// lowest 1 cm of the set supported.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            meshes: None,
            density: 2400.0,
            bond_seconds: 0.01,
            support: SupportRule::BelowY(f32::NEG_INFINITY),
            material: DestructionMaterial::default(),
            source: None,
            fracture_seed: None,
            pattern_id: None,
            outer_material: None,
            inner_material: None,
        }
    }
}

/// Builds the runtime chunk asset of a fracture set.
///
/// # Errors
/// [`DestructibleError::Invalid`] for a non-positive density or bond time, an explicit support
/// index out of range, or a set whose result fails validation (a chunk with no volume, fewer than
/// four distinct vertices, or a non-finite value).
pub fn cook_destructible(
    set: &FractureSet,
    settings: &CookSettings,
) -> Result<DestructibleAsset, DestructibleError> {
    let _span = tracing::info_span!("cook_destructible", chunks = set.chunks.len()).entered();
    let fail = |reason: String| DestructibleError::Invalid {
        name: settings.name.clone(),
        reason,
    };
    if !(settings.density.is_finite() && settings.density > 0.0) {
        return Err(fail("density must be > 0".into()));
    }
    if !(settings.bond_seconds.is_finite() && settings.bond_seconds > 0.0) {
        return Err(fail("bond_seconds must be > 0".into()));
    }
    // `BelowY(-inf)` means "the lowest 1 cm of the whole set".
    let floor = set
        .chunks
        .iter()
        .map(|c| c.bounds_min[1])
        .fold(f32::INFINITY, f32::min);
    let support_y = match settings.support {
        SupportRule::BelowY(y) if y == f32::NEG_INFINITY => Some(floor + 0.01),
        SupportRule::BelowY(y) => Some(y),
        _ => None,
    };
    if let SupportRule::Explicit(list) = &settings.support
        && let Some(bad) = list.iter().find(|&&i| i as usize >= set.chunks.len())
    {
        return Err(fail(format!(
            "support index {bad} is outside the {} chunks",
            set.chunks.len()
        )));
    }
    let prefix = settings
        .source
        .as_ref()
        .map_or(settings.name.as_str(), |s| s.kit.as_str());
    let chunks = set
        .chunks
        .iter()
        .enumerate()
        .map(|(i, c)| DestructibleChunk {
            name: format!("frac_{prefix}_{i}"),
            parent: None,
            centroid: c.centroid,
            mass: c.volume * settings.density,
            inertia: None,
            hulls: vec![hull_points(c)],
            lods: Vec::new(),
            fx_tag: None,
            small: false,
            lifetime_secs: None,
            edge_heat: None,
            bounds_min: c.bounds_min,
            bounds_max: c.bounds_max,
            support: match &settings.support {
                SupportRule::None => false,
                SupportRule::BelowY(_) => support_y.is_some_and(|y| c.bounds_min[1] <= y),
                SupportRule::Explicit(list) => list.contains(&(i as u32)),
            },
        })
        .collect();
    let bonds = set
        .joints
        .iter()
        .map(|j| DestructibleBond {
            a: j.a,
            b: j.b,
            area: j.contact_area,
            anchor: j.anchor,
            normal: j.normal,
            // A zero-strength joint still needs a positive threshold: the smallest normal f32.
            break_impulse: (j.break_force * settings.bond_seconds).max(f32::MIN_POSITIVE),
            max_load: None,
        })
        .collect();
    let asset = DestructibleAsset {
        format_version: DESTRUCTIBLE_FORMAT_VERSION,
        name: settings.name.clone(),
        source: settings.source.clone(),
        pattern_id: settings.pattern_id.clone(),
        fracture_seed: settings.fracture_seed,
        replaces_stage: None,
        meshes: settings.meshes.clone(),
        outer_material: settings.outer_material.clone(),
        inner_material: settings.inner_material.clone(),
        material: settings.material.clone(),
        chunks,
        bonds,
        load: None,
        falling: None,
    };
    asset.validate()?;
    Ok(asset)
}

/// The chunk's vertices about its centroid; more than `MAX_HULL_POINTS` are thinned by
/// farthest-point selection (Gonzalez, "Clustering to minimize the maximum intercluster distance",
/// Theoretical Computer Science 38, 1985): start from the vertex farthest from the centroid, then
/// repeatedly add the vertex farthest from every vertex chosen so far (ties: the lowest index).
fn hull_points(c: &FractureChunk) -> Vec<[f32; 3]> {
    let rel: Vec<[f32; 3]> = c
        .positions
        .iter()
        .map(|p| [p[0] - c.centroid[0], p[1] - c.centroid[1], p[2] - c.centroid[2]])
        .collect();
    if rel.len() <= MAX_HULL_POINTS {
        return rel;
    }
    let d2 = |a: [f32; 3], b: [f32; 3]| {
        let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
    };
    let farthest = |dist: &[f32]| {
        let mut best = 0usize;
        for (i, &d) in dist.iter().enumerate() {
            if d > dist[best] {
                best = i;
            }
        }
        best
    };
    let mut nearest: Vec<f32> = rel.iter().map(|p| d2(*p, [0.0; 3])).collect();
    let mut chosen = Vec::with_capacity(MAX_HULL_POINTS);
    while chosen.len() < MAX_HULL_POINTS {
        let next = farthest(&nearest);
        chosen.push(rel[next]);
        for (n, p) in nearest.iter_mut().zip(&rel) {
            *n = n.min(d2(*p, rel[next]));
        }
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hull_is_thinned_to_the_cap_keeping_the_extremes() {
        // A dense line of 1000 points from -1 to 1 plus two far points.
        let mut positions: Vec<[f32; 3]> = (0..1000).map(|i| [i as f32 / 500.0 - 1.0, 0.0, 0.0]).collect();
        positions.push([0.0, 5.0, 0.0]);
        positions.push([0.0, 0.0, -5.0]);
        let chunk = FractureChunk {
            site: [0.0; 3],
            positions,
            indices: Vec::new(),
            interior_start: 0,
            volume: 1.0,
            centroid: [0.0; 3],
            bounds_min: [-1.0, 0.0, -5.0],
            bounds_max: [1.0, 5.0, 0.0],
            interior_area: 0.0,
            render: Default::default(),
        };
        let hull = hull_points(&chunk);
        assert_eq!(hull.len(), MAX_HULL_POINTS);
        for extreme in [[0.0, 5.0, 0.0], [0.0, 0.0, -5.0], [-1.0, 0.0, 0.0]] {
            assert!(hull.contains(&extreme), "{extreme:?} kept");
        }
        assert_eq!(hull, hull_points(&chunk), "deterministic");
    }
}
