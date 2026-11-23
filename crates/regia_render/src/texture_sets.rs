// SPDX-License-Identifier: MPL-2.0
//! Texture-set keys and the census for bindless step 2 (ADR-356, step 2a).
//!
//! Bindless step 2 (ADR-317 step 2, refined by ADR-356) gives every draw a material slot and
//! binds material textures in groups shared by every standard material that samples the same
//! seven views. What that saves depends on the content: a scene whose materials each bring their
//! own textures shares nothing, and a kit with many tints of one texture set shares a lot. This
//! module measures it on the running renderer before step 2 is built:
//!
//! - each standard material entry keeps the key of the views its group binds
//!   ([`texture_set_key`]);
//! - [`resident_census`] counts the distinct sets among the resident materials;
//! - [`pass_census`] walks one pass's draw list in order and counts the group-2 changes it makes
//!   today (one per change of material) against the changes it would make if a draw changed group
//!   only when its texture set changed, and the floor if the draws were also sorted by set.
//!
//! Nothing here binds, draws or reorders, so a frame is byte-identical with the census on or off.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::key_hash::{KeyBuild, KeyHasher, KeyMap};

/// The key of a texture set: the identities of the views a material group binds, in bind order.
///
/// Two live materials get the same key when they bind the same view objects. wgpu compares
/// views by identity, not by content, so two uploads of the same image are two sets (that is
/// also what step 2 would see). Keys are only compared between live cache entries: an entry's
/// bind group keeps its views alive, so a view id is not reused while a key that names it is
/// still held.
pub(crate) fn texture_set_key<T: Hash>(views: &[T]) -> u64 {
    let mut hasher = KeyHasher::default();
    views.hash(&mut hasher);
    hasher.finish()
}

/// What one material binds in group 2, as the census sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SetIdentity {
    /// A standard material: the [`texture_set_key`] of its seven views. Step 2 shares one group
    /// between every material with the same key.
    Shared(u64),
    /// A material that keeps a group of its own through step 2 (a material graph, until step 5
    /// moves graph parameters into the arena): its material key.
    Own(u64),
}

/// Texture sets among the resident materials ([`resident_census`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResidentCensus {
    /// Standard materials ([`SetIdentity::Shared`]).
    pub(crate) standard_materials: usize,
    /// Materials with a group of their own ([`SetIdentity::Own`]).
    pub(crate) graph_materials: usize,
    /// Distinct texture sets among the standard materials.
    pub(crate) texture_sets: usize,
    /// Standard materials whose set at least one other standard material binds too.
    pub(crate) shared_materials: usize,
    /// The most standard materials on one set.
    pub(crate) largest_set: usize,
}

/// Counts the texture sets of `materials` (one identity per resident material).
pub(crate) fn resident_census(materials: impl IntoIterator<Item = SetIdentity>) -> ResidentCensus {
    let mut sets: KeyMap<u64, usize> = KeyMap::default();
    let mut census = ResidentCensus::default();
    for identity in materials {
        match identity {
            SetIdentity::Shared(key) => {
                census.standard_materials += 1;
                *sets.entry(key).or_insert(0) += 1;
            }
            SetIdentity::Own(_) => census.graph_materials += 1,
        }
    }
    census.texture_sets = sets.len();
    census.shared_materials = sets.values().filter(|&&count| count > 1).sum();
    census.largest_set = sets.values().copied().max().unwrap_or(0);
    census
}

/// One pass's draw list, as group 2 sees it ([`pass_census`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PassCensus {
    /// Draws in the list.
    pub(crate) draws: usize,
    /// Runs of consecutive draws with the same material: the group-2 binds today.
    pub(crate) material_runs: usize,
    /// Runs of consecutive draws with the same texture set: the group-2 binds with groups shared
    /// by set and the draw order unchanged.
    pub(crate) texture_set_runs: usize,
    /// Distinct materials in the list.
    pub(crate) materials: usize,
    /// Distinct sets in the list: the group-2 binds if the draws were also sorted by set.
    pub(crate) texture_sets: usize,
}

