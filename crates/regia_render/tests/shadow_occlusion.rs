// SPDX-License-Identifier: MPL-2.0
//! Two-phase occlusion of the shadow casters (ADR-2072, `docs/specs/98-visibility-buffer.md`
//! § 10a), on the headless device (lavapipe in CI): the culling differential test.
//!
//! A slab hangs over a field of 256 small boxes, lit by the sun and by a spot light from above.
//! Two renderers draw the same frames, one with the shadow cull and one without. Every frame,
//! every sun cascade and the local-light atlas must hold the same depth texel for texel, and the
//! frames must match byte for byte: culling never changes a rendered pixel. The GPU counters show
//! the cull working: the first frame has no pyramid history (phase 1 draws every caster), the
//! second skips the boxes under the slab, and when the slab goes away the boxes phase 1 still
//! skips (last redraw's pyramid hid them) are drawn by phase 2. The mixed scene adds casters the
//! lists batch apart, shadow scale and lift, and a camera, sun and light set that move every frame.

use regia_core::math::{Mat4, Vec3};
use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    IndirectStrategy, LabLocalLight, LabMaterial, LabPointLight, LabSpotLight, LabVertex, RenderDevice,
    RenderProfile, RenderProfilePreset, ShadowCast, ShadowOcclusionCounters, ShadowOcclusionSkip, box_mesh,
};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 96;
/// Boxes per side of the field under the slab.
const FIELD: usize = 16;
/// Sun cascades drawn (kinds 0..CASCADES), and the atlas (kind 4).
const CASCADES: usize = 2;

struct Geometry {
    ground: (Vec<LabVertex>, Vec<u32>),
    slab: (Vec<LabVertex>, Vec<u32>),
    cube: (Vec<LabVertex>, Vec<u32>),
    material: LabMaterial,
    lights: Vec<LabLocalLight>,
}

impl Geometry {
    fn new() -> Self {
        Self {
            ground: box_mesh([8.0, 0.05, 8.0]),
            slab: box_mesh([3.2, 0.05, 3.2]),
            cube: box_mesh([0.1, 0.1, 0.1]),
            material: LabMaterial {
                base_color: [0.6, 0.55, 0.5, 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
            lights: vec![LabLocalLight::Spot(LabSpotLight {
                position: [0.3, 6.0, 0.2],
                direction: [0.0, -1.0, 0.0],
                color: [1.0, 0.9, 0.8],
                intensity: 400.0,
                range: 14.0,
                inner_angle: 0.4,
                outer_angle: 0.8,
                cast_shadows: true,
                source_radius: None,
            })],
        }
    }

    fn mesh<'g>(
        &'g self,
        key: u64,
        mesh_key: u64,
        geometry: &'g (Vec<LabVertex>, Vec<u32>),
        at: Vec3,
    ) -> FrameMesh<'g> {
        FrameMesh {
            is_static: false,
            mesh_key,
            material_key: 1,
            object_key: key,
            vertices: geometry.0.as_slice(),
            indices: geometry.1.as_slice(),
            material: &self.material,
            model: Mat4::from_translation(at).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    /// The ground, the slab (when `slab`), the field under it, and a `mover` box at `mover_x`.
    fn meshes(&self, slab: bool, mover_x: f32) -> Vec<FrameMesh<'_>> {
        let mut out = vec![self.mesh(1, 1, &self.ground, Vec3::new(0.0, -0.05, 0.0))];
        if slab {
            out.push(self.mesh(2, 2, &self.slab, Vec3::new(0.0, 3.0, 0.0)));
        }
        for i in 0..FIELD * FIELD {
            let (col, row) = ((i % FIELD) as f32, (i / FIELD) as f32);
            let at = Vec3::new(-2.4 + col * 0.32, 0.15 + (i % 3) as f32 * 0.2, -2.4 + row * 0.32);
            out.push(self.mesh(i as u64 + 10, 3, &self.cube, at));
        }
        out.push(self.mesh(5, 3, &self.cube, Vec3::new(mover_x, 0.6, 0.4)));
        out
    }
}

fn quality(paged: bool) -> FrameQuality {
    let base = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    FrameQuality {
        csm_cascade_count: CASCADES as u32,
        csm_resolution: 512,
        local_shadow_atlas_size: 512,
        paged_sun_shadows: paged,
        taau: false,
        ..base
    }
}

fn scene<'a>(
    meshes: Vec<FrameMesh<'a>>,
    lights: &'a [LabLocalLight],
    quality: FrameQuality,
) -> FrameScene<'a> {
    FrameScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 2.5, 7.5],
        target: [0.0, 0.5, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.5,
        double_sided: false,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.15, -1.0, -0.1], [1.0, 1.0, 1.0], 3.0),
        local_lights: lights,
        quality,
        meshes,
    }
}

