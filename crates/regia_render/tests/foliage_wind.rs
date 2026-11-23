// SPDX-License-Identifier: MPL-2.0
//! Foliage wind on the dynamic-caster shadow class (`specs/88`, ADR-2028), on lavapipe.
//!
//! A vertical card with the ready-made [`FoliageSway`] stands on a ground plane under a low sun,
//! with paged sun cascades. The sway moves the card inside its bounds, so the page cache cannot
//! see it move. The oracle is a fresh renderer, which draws every page: after the wind changes,
//! a warm renderer with the class on must match it, and with the class off it must not (its pages
//! still hold the old pose).

use std::sync::Arc;
use std::time::Instant;

use regia_render::{
    DirectionalLightUniform, DynamicCasterShadows, FoliageSway, FrameMesh, FrameQuality, FrameRenderer,
    LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, LabVertex, OutdoorParams, RenderDevice,
    SharedFrameInfo, SharedPass, SharedPassDraw, SharedUpload,
};

/// Opaque green, lit (the shadow on the ground is what the test reads).
const SURFACE: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::Surface(
        vec3<f32>(0.2, 0.6, 0.15),
        1.0,
        0.0,
        0.8,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}
";

const SIZE: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A 1.6 m tall card that bends 0.16-0.8 m at its top between 2 and 12 m/s of wind.
fn sway() -> FoliageSway {
    FoliageSway {
        stiffness: 0.08,
        height_m: 1.6,
        flutter_m: 0.0,
        gust_period_s: 4.0,
        max_wind_m_per_s: 15.0,
        ..FoliageSway::default()
    }
}

fn ground() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 1.0, 0.0];
    let e = 4.0;
    (
        vec![
            LabVertex::from_pos_normal_uv([-e, 0.0, -e], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([e, 0.0, -e], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([e, 0.0, e], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-e, 0.0, e], n, [0.0, 1.0]),
        ],
        vec![0, 2, 1, 0, 3, 2],
    )
}

/// The card in the plane x = 0, z in [-0.6, 0.6], y in [0, 1.6]: it faces the sun (which
/// travels toward +x), so its shadow falls along +x, the way the wind bends it.
fn card() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [-1.0, 0.0, 0.0];
    let h = sway().height_m;
    // Two rows so the bend (h²) has vertices in the middle of the card.
    let mut vertices = Vec::new();
    for (row, y) in [0.0, 0.5 * h, h].into_iter().enumerate() {
        for z in [-0.6, 0.6] {
            vertices.push(LabVertex::from_pos_normal_uv(
                [0.0, y, z],
                n,
                [f32::from(z > 0.0), row as f32 * 0.5],
            ));
        }
    }
    (vertices, vec![0, 1, 3, 0, 3, 2, 2, 3, 5, 2, 5, 4])
}

fn lab() -> LabScene {
    let (vertices, indices) = ground();
    LabScene {
        width: SIZE,
        height: SIZE,
        eye: [0.5, 4.5, 4.5],
        target: [1.2, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 30.0,
        depth_prepass: false,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.8, -0.6, 0.1], [1.0; 3], 40_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices,
            indices,
            ..LabMesh::unskinned()
        }],
    }
}

fn paged() -> FrameQuality {
    FrameQuality {
        paged_sun_shadows: true,
        ..FrameQuality::LAB
    }
}

struct Fixture {
    lab: LabScene,
    card_vertices: Vec<LabVertex>,
    card_indices: Vec<u32>,
    material: LabMaterial,
}

impl Fixture {
    fn new() -> Self {
        let (card_vertices, card_indices) = card();
        Self {
            lab: lab(),
            card_vertices,
            card_indices,
            material: LabMaterial {
                surface_graph: Some(Arc::new(sway().graph(SURFACE))),
                ..LabMaterial::default()
            },
        }
    }

    fn render(
        &self,
        renderer: &mut FrameRenderer,
        device: &RenderDevice,
        wind: [f32; 2],
        time: f32,
    ) -> Vec<u8> {
        let mut scene = self.lab.as_frame_scene_with_quality(paged());
        let template = scene.meshes[0];
        scene.meshes.push(FrameMesh {
            mesh_key: 0x0F01_1A6E << 3,
            object_key: 0x0F01_1A6E,
            material_key: 0x0F01_1A6F,
            material: &self.material,
            vertices: &self.card_vertices,
            indices: &self.card_indices,
            ..template
        });
        renderer.set_foliage_wind(Some(wind));
        renderer.set_material_clock(time, time - 1.0 / 60.0);
        match renderer.render_offscreen(device, &scene) {
            Ok(frame) => frame.pixels,
            Err(err) => panic!("frame: {err}"),
        }
    }
}