/// Walks a draw list in order: `(material key, what that material binds)` per draw.
pub(crate) fn pass_census(draws: impl IntoIterator<Item = (u64, SetIdentity)>) -> PassCensus {
    let mut census = PassCensus::default();
    let mut materials: HashSet<u64, KeyBuild> = HashSet::default();
    let mut sets: HashSet<SetIdentity, KeyBuild> = HashSet::default();
    let mut last: Option<(u64, SetIdentity)> = None;
    for (material, set) in draws {
        census.draws += 1;
        if last.is_none_or(|(previous, _)| previous != material) {
            census.material_runs += 1;
        }
        if last.is_none_or(|(_, previous)| previous != set) {
            census.texture_set_runs += 1;
        }
        last = Some((material, set));
        materials.insert(material);
        sets.insert(set);
    }
    census.materials = materials.len();
    census.texture_sets = sets.len();
    census
}

/// The bindless step-2 census (ADR-356): what sharing material groups by texture set would
/// save on the running scene ([`crate::FrameRenderer::texture_set_stats`]).
///
/// The resident counts are read on demand and cost nothing per frame. The pass counts come
/// from the last main-view frame drawn while the census was on
/// ([`crate::FrameRenderer::set_texture_set_census`], off by default) and are zero otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TextureSetStats {
    /// Whether the per-frame pass census runs.
    pub census: bool,
    /// Standard materials resident in the material cache.
    pub standard_materials: usize,
    /// Material-graph materials resident (each keeps a group of its own until step 5).
    pub graph_materials: usize,
    /// Distinct texture sets among the standard materials: the material groups step 2 would
    /// keep for them.
    pub texture_sets: usize,
    /// Standard materials whose texture set at least one other resident material binds too.
    pub shared_materials: usize,
    /// The most standard materials on one texture set.
    pub largest_set: usize,
    /// Main-view frames the pass census has observed since it was turned on.
    pub frames: u64,
    /// Draws in the last observed frame's forward opaque list: the visible opaque and alpha-test
    /// draws in the order they were collected. Visibility-buffer draws are not in it (the
    /// resolve shades them; step 4).
    pub opaque_draws: usize,
    /// Group-2 changes that list makes today: one per change of material between consecutive
    /// draws.
    pub material_runs: usize,
    /// Group-2 changes the same list would make with groups shared by texture set (step 2, draw
    /// order unchanged).
    pub texture_set_runs: usize,
    /// Distinct materials in that list.
    pub pass_materials: usize,
    /// Distinct texture sets in that list: the group-2 changes if step 2 also sorted the draws
    /// by set.
    pub pass_texture_sets: usize,
}

/// The renderer's census state: the switch and the last observed pass.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TextureSetCensus {
    enabled: bool,
    frames: u64,
    last: PassCensus,
}