/// `None` when the device cannot run the GPU-driven shadow passes: the cull never runs there.
fn device() -> Option<RenderDevice> {
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip: the device draws no GPU objects (no multi-draw indirect with a first instance)");
        return None;
    }
    Some(device)
}

fn mismatched(a: &[u8], b: &[u8]) -> usize {
    a.chunks(4).zip(b.chunks(4)).filter(|(x, y)| x != y).count()
}

/// Every shadow map of `culled` equals `plain`'s, texel for texel (bit patterns).
fn assert_same_maps(device: &RenderDevice, culled: &FrameRenderer, plain: &FrameRenderer, label: &str) {
    for kind in (0..CASCADES).chain([4]) {
        let a = culled
            .shadow_map_depth(device, kind)
            .expect("readback")
            .expect("map");
        let b = plain
            .shadow_map_depth(device, kind)
            .expect("readback")
            .expect("map");
        assert_eq!(a.len(), b.len(), "{label}: kind {kind} size");
        let differ = a
            .iter()
            .zip(&b)
            .filter(|(x, y)| x.to_bits() != y.to_bits())
            .count();
        assert_eq!(
            differ, 0,
            "{label}: kind {kind}: {differ} texels differ from the unculled map"
        );
        assert!(
            a.iter().any(|d| *d < 1.0),
            "{label}: kind {kind} holds no caster (the scene does not exercise it)"
        );
    }
}

/// Renders one frame with both renderers, checks maps and pixels, returns the culled one's counts.
fn frame(
    device: &RenderDevice,
    culled: &mut FrameRenderer,
    plain: &mut FrameRenderer,
    scene: &FrameScene<'_>,
    label: &str,
) -> Option<ShadowOcclusionCounters> {
    let a = culled.render_offscreen(device, scene).expect("culled frame");
    let b = plain.render_offscreen(device, scene).expect("plain frame");
    assert_eq!(
        mismatched(&a.pixels, &b.pixels),
        0,
        "{label}: the shadow cull changed the frame"
    );
    assert_same_maps(device, culled, plain, label);
    assert_eq!(
        plain.shadow_occlusion_stats().skipped,
        Some(ShadowOcclusionSkip::Off),
        "{label}"
    );
    let counts = culled
        .shadow_occlusion_counters(device)
        .expect("counter readback");
    eprintln!("{label}: {:?} {counts:?}", culled.shadow_occlusion_stats());
    counts
}

fn renderers(min_casters: u32) -> (FrameRenderer, FrameRenderer) {
    let mut culled = FrameRenderer::new();
    culled.set_shadow_occlusion(true);
    culled.set_shadow_occlusion_min_casters(Some(min_casters));
    let plain = FrameRenderer::new();
    assert!(culled.shadow_occlusion());
    assert!(!plain.shadow_occlusion());
    (culled, plain)
}