/// Pixels (RGBA8) whose largest channel difference is above `tolerance`.
fn differing(a: &[u8], b: &[u8], tolerance: u8) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > tolerance))
        .count()
}

const CALM: [f32; 2] = [2.0, 0.0];
const GALE: [f32; 2] = [12.0, 0.0];
const TIME: f32 = 1.0;

#[test]
fn a_swaying_casters_shadow_follows_the_wind_with_the_class_on() {
    let device = device();
    let fixture = Fixture::new();
    // What the gale looks like with every page drawn.
    let truth = fixture.render(&mut FrameRenderer::new(), &device, GALE, TIME);
    let calm = fixture.render(&mut FrameRenderer::new(), &device, CALM, TIME);
    let moved = differing(&truth, &calm, 2);
    assert!(
        moved > 20,
        "the wind must move the card and its shadow: {moved} pixels"
    );

    let on = DynamicCasterShadows::On {
        cascades: 2,
        local_lights: false,
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_dynamic_caster_shadows(on);
    fixture.render(&mut renderer, &device, CALM, TIME);
    fixture.render(&mut renderer, &device, CALM, TIME);
    let warm = fixture.render(&mut renderer, &device, GALE, TIME);
    let report = renderer.dynamic_caster_report();
    assert_eq!(report.animated_casters, 1, "{report:?}");
    assert_eq!(report.policy.cascade_mask, 0b11, "{report:?}");
    assert!(
        report.dynamic_pages > 0,
        "the card's pages are refreshed: {report:?}"
    );
    let stale = differing(&warm, &truth, 2);
    assert_eq!(
        stale, 0,
        "with the class on the cached frame matches a full redraw"
    );
}

#[test]
fn with_the_class_off_the_cached_shadow_keeps_the_old_pose() {
    let device = device();
    let fixture = Fixture::new();
    let truth = fixture.render(&mut FrameRenderer::new(), &device, GALE, TIME);

    let mut renderer = FrameRenderer::new();
    renderer.set_dynamic_caster_shadows(DynamicCasterShadows::Off);
    fixture.render(&mut renderer, &device, CALM, TIME);
    let calm = fixture.render(&mut renderer, &device, CALM, TIME);
    let warm = fixture.render(&mut renderer, &device, GALE, TIME);
    let report = renderer.dynamic_caster_report();
    assert_eq!(report.animated_casters, 1, "{report:?}");
    assert_eq!(report.dynamic_pages, 0, "{report:?}");
    let stale = differing(&warm, &truth, 2);
    assert!(
        stale > 5,
        "the cached pages hold the calm pose, so the shadow differs from a full redraw ({stale} pixels)"
    );
    // Spec 88 § 7: the card itself still moves with the class off; only its shadow holds.
    let card_moved = differing(&warm, &calm, 2);
    assert!(
        card_moved > 5,
        "with the class off the card still sways ({card_moved} pixels)"
    );
}

#[test]
fn the_tier_default_refreshes_the_lab_tiers_first_cascade_and_is_deterministic() {
    let device = device();
    let fixture = Fixture::new();
    let mut a = FrameRenderer::new();
    assert_eq!(a.dynamic_caster_shadows(), DynamicCasterShadows::Tier);
    let first = fixture.render(&mut a, &device, GALE, 2.5);
    let again = fixture.render(&mut FrameRenderer::new(), &device, GALE, 2.5);
    assert_eq!(first, again, "same wind and clock, same frame");
    fixture.render(&mut a, &device, GALE, 2.6);
    let report = a.dynamic_caster_report();
    // The lab quality has two cascades: the tier refreshes cascade 0 only.
    assert_eq!(report.policy.cascade_mask, 0b1, "{report:?}");
    assert!(!report.policy.unpaged);
    // Unpaged frames need no refresh.
    let mut unpaged = FrameRenderer::new();
    let mut scene_quality = fixture.lab.as_frame_scene_with_quality(FrameQuality::LAB);
    scene_quality.meshes.truncate(1);
    if let Err(err) = unpaged.render_offscreen(&device, &scene_quality) {
        panic!("unpaged frame: {err}");
    }
    assert!(unpaged.dynamic_caster_report().policy.unpaged);
}

/// A shared draw (terrain grass) that sways in its shadow pass and reports one box.
struct SwayingSharedDraw {
    boxes: Vec<[[f32; 3]; 2]>,
    cascades: u8,
}

impl SharedPassDraw for SwayingSharedDraw {
    fn prepare(&mut self, _info: &SharedFrameInfo, _upload: &mut SharedUpload<'_>) {}
    fn draw<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>, _kind: SharedPass) {}
    fn shadow_key(&self) -> Option<u64> {
        Some(0x6A55)
    }
    fn dynamic_shadow_boxes(&self) -> &[[[f32; 3]; 2]] {
        &self.boxes
    }
    fn dynamic_shadow_box_cascades(&self) -> u8 {
        self.cascades
    }
}

/// Records the wind each frame hands shared draws.
struct WindProbe(std::sync::Arc<std::sync::Mutex<Vec<Option<[f32; 2]>>>>);

impl SharedPassDraw for WindProbe {
    fn prepare(&mut self, info: &SharedFrameInfo, _upload: &mut SharedUpload<'_>) {
        match self.0.lock() {
            Ok(mut seen) => seen.push(info.foliage_wind),
            Err(err) => panic!("probe lock: {err}"),
        }
    }
    fn draw<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>, _kind: SharedPass) {}
}