impl TextureSetCensus {
    /// Whether the per-frame pass census runs.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turns the pass census on or off. A change starts the frame count and the last pass over.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if enabled != self.enabled {
            self.frames = 0;
            self.last = PassCensus::default();
        }
        self.enabled = enabled;
    }

    /// Records one main-view frame's opaque list (only while enabled).
    pub(crate) fn record(&mut self, pass: PassCensus) {
        if self.enabled {
            self.frames += 1;
            self.last = pass;
        }
    }

    /// The public reading: `resident` (counted now) with the last observed pass.
    pub(crate) fn stats(&self, resident: ResidentCensus) -> TextureSetStats {
        TextureSetStats {
            census: self.enabled,
            standard_materials: resident.standard_materials,
            graph_materials: resident.graph_materials,
            texture_sets: resident.texture_sets,
            shared_materials: resident.shared_materials,
            largest_set: resident.largest_set,
            frames: self.frames,
            opaque_draws: self.last.draws,
            material_runs: self.last.material_runs,
            texture_set_runs: self.last.texture_set_runs,
            pass_materials: self.last.materials,
            pass_texture_sets: self.last.texture_sets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_follows_identity_and_bind_order() {
        let a = texture_set_key(&[1u64, 2, 3, 4, 5, 6, 7]);
        assert_eq!(
            a,
            texture_set_key(&[1u64, 2, 3, 4, 5, 6, 7]),
            "same views, same key"
        );
        assert_ne!(
            a,
            texture_set_key(&[2u64, 1, 3, 4, 5, 6, 7]),
            "bind order matters"
        );
        assert_ne!(a, texture_set_key(&[1u64, 2, 3, 4, 5, 6, 8]), "one view differs");
        assert_ne!(
            a,
            texture_set_key(&[1u64, 2, 3, 4, 5, 6]),
            "the slot count is part of it"
        );
    }

    #[test]
    fn resident_sets_count_sharing() {
        use SetIdentity::{Own, Shared};
        let census = resident_census([Shared(10), Shared(10), Shared(10), Shared(20), Shared(30), Own(7)]);
        assert_eq!(
            census,
            ResidentCensus {
                standard_materials: 5,
                graph_materials: 1,
                texture_sets: 3,
                shared_materials: 3,
                largest_set: 3,
            }
        );
        assert_eq!(
            resident_census(Vec::<SetIdentity>::new()),
            ResidentCensus::default()
        );
        // Graph materials never share, and an own key never meets a shared key of the same value.
        let graphs = resident_census([Own(10), Own(10), Shared(10)]);
        assert_eq!(graphs.texture_sets, 1);
        assert_eq!(graphs.shared_materials, 0);
        assert_eq!(graphs.graph_materials, 2);
    }

    #[test]
    fn pass_runs_count_material_and_set_changes() {
        use SetIdentity::{Own, Shared};
        // Materials 1 and 2 tint one set; 3 has its own textures; 9 is a graph material.
        let list = [
            (1, Shared(100)),
            (1, Shared(100)),
            (2, Shared(100)),
            (3, Shared(300)),
            (1, Shared(100)),
            (9, Own(9)),
            (2, Shared(100)),
        ];
        let census = pass_census(list);
        assert_eq!(
            census,
            PassCensus {
                draws: 7,
                // 1 | 2 | 3 | 1 | 9 | 2
                material_runs: 6,
                // 100 | 300 | 100 | own 9 | 100
                texture_set_runs: 5,
                materials: 4,
                texture_sets: 3,
            }
        );
        assert_eq!(
            pass_census(Vec::<(u64, SetIdentity)>::new()),
            PassCensus::default()
        );
        // One material: one run either way.
        let single = pass_census([(5, Shared(1)), (5, Shared(1))]);
        assert_eq!((single.material_runs, single.texture_set_runs), (1, 1));
    }

    #[test]
    fn set_runs_never_exceed_material_runs() {
        use SetIdentity::Shared;
        // A deterministic mix: material m binds set m % 3.
        let list: Vec<(u64, SetIdentity)> = (0..200u64)
            .map(|i| (i * 7 % 11, Shared(i * 7 % 11 % 3)))
            .collect();
        let census = pass_census(list);
        assert!(census.texture_set_runs <= census.material_runs, "{census:?}");
        assert!(census.texture_sets <= census.materials, "{census:?}");
        assert!(census.texture_sets <= census.texture_set_runs, "{census:?}");
        assert_eq!(census.materials, 11);
        assert_eq!(census.texture_sets, 3);
    }

    #[test]
    fn the_census_switch_resets_and_gates_recording() {
        let pass = PassCensus {
            draws: 4,
            material_runs: 4,
            texture_set_runs: 3,
            materials: 3,
            texture_sets: 2,
        };
        let mut census = TextureSetCensus::default();
        census.record(pass);
        let off = census.stats(ResidentCensus::default());
        assert!(!off.census);
        assert_eq!((off.frames, off.opaque_draws), (0, 0), "off records nothing");
        census.set_enabled(true);
        census.record(pass);
        census.record(pass);
        let on = census.stats(ResidentCensus {
            standard_materials: 3,
            texture_sets: 2,
            shared_materials: 2,
            largest_set: 2,
            graph_materials: 0,
        });
        assert!(on.census);
        assert_eq!(on.frames, 2);
        assert_eq!(
            (on.opaque_draws, on.material_runs, on.texture_set_runs),
            (4, 4, 3)
        );
        assert_eq!((on.pass_materials, on.pass_texture_sets), (3, 2));
        assert_eq!(
            (on.standard_materials, on.texture_sets, on.shared_materials),
            (3, 2, 2)
        );
        census.set_enabled(true);
        assert_eq!(
            census.stats(ResidentCensus::default()).frames,
            2,
            "no change, no reset"
        );
        census.set_enabled(false);
        let reset = census.stats(ResidentCensus::default());
        assert_eq!(
            (reset.frames, reset.opaque_draws),
            (0, 0),
            "off clears the last pass"
        );
    }
}

/// Frame-level checks on the CPU path: exact census counts on a fixed scene, and the census on
/// against off on the same device (identical pixels). No golden image, no timing.
#[cfg(test)]
mod frame_tests {
    use crate::{
        DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabTexture,
        LabVertex, TextureSetStats,
    };

    fn quad(x: f32, material: usize) -> LabMesh {
        LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([x - 0.3, -0.3, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
                LabVertex::from_pos_normal_uv([x + 0.3, -0.3, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
                LabVertex::from_pos_normal_uv([x + 0.3, 0.3, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([x - 0.3, 0.3, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
            material,
            ..LabMesh::unskinned()
        }
    }

    fn tinted(r: f32) -> LabMaterial {
        LabMaterial {
            base_color: [r, 0.5, 0.25, 1.0],
            ..LabMaterial::default()
        }
    }

    /// Materials 0 and 1 are untextured tints (both groups bind the renderer's fallback views:
    /// one texture set); material 2 brings its own albedo (a second set). Four quads in scene
    /// order: 0, 1, 2, 0.
    fn scene() -> LabScene {
        let checker = LabTexture {
            width: 2,
            height: 2,
            rgba: vec![
                255, 255, 255, 255, 40, 40, 40, 255, //
                40, 40, 40, 255, 255, 255, 255, 255,
            ],
        };
        let textured = LabMaterial {
            albedo: Some(checker),
            ..tinted(0.9)
        };
        LabScene {
            width: 64,
            height: 32,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 20.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 5_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials: vec![tinted(0.8), tinted(0.2), textured],
            meshes: vec![quad(-1.2, 0), quad(-0.4, 1), quad(0.4, 2), quad(1.2, 0)],
        }
    }

    #[test]
    fn census_counts_are_exact_on_a_fixed_scene() {
        let device = crate::device::test_device();
        let lab = scene();
        let frame = lab.as_frame_scene_with_quality(FrameQuality::LAB);
        let mut renderer = FrameRenderer::new();
        assert!(!renderer.texture_set_census(), "off by default");
        assert_eq!(renderer.texture_set_stats(), TextureSetStats::default());
        renderer.render_offscreen(&device, &frame).expect("frame");
        let off = renderer.texture_set_stats();
        // The resident counts need no switch: three standard materials on two sets.
        assert_eq!(
            (off.standard_materials, off.graph_materials, off.texture_sets),
            (3, 0, 2),
            "{off:?}"
        );
        assert_eq!((off.shared_materials, off.largest_set), (2, 2), "{off:?}");
        assert_eq!((off.frames, off.opaque_draws), (0, 0), "no pass census while off");
        renderer.set_texture_set_census(true);
        for _ in 0..2 {
            renderer.render_offscreen(&device, &frame).expect("frame");
        }
        let on = renderer.texture_set_stats();
        assert!(on.census);
        assert_eq!(on.frames, 2);
        // Materials 0 | 1 | 2 | 0 bind four groups today; by texture set, fallback | own | fallback.
        assert_eq!(
            (on.opaque_draws, on.material_runs, on.texture_set_runs),
            (4, 4, 3),
            "{on:?}"
        );
        assert_eq!((on.pass_materials, on.pass_texture_sets), (3, 2), "{on:?}");
        renderer.set_texture_set_census(false);
        let reset = renderer.texture_set_stats();
        assert_eq!((reset.frames, reset.opaque_draws), (0, 0), "{reset:?}");
        assert_eq!(reset.texture_sets, 2, "resident counts stay readable");
    }

    /// The census only reads: the same frames with it on and off are byte-identical.
    #[test]
    fn census_on_and_off_draw_identical_pixels() {
        let device = crate::device::test_device();
        let mut lab = scene();
        let mut plain = FrameRenderer::new();
        let mut counted = FrameRenderer::new();
        counted.set_texture_set_census(true);
        for frame in 0..3 {
            lab.materials[1].roughness = 0.3 + 0.2 * frame as f32;
            let scene = lab.as_frame_scene_with_quality(FrameQuality::LAB);
            let a = plain.render_offscreen(&device, &scene).expect("frame").pixels;
            let b = counted.render_offscreen(&device, &scene).expect("frame").pixels;
            assert_eq!(a.len(), b.len());
            let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
            assert_eq!(differing, 0, "frame {frame}: {differing} bytes differ");
        }
        assert_eq!(counted.texture_set_stats().frames, 3);
        assert_eq!(plain.texture_set_stats().frames, 0);
    }
}