#[test]
fn culled_shadow_maps_match_the_unculled_maps_and_skip_hidden_casters() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    let (mut culled, mut plain) = renderers(64);
    let q = quality(false);

    // First frame: no pyramid yet, so phase 1 draws every caster.
    let s = scene(geometry.meshes(true, 5.0), &geometry.lights, q);
    let first = frame(&device, &mut culled, &mut plain, &s, "first frame").expect("culled frame ran");
    let stats = culled.shadow_occlusion_stats();
    assert!(stats.active, "{stats:?}");
    for kind in (0..CASCADES).chain([4]) {
        assert!(stats.kinds[kind].active, "kind {kind}: {stats:?}");
        assert_eq!(stats.kinds[kind].history_jobs, 0, "kind {kind}: {stats:?}");
        assert_eq!(stats.kinds[kind].unbounded, 0, "kind {kind}: {stats:?}");
    }
    assert_eq!(first.total.phase2, 0, "{first:?}");
    assert_eq!(first.total.culled, 0, "{first:?}");
    assert!(stats.pyramid_bytes > 0, "{stats:?}");

    // Second frame, same view (paging off: every map is redrawn): the boxes under the slab are
    // skipped, and phase 2 finds none of them visible.
    let second = frame(&device, &mut culled, &mut plain, &s, "second frame").expect("culled frame ran");
    let stats = culled.shadow_occlusion_stats();
    for kind in (0..CASCADES).chain([4]) {
        assert_eq!(
            stats.kinds[kind].history_jobs, stats.kinds[kind].jobs,
            "kind {kind}: {stats:?}"
        );
    }
    assert_eq!(second.total.phase2, 0, "{second:?}");
    assert!(
        second.kinds[0].culled > 50,
        "the slab hid too few boxes from the sun: {second:?}"
    );
    assert!(
        second.kinds[4].culled > 50,
        "the slab hid too few boxes from the spot: {second:?}"
    );

    // The slab goes: last redraw's pyramid still hides the boxes for phase 1, and phase 2 (this
    // frame's depth) draws them.
    let s = scene(geometry.meshes(false, 5.0), &geometry.lights, q);
    let third = frame(&device, &mut culled, &mut plain, &s, "slab removed").expect("culled frame ran");
    assert!(
        third.kinds[0].phase2 > 50,
        "phase 2 missed the uncovered boxes: {third:?}"
    );
    assert!(
        third.kinds[4].phase2 > 50,
        "phase 2 missed the uncovered boxes: {third:?}"
    );

    // Settled: phase 1 has the pyramid without the slab.
    let fourth = frame(&device, &mut culled, &mut plain, &s, "settled").expect("culled frame ran");
    assert_eq!(fourth.total.phase2, 0, "{fourth:?}");

    // The slab comes back: every map still matches.
    let s = scene(geometry.meshes(true, 5.0), &geometry.lights, q);
    frame(&device, &mut culled, &mut plain, &s, "slab back");
    frame(&device, &mut culled, &mut plain, &s, "slab back, settled");
}

/// The harder scene: casters the lists batch apart (two meshes, a double-sided material, a masked
/// material that passes its alpha test and one that discards every fragment), shadow scale and
/// lift on some, under a camera and lights that move every frame, with lights joining and leaving
/// the atlas (so tiles change owner) and the slab going away for two steps.
struct MixedScene {
    ground: (Vec<LabVertex>, Vec<u32>),
    slab: (Vec<LabVertex>, Vec<u32>),
    cube: (Vec<LabVertex>, Vec<u32>),
    tall: (Vec<LabVertex>, Vec<u32>),
    materials: [LabMaterial; 4],
}