/// A direct `set_foliage_wind` reaches shared draws. A `SceneWind` (spec 99 § 3.3) sets the same
/// foliage wind for materials but stays away from shared draws: the terrain's wind follows that
/// resource already, so grass would multiply by the speed twice (ADR-2043).
#[test]
fn shared_draws_take_a_direct_wind_but_not_the_scene_winds() {
    let device = device();
    let lab = lab();
    let scene = lab.as_frame_scene_with_quality(paged());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(WindProbe(seen.clone()))));
    let frame = |renderer: &mut FrameRenderer| {
        if let Err(err) = renderer.render_offscreen(&device, &scene) {
            panic!("frame: {err}");
        }
    };
    renderer.set_foliage_wind(Some([3.0, 0.0]));
    frame(&mut renderer);
    renderer.set_outdoor(OutdoorParams {
        scene_wind: Some(Some([7.0, 1.0])),
        ..OutdoorParams::default()
    });
    frame(&mut renderer);
    assert_eq!(
        renderer.foliage_wind(),
        Some([7.0, 1.0]),
        "materials follow the scene wind"
    );
    renderer.set_outdoor(OutdoorParams::default());
    renderer.set_foliage_wind(Some([2.0, 0.0]));
    frame(&mut renderer);
    let seen = match seen.lock() {
        Ok(seen) => seen.clone(),
        Err(err) => panic!("probe lock: {err}"),
    };
    assert_eq!(seen, vec![Some([3.0, 0.0]), None, Some([2.0, 0.0])]);
}

/// `SharedPassDraw::dynamic_shadow_boxes` joins the class: with no animated mesh at all, the
/// box's pages are redrawn every frame while the class refreshes, and never with it off.
#[test]
fn a_shared_draws_swaying_boxes_join_the_class() {
    let device = device();
    let lab = lab();
    let scene = lab.as_frame_scene_with_quality(paged());
    let shared = || {
        Box::new(SwayingSharedDraw {
            boxes: vec![[[1.0, 0.0, -0.5], [1.6, 0.4, 0.5]]],
            cascades: u8::MAX,
        }) as Box<dyn SharedPassDraw + Send + Sync>
    };
    let frames = |renderer: &mut FrameRenderer| {
        for _ in 0..3 {
            if let Err(err) = renderer.render_offscreen(&device, &scene) {
                panic!("frame: {err}");
            }
        }
        renderer.dynamic_caster_report()
    };
    let mut on = FrameRenderer::new();
    on.set_shared_draw(Some(shared()));
    on.set_dynamic_caster_shadows(DynamicCasterShadows::On {
        cascades: 1,
        local_lights: false,
    });
    let report = frames(&mut on);
    assert_eq!(report.animated_casters, 0, "{report:?}");
    assert_eq!(report.shared_boxes, 1, "{report:?}");
    assert!(report.dynamic_pages > 0, "{report:?}");

    let mut off = FrameRenderer::new();
    off.set_shared_draw(Some(shared()));
    off.set_dynamic_caster_shadows(DynamicCasterShadows::Off);
    let report = frames(&mut off);
    assert_eq!(report.shared_boxes, 0, "{report:?}");
    assert_eq!(report.dynamic_pages, 0, "{report:?}");
}

/// A shadowed point light beside the swaying card, so the atlas has a tile that card reaches.
fn light_over_the_card() -> LabLocalLight {
    LabLocalLight::Point(LabPointLight {
        position: [1.3, 1.0, 0.0],
        color: [1.0; 3],
        intensity: 2_000.0,
        radius: 6.0,
        cast_shadows: true,
        source_radius: None,
    })
}