impl MixedScene {
    fn new() -> Self {
        let plain = LabMaterial {
            base_color: [0.6, 0.55, 0.5, 1.0],
            roughness: 0.7,
            ..LabMaterial::default()
        };
        let masked = |alpha: f32| LabMaterial {
            base_color: [0.4, 0.5, 0.4, alpha],
            alpha_mode: ALPHA_MASK,
            alpha_cutoff: 0.5,
            ..LabMaterial::default()
        };
        Self {
            ground: box_mesh([8.0, 0.05, 8.0]),
            slab: box_mesh([3.2, 0.05, 3.2]),
            cube: box_mesh([0.1, 0.1, 0.1]),
            tall: box_mesh([0.08, 0.5, 0.08]),
            materials: [
                plain,
                LabMaterial {
                    double_sided: true,
                    ..LabMaterial::default()
                },
                masked(0.9),
                masked(0.2),
            ],
        }
    }

    fn mesh<'g>(
        &'g self,
        key: u64,
        mesh: (u64, &'g (Vec<LabVertex>, Vec<u32>)),
        material: usize,
        shadow: ShadowCast,
        at: Vec3,
    ) -> FrameMesh<'g> {
        FrameMesh {
            is_static: false,
            mesh_key: mesh.0,
            material_key: material as u64 + 1,
            object_key: key,
            vertices: mesh.1.0.as_slice(),
            indices: mesh.1.1.as_slice(),
            material: &self.materials[material],
            model: Mat4::from_translation(at).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    fn meshes(&self, slab: bool) -> Vec<FrameMesh<'_>> {
        let mut out = vec![self.mesh(
            1,
            (1, &self.ground),
            0,
            ShadowCast::All,
            Vec3::new(0.0, -0.05, 0.0),
        )];
        if slab {
            out.push(self.mesh(2, (2, &self.slab), 0, ShadowCast::All, Vec3::new(0.0, 3.0, 0.0)));
        }
        for i in 0..FIELD * FIELD {
            let (col, row) = ((i % FIELD) as f32, (i / FIELD) as f32);
            let at = Vec3::new(-2.4 + col * 0.32, 0.15 + (i % 3) as f32 * 0.2, -2.4 + row * 0.32);
            let mesh = if i % 5 == 0 {
                (4, &self.tall)
            } else {
                (3, &self.cube)
            };
            // A short caster the shadow passes draw larger and higher than it is drawn: its bound
            // must be the shadow's, not the mesh's.
            let shadow = if i % 7 == 0 {
                ShadowCast::Local {
                    reach: 6.0,
                    max_cascades: 4,
                    shadow_scale: 1.5,
                    shadow_lift: 0.3,
                    anchor: None,
                    lod_switch_keeps_pages: false,
                }
            } else {
                ShadowCast::All
            };
            out.push(self.mesh(i as u64 + 10, mesh, i % 4, shadow, at));
        }
        out
    }
}

fn spot(position: [f32; 3]) -> LabLocalLight {
    LabLocalLight::Spot(LabSpotLight {
        position,
        direction: [0.0, -1.0, 0.0],
        color: [1.0, 0.9, 0.8],
        intensity: 400.0,
        range: 14.0,
        inner_angle: 0.4,
        outer_angle: 0.8,
        cast_shadows: true,
        source_radius: None,
    })
}

/// Eight frames with a new camera, sun and light set each: the maps and frames must match the
/// unculled renderer's on every one, and the cull must really have run with history and phase 2.
fn mixed_moving_scene_is_exact(paged: bool) {
    let Some(device) = device() else {
        return;
    };
    let geometry = MixedScene::new();
    let (mut culled, mut plain) = renderers(1);
    culled.set_alpha_tested_shadows(true);
    plain.set_alpha_tested_shadows(true);
    let q = quality(paged);
    let (mut total_culled, mut total_phase2, mut with_history) = (0, 0, 0);
    for step in 0..8u32 {
        let t = step as f32;
        let mut lights = vec![spot([0.3 + t * 0.15, 6.0, 0.2])];
        if step >= 3 {
            lights.push(LabLocalLight::Point(LabPointLight {
                position: [1.5, 2.0, 1.0 - t * 0.1],
                color: [0.8, 0.9, 1.0],
                intensity: 600.0,
                radius: 9.0,
                cast_shadows: true,
                source_radius: None,
            }));
        }
        if step >= 5 {
            lights.push(spot([-1.2, 5.5, -0.8 + t * 0.05]));
        }
        if step == 7 {
            // The first spot leaves: the others' tiles may change rectangles.
            lights.remove(0);
        }
        let slab = !(4..=5).contains(&step);
        let mut s = scene(geometry.meshes(slab), &lights, q);
        s.eye = [t * 0.35 - 1.0, 2.5 + t * 0.1, 7.5 - t * 0.2];
        s.directional =
            DirectionalLightUniform::new([-0.15 + t * 0.02, -1.0, -0.1 + t * 0.01], [1.0, 1.0, 1.0], 3.0);
        let label = format!("{} step {step}", if paged { "paged" } else { "whole" });
        let counts = frame(&device, &mut culled, &mut plain, &s, &label);
        let stats = culled.shadow_occlusion_stats();
        for kind in (0..CASCADES).chain([4]) {
            let k = stats.kinds[kind];
            if let Some(counts) = counts.filter(|_| k.active) {
                let c = counts.kinds[kind];
                assert_eq!(
                    c.phase1 + c.phase2 + c.culled,
                    k.instances,
                    "{label}: kind {kind}: every caster is counted once: {c:?} {k:?}"
                );
            }
            with_history += k.history_jobs;
        }
        if let Some(counts) = counts.filter(|_| stats.active) {
            total_culled += counts.total.culled;
            total_phase2 += counts.total.phase2;
        }
    }
    assert!(with_history > 0, "no job tested a moved pyramid");
    assert!(total_culled > 50, "the scene hid too few casters: {total_culled}");
    assert!(
        total_phase2 > 0,
        "the slab going away must bring casters back through phase 2"
    );
}

#[test]
fn mixed_casters_under_moving_views_match_the_unculled_maps() {
    mixed_moving_scene_is_exact(false);
}

#[test]
fn mixed_casters_under_moving_views_match_the_unculled_maps_when_paged() {
    mixed_moving_scene_is_exact(true);
}

#[test]
fn paged_cascades_cull_their_dirty_pages_exactly() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    // Page jobs hold only the casters over the page: cull from the first instance.
    let (mut culled, mut plain) = renderers(1);
    let q = quality(true);
    frame(
        &device,
        &mut culled,
        &mut plain,
        &scene(geometry.meshes(true, -2.0), &geometry.lights, q),
        "first",
    );
    // Nothing moved: every cascade and tile keeps its depth.
    frame(
        &device,
        &mut culled,
        &mut plain,
        &scene(geometry.meshes(true, -2.0), &geometry.lights, q),
        "held",
    );
    assert_eq!(
        culled.shadow_occlusion_stats().skipped,
        Some(ShadowOcclusionSkip::NoJobs),
        "{:?}",
        culled.shadow_occlusion_stats()
    );
    // A box walks out from under the slab: its old and new pages are cleared and redrawn,
    // scissored, with the rest of the cascade loaded.
    let mut paged_frames = 0;
    for (step, x) in [-1.4f32, -0.6, 0.4, 1.6, 2.8, 4.0, 5.2].into_iter().enumerate() {
        frame(
            &device,
            &mut culled,
            &mut plain,
            &scene(geometry.meshes(true, x), &geometry.lights, q),
            &format!("mover step {step}"),
        );
        let stats = culled.shadow_occlusion_stats();
        if stats
            .kinds
            .iter()
            .take(CASCADES)
            .any(|k| k.active && k.history_jobs > 0)
        {
            paged_frames += 1;
        }
    }
    assert!(paged_frames >= 3, "too few page redraws culled with history");
}