/// The class's local-light half on a whole frame: a swaying mesh keeps its light's atlas tile
/// redrawing, and the same frame with `local_lights` off keeps the tile cached.
#[test]
fn a_swaying_casters_atlas_tile_is_redrawn_every_frame() {
    let device = device();
    let mut fixture = Fixture::new();
    fixture.lab.local_lights = vec![light_over_the_card()];
    let tiles = |local_lights: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_dynamic_caster_shadows(DynamicCasterShadows::On {
            cascades: 1,
            local_lights,
        });
        for step in 0..3 {
            fixture.render(&mut renderer, &device, [6.0, 0.0], 1.0 + step as f32 * 0.2);
        }
        renderer.dynamic_caster_report()
    };
    let on = tiles(true);
    assert_eq!(on.dynamic_atlas_tiles, 1, "{on:?}");
    assert_eq!(tiles(false).dynamic_atlas_tiles, 0);
}

/// Shared geometry draws into sun cascades only (`SharedPass::Shadow`): its boxes refresh the
/// cascades it reports and never an atlas tile, which would redraw without its content.
#[test]
fn shared_boxes_refresh_only_their_cascades_and_no_atlas_tile() {
    let device = device();
    let mut lab = lab();
    lab.local_lights = vec![light_over_the_card()];
    let scene = lab.as_frame_scene_with_quality(paged());
    let refreshed = |cascades: u8| {
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(SwayingSharedDraw {
            boxes: vec![[[1.0, 0.0, -0.5], [1.6, 0.4, 0.5]]],
            cascades,
        }) as Box<dyn SharedPassDraw + Send + Sync>));
        renderer.set_dynamic_caster_shadows(DynamicCasterShadows::On {
            cascades: 2,
            local_lights: true,
        });
        for _ in 0..3 {
            if let Err(err) = renderer.render_offscreen(&device, &scene) {
                panic!("frame: {err}");
            }
        }
        renderer.dynamic_caster_report()
    };
    let every = refreshed(u8::MAX);
    let first = refreshed(0b01);
    assert_eq!((every.shared_boxes, first.shared_boxes), (1, 1));
    assert!(first.dynamic_pages > 0, "{first:?}");
    assert!(every.dynamic_pages > first.dynamic_pages, "{every:?} {first:?}");
    assert_eq!(every.dynamic_atlas_tiles, 0, "{every:?}");
    // A reporter that sways in no cascade costs nothing.
    let none = refreshed(0);
    assert_eq!(none.dynamic_pages, 0, "{none:?}");
    assert_eq!(none.shared_boxes, 0, "{none:?}");
}

/// Frame cost with the class on and off while the clock runs: CPU wall time of whole offscreen
/// frames (correctness-grade only), and on a card with timestamp queries the GPU shadow section
/// and frame (spec 88 § 8's number; ADR-2309). Run with
/// `cargo test -p regia_render --test foliage_wind -- --ignored --nocapture`.
#[test]
#[ignore = "timing print for the lane report; no assertion on time"]
fn bench_dynamic_casters_frame_cost() {
    let device = device();
    let fixture = Fixture::new();
    for (name, mode) in [
        ("off", DynamicCasterShadows::Off),
        (
            "on",
            DynamicCasterShadows::On {
                cascades: 2,
                local_lights: false,
            },
        ),
    ] {
        let mut renderer = FrameRenderer::new();
        renderer.set_dynamic_caster_shadows(mode);
        // GPU timestamps where the device has them (a real card): the shadow section is what
        // spec 88 § 8 budgets. The stamps read back two frames late, so the warm-up covers that.
        renderer.set_gpu_timing(&device, true);
        for warm in 0..3 {
            fixture.render(&mut renderer, &device, GALE, warm as f32 / 60.0);
        }
        let frames = 30;
        let mut pages = 0;
        let (mut shadows_ms, mut frame_ms) = (Vec::new(), Vec::new());
        let started = Instant::now();
        for frame in 0..frames {
            fixture.render(&mut renderer, &device, GALE, 0.05 + frame as f32 / 60.0);
            pages += renderer.dynamic_caster_report().dynamic_pages;
            let gpu = renderer.gpu_timings();
            shadows_ms.push(gpu.shadows_ms);
            frame_ms.push(gpu.frame_ms);
        }
        let ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
        let median = |values: &mut Vec<f32>| {
            values.sort_by(f32::total_cmp);
            values.get(values.len() / 2).copied().unwrap_or(0.0)
        };
        println!(
            "dynamic casters {name}: {ms:.2} ms per {SIZE}x{SIZE} frame (CPU wall, readback), {:.1} dynamic pages per frame, \
             GPU shadows p50 {:.3} ms, GPU frame p50 {:.3} ms (0 without timestamp queries)",
            f64::from(pages) / f64::from(frames),
            median(&mut shadows_ms),
            median(&mut frame_ms),
        );
    }
}