#[test]
fn switched_off_or_small_frames_skip_with_a_reason_and_hold_nothing() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    let lights = &geometry.lights;
    let mut renderer = FrameRenderer::new();
    let s = scene(geometry.meshes(true, 5.0), lights, quality(false));
    renderer.render_offscreen(&device, &s).expect("frame");
    let stats = renderer.shadow_occlusion_stats();
    assert!(!stats.enabled && !stats.active);
    assert_eq!(stats.skipped, Some(ShadowOcclusionSkip::Off));
    assert_eq!(stats.pyramid_bytes, 0);
    assert!(
        renderer
            .shadow_occlusion_counters(&device)
            .expect("readback")
            .is_none()
    );

    // The default threshold (512) is above this scene's 259 casters per kind.
    renderer.set_shadow_occlusion(true);
    renderer.render_offscreen(&device, &s).expect("frame");
    let stats = renderer.shadow_occlusion_stats();
    assert!(stats.enabled && !stats.active, "{stats:?}");
    assert_eq!(stats.skipped, Some(ShadowOcclusionSkip::FewCasters));
    assert_eq!(stats.min_casters, regia_render::SHADOW_OCCLUSION_MIN_CASTERS);

    renderer.set_shadow_occlusion_min_casters(Some(1));
    renderer.render_offscreen(&device, &s).expect("frame");
    let stats = renderer.shadow_occlusion_stats();
    assert!(stats.active && stats.pyramid_bytes > 0, "{stats:?}");
    assert_eq!(stats.frames, 1);
    let three_kinds = stats.pyramid_bytes;

    // One cascade fewer: the second cascade's map goes, and its pyramid with it.
    let fewer = FrameQuality {
        csm_cascade_count: 1,
        ..quality(false)
    };
    let s1 = scene(geometry.meshes(true, 5.0), lights, fewer);
    renderer.render_offscreen(&device, &s1).expect("frame");
    let stats = renderer.shadow_occlusion_stats();
    assert!(stats.active, "{stats:?}");
    assert!(
        stats.pyramid_bytes < three_kinds,
        "the dropped cascade's pyramid is still held: {stats:?}"
    );
    renderer.render_offscreen(&device, &s).expect("frame");

    // Off again: the pyramids go, the threshold stays.
    renderer.set_shadow_occlusion(false);
    renderer.render_offscreen(&device, &s).expect("frame");
    let stats = renderer.shadow_occlusion_stats();
    assert_eq!(stats.skipped, Some(ShadowOcclusionSkip::Off));
    assert_eq!(stats.pyramid_bytes, 0);
    assert_eq!(stats.min_casters, 1);
    // Three culled frames: the first one on, one cascade fewer, and both cascades again.
    assert_eq!(stats.frames, 3);
}

/// The shadow passes' GPU time with the cull on and off (`--ignored`; prints, asserts nothing
/// about time): a 64×64 field of boxes under the slab, four 2048² cascades redrawn every frame
/// (paging off) and the spot light's atlas. `shadows_ms` spans every shadow kind, the pyramid
/// builds and both phases' passes; phase 1's dispatches record just before the first kind's
/// stamp. On lavapipe this is a correctness-path number only: the lead runs it on a card.
#[test]
#[ignore = "timing printout; run on the bench GPU"]
fn shadow_occlusion_cost() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    let quality = FrameQuality {
        csm_cascade_count: 4,
        csm_resolution: 2048,
        local_shadow_atlas_size: 2048,
        ..quality(false)
    };
    let dense = |slab: bool| {
        let mut meshes = geometry.meshes(slab, 5.0);
        for i in 0..64 * 64usize {
            let (col, row) = ((i % 64) as f32, (i / 64) as f32);
            let at = Vec3::new(
                -3.0 + col * 0.095,
                0.12 + (i % 5) as f32 * 0.1,
                -3.0 + row * 0.095,
            );
            meshes.push(geometry.mesh(i as u64 + 1000, 3, &geometry.cube, at));
        }
        meshes
    };
    for (on, slab) in [(false, true), (true, true), (false, false), (true, false)] {
        let mut renderer = FrameRenderer::new();
        renderer.set_gpu_timing(&device, true);
        renderer.set_shadow_occlusion(on);
        let mut s = scene(dense(slab), &geometry.lights, quality);
        s.width = 1280;
        s.height = 720;
        for _ in 0..6 {
            renderer.render_offscreen(&device, &s).expect("warm-up");
        }
        let frames = 16;
        let mut shadows = 0.0f64;
        for _ in 0..frames {
            renderer.render_offscreen(&device, &s).expect("frame");
            shadows += f64::from(renderer.gpu_timings().shadows_ms);
        }
        let counts = on.then(|| renderer.shadow_occlusion_counters(&device).ok().flatten());
        eprintln!(
            "shadow_occlusion={on} slab={slab}: shadows {:.3} ms (mean of {frames}); phases {counts:?}",
            shadows / f64::from(frames),
        );
    }
}

/// The shadow casters' phase 1 is the frame graph's optional `shadow_occlusion` node (ADR-5178):
/// switched on, the graph holds it right before `shadows`, the frame records it through the
/// graph's dispatch and the cull runs; switched off, the node leaves the graph, nothing records
/// it, it holds no pyramid, and the frame is the unculled frame byte for byte. Every frame closes
/// its dispatch with no violation.
#[test]
fn shadow_occlusion_is_added_and_removed_as_a_graph_node() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    let s = scene(geometry.meshes(true, 5.0), &geometry.lights, quality(false));
    let mut plain = FrameRenderer::new();
    let mut renderer = FrameRenderer::new();
    renderer.set_shadow_occlusion_min_casters(Some(64));
    let held = |r: &FrameRenderer| {
        r.graph_report()
            .graph
            .nodes
            .iter()
            .any(|n| n.name == "shadow_occlusion")
    };
    let clean = |r: &FrameRenderer, label: &str| {
        let report = r.graph_report();
        assert_eq!(
            report.dispatch.violations, 0,
            "{label}: {:?}",
            report.dispatch.last_violation
        );
        assert!(
            report.dispatch.skipped.is_empty(),
            "{label}: {:?}",
            report.dispatch
        );
        report.dispatch.recorded
    };

    // Off (the default): not in the graph, never recorded, nothing held.
    assert!(!held(&renderer));
    let a = renderer.render_offscreen(&device, &s).expect("frame");
    let b = plain.render_offscreen(&device, &s).expect("frame");
    assert_eq!(mismatched(&a.pixels, &b.pixels), 0);
    let recorded = clean(&renderer, "off");
    assert!(!recorded.contains(&"shadow_occlusion"), "{recorded:?}");
    assert_eq!(recorded.first(), Some(&"shadows"), "{recorded:?}");
    let stats = renderer.shadow_occlusion_stats();
    assert_eq!(stats.skipped, Some(ShadowOcclusionSkip::Off));
    assert_eq!(stats.pyramid_bytes, 0);

    // On: the graph holds it right before `shadows`, the frame records it, the cull runs, and
    // the maps and the frame stay the unculled ones.
    renderer.set_shadow_occlusion(true);
    assert!(held(&renderer));
    let order = renderer.graph_report().graph.order;
    assert_eq!(&order[..2], ["shadow_occlusion", "shadows"], "{order:?}");
    for frame_no in 0..2 {
        let label = format!("on, frame {frame_no}");
        let a = renderer.render_offscreen(&device, &s).expect("frame");
        let b = plain.render_offscreen(&device, &s).expect("frame");
        assert_eq!(mismatched(&a.pixels, &b.pixels), 0, "{label}");
        assert_same_maps(&device, &renderer, &plain, &label);
        let recorded = clean(&renderer, &label);
        assert_eq!(
            &recorded[..2],
            ["shadow_occlusion", "shadows"],
            "{label}: {recorded:?}"
        );
        let stats = renderer.shadow_occlusion_stats();
        assert!(stats.active && stats.pyramid_bytes > 0, "{label}: {stats:?}");
    }
    let counts = renderer
        .shadow_occlusion_counters(&device)
        .expect("readback")
        .expect("a culled frame");
    assert!(
        counts.total.culled > 0,
        "the second frame culled nothing: {counts:?}"
    );

    // Off again: the node leaves the graph, the pyramids go, nothing records it.
    renderer.set_shadow_occlusion(false);
    assert!(!held(&renderer));
    let a = renderer.render_offscreen(&device, &s).expect("frame");
    let b = plain.render_offscreen(&device, &s).expect("frame");
    assert_eq!(mismatched(&a.pixels, &b.pixels), 0);
    let recorded = clean(&renderer, "off again");
    assert!(!recorded.contains(&"shadow_occlusion"), "{recorded:?}");
    let stats = renderer.shadow_occlusion_stats();
    assert_eq!(stats.skipped, Some(ShadowOcclusionSkip::Off));
    assert_eq!(stats.pyramid_bytes, 0);
    assert_eq!(stats.frames, 2, "{stats:?}");
}

/// Review fixrev-graph-dispatch-g finding 1 (ADR-5626): `render.graph` lists `shadow_occlusion`
/// as recorded only on a frame where phase 1 can cull. With the switch on, a CPU-culled frame
/// (`gpu_driven` off: no GPU objects) and a paged frame that redraws no map (no shadow jobs)
/// list the held node as inactive, the cull's report still says why it skipped, the frame is the
/// unculled frame byte for byte, and the dispatch counts no violation.
#[test]
fn shadow_occlusion_is_inactive_on_frames_where_phase1_cannot_cull() {
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    let geometry = Geometry::new();
    let (mut culled, mut plain) = renderers(1);
    let check = |r: &FrameRenderer, label: &str, active: bool| {
        let report = r.graph_report();
        let dispatch = &report.dispatch;
        assert_eq!(dispatch.violations, 0, "{label}: {:?}", dispatch.last_violation);
        assert!(dispatch.skipped.is_empty(), "{label}: {dispatch:?}");
        assert_eq!(
            dispatch.recorded.contains(&"shadow_occlusion"),
            active,
            "{label}: {dispatch:?}"
        );
        assert_eq!(
            dispatch.inactive.contains(&"shadow_occlusion"),
            !active,
            "{label}: {dispatch:?}"
        );
        assert!(dispatch.recorded.contains(&"shadows"), "{label}: {dispatch:?}");
    };

    // CPU-culled: the shadow lists are not GPU-driven on any device.
    let cpu = FrameQuality {
        gpu_driven: false,
        ..quality(false)
    };
    let s = scene(geometry.meshes(true, 5.0), &geometry.lights, cpu);
    let a = culled.render_offscreen(&device, &s).expect("frame");
    let b = plain.render_offscreen(&device, &s).expect("frame");
    assert_eq!(mismatched(&a.pixels, &b.pixels), 0);
    let stats = culled.shadow_occlusion_stats();
    assert_eq!(
        stats.skipped,
        Some(ShadowOcclusionSkip::NoGpuObjects),
        "{stats:?}"
    );
    check(&culled, "cpu-culled", false);

    // GPU-driven and paged: the first frame culls; the next one redraws no map.
    if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip (GPU part): the device draws no GPU objects");
        return;
    }
    let paged = scene(geometry.meshes(true, -2.0), &geometry.lights, quality(true));
    frame(&device, &mut culled, &mut plain, &paged, "paged first");
    assert!(culled.shadow_occlusion_stats().active);
    check(&culled, "paged first", true);
    frame(&device, &mut culled, &mut plain, &paged, "paged held");
    let stats = culled.shadow_occlusion_stats();
    assert_eq!(stats.skipped, Some(ShadowOcclusionSkip::NoJobs), "{stats:?}");
    check(&culled, "paged held", false);
}
