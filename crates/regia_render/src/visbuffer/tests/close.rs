// SPDX-License-Identifier: MPL-2.0
//! The dense-scene acceptance run (ADR-4906, `docs/specs/98-visibility-buffer.md` § 8): one scene
//! with every kind of instance the path takes (opaque, masked, swaying, lightmap receiver) on
//! cooked cluster DAGs, under a residency budget that cannot hold every fine band, with the LOD
//! band on (ADR-4631) and a camera that cuts between two groups 297 m apart.
//!
//! "Nothing missing" is checked without goldens, on every frame including each cut's first frame:
//! the path's counters hold every instance in its class, the GPU lists drop no cluster, and each
//! rock's footprint prints. A footprint comes from reference frames of the same device (fresh
//! renderers, no residency): the rock's body is where the view with the rock but without its
//! shadow differs from the view without the rock, its shadow where the full view differs from the
//! shadowless one. On every frame most of the body must differ from the rock-less view, most of the
//! shadow must be darker than the shadowless view, and where the body prints it must stay close to
//! the full view (a wrong material moves it far). Once warm, a frame creates no path object and the arena and the
//! path's buffers keep their size across the cuts. The classic fallbacks (a device without the texture table, and one
//! without the path's storage buffers) draw the same instances with the same checks.

use super::residency::dense_rock;
use super::*;
use crate::visbuffer::ClusterResidency;
use crate::{FoliageSway, RenderDevice};
use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};

const GROUND: u64 = 1;
/// Group one's rocks (opaque and masked) share this cooked mesh.
const ROCK_NEAR: u64 = 2;
/// Group two's swaying rock.
const ROCK_SWAY: u64 = 3;
/// Group two's lightmap receiver (its own key: a receiver's mesh carries the UV2 stream).
const ROCK_RECEIVER: u64 = 4;
const RECEIVER_OBJECT: u64 = 13;
/// Where group two stands on Z.
const FAR_Z: f32 = 297.0;
/// The rocks' height and scale: they float, so each sun shadow lands beside its rock.
const ROCK_Y: f32 = 1.0;
const ROCK_SCALE: f32 = 0.45;
/// Light travels down and along +X at 45°, so each rock's shadow falls beside it.
const SUN: [f32; 3] = [1.0, -1.0, 0.0];

/// The four rocks: (object, mesh key, material index, x, z).
const ROCKS: [(u64, u64, usize, f32, f32); 4] = [
    (10, ROCK_NEAR, 0, -0.9, -3.0),
    (11, ROCK_NEAR, 1, 0.9, -3.0),
    (12, ROCK_SWAY, 2, -0.9, FAR_Z),
    (RECEIVER_OBJECT, ROCK_RECEIVER, 3, 0.9, FAR_Z),
];

/// The two views: in front of group one, and in front of group two (group one 300 m behind it,
/// past the far plane).
const VIEWS: [([f32; 3], [f32; 3]); 2] = [
    ([0.0, 1.6, 0.0], [0.0, 0.6, -3.0]),
    ([0.0, 1.6, FAR_Z + 3.0], [0.0, 0.6, FAR_Z]),
];

/// An 8 × 2 atlas with one entry over all of it, bright enough to tell from the probe light.
fn lightmap() -> Lightmap {
    let irr: Vec<[f32; 3]> = (0..16).map(|i| [4_000.0 + 1_000.0 * (i % 8) as f32; 3]).collect();
    Lightmap::from_irradiance(
        LightmapLayout {
            width: 8,
            height: 2,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "rock".into(),
                source_vertices: 4,
                rect: [0, 0, 8, 2],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 1.0,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                indices: vec![0, 1, 2, 0, 2, 3],
            }],
        },
        &irr,
        vec![Coverage::BAKED; 16],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    )
    .expect("valid lightmap")
}

/// The materials of [`ROCKS`] in order: opaque, masked (alpha 1 everywhere, so it prints solid),
/// adopted for sway, and the receiver's plain material.
fn materials(f: &Fixture) -> Vec<LabMaterial> {
    let masked = LabMaterial {
        alpha_mode: crate::ALPHA_MASK,
        alpha_cutoff: 0.5,
        ..f.rock.clone()
    };
    let sway = FoliageSway {
        stiffness: 0.05,
        height_m: 2.0,
        flutter_m: 0.01,
        gust_period_s: 4.0,
        max_wind_m_per_s: 20.0,
        ..FoliageSway::default()
    }
    .adopt(&f.rock)
    .expect("a plain material adopts sway");
    vec![f.rock.clone(), masked, sway, f.rock.clone()]
}

/// What a reference frame leaves out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Omit {
    /// The full scene.
    Nothing,
    /// The rock with this object key.
    Rock(u64),
    /// The shadow of the rock with this object key (the rock stays).
    Shadow(u64),
}

/// The scene at `view`: two grounds (one under each group) and the four rocks, less `omit`.
fn close_scene<'a>(
    f: &'a Fixture,
    post: &'a PostSettings,
    rock: &'a (Vec<LabVertex>, Vec<u32>),
    uv1: &'a [[f32; 2]],
    materials: &'a [LabMaterial],
    view: usize,
    omit: Omit,
) -> FrameScene<'a> {
    let mut s = scene(f, post, true, false);
    let ground = s.meshes[0];
    assert_eq!(ground.mesh_key, GROUND, "the fixture's first mesh is its ground");
    s.meshes = vec![
        FrameMesh {
            object_key: 1,
            ..ground
        },
        FrameMesh {
            object_key: 2,
            model: translate(0.0, -0.05, FAR_Z, 1.0),
            ..ground
        },
    ];
    for &(object_key, mesh_key, material, x, z) in &ROCKS {
        if omit == Omit::Rock(object_key) {
            continue;
        }
        let shadow = if omit == Omit::Shadow(object_key) {
            ShadowCast::Off
        } else {
            ShadowCast::All
        };
        s.meshes.push(FrameMesh {
            is_static: false,
            mesh_key,
            material_key: 2000 + material as u64,
            object_key,
            vertices: &rock.0,
            indices: &rock.1,
            material: &materials[material],
            model: translate(x, ROCK_Y, z, ROCK_SCALE),
            joints: &[],
            receiver_layer: 0,
            shadow,
            uv1: if mesh_key == ROCK_RECEIVER { uv1 } else { &[] },
            lod_dither: 0.0,
        });
    }
    (s.eye, s.target) = VIEWS[view];
    s.directional = crate::DirectionalLightUniform::new(SUN, [1.0; 3], 100_000.0);
    // A frame depends on its own geometry only (as `residency::rock_scene`), so the cut's first
    // frame is judged on what the path drew, not on temporal history.
    s.quality.screen_space_gi = false;
    s.quality.gi_probes = false;
    s.quality.ssr = false;
    s.quality.taau = false;
    s.quality.volumetric_fog = false;
    s.quality.volumetric_clouds = false;
    s.quality.ssao = crate::SsaoMode::Off;
    s
}

/// A 24-column luminance sketch of an RGBA frame, for a failure message.
fn sketch(img: &[u8], width: u32) -> String {
    const RAMP: &[u8] = b" .:-=+*#%@";
    let step = (width / 24).max(1) as usize;
    let width = width as usize;
    let height = img.len() / 4 / width;
    let mut out = String::new();
    for y in (0..height).step_by(step * 2) {
        for x in (0..width).step_by(step) {
            let i = (y * width + x) * 4;
            let l = (u32::from(img[i]) + 2 * u32::from(img[i + 1]) + u32::from(img[i + 2])) / 4;
            out.push(char::from(RAMP[(l as usize * (RAMP.len() - 1)) / 255]));
        }
        out.push('\n');
    }
    out
}

fn max_diff(a: &[u8], b: &[u8], px: usize) -> u8 {
    (0..3)
        .map(|c| a[px * 4 + c].abs_diff(b[px * 4 + c]))
        .max()
        .unwrap_or(0)
}

fn luminance(img: &[u8], px: usize) -> i32 {
    (i32::from(img[px * 4]) + 2 * i32::from(img[px * 4 + 1]) + i32::from(img[px * 4 + 2])) / 4
}

/// Pixels where `a` and `b` differ by more than `levels` in some channel.
fn differing(a: &[u8], b: &[u8], levels: u8) -> Vec<usize> {
    (0..a.len() / 4)
        .filter(|&px| max_diff(a, b, px) > levels)
        .collect()
}

/// One rock's footprint in one view.
struct Footprint {
    label: String,
    /// The view without the rock.
    without: Vec<u8>,
    /// The view with the rock but not its shadow.
    shadowless: Vec<u8>,
    body: Vec<usize>,
    shadow: Vec<usize>,
}

/// A reference frame differs from another by more than this where a footprint is.
const FOOTPRINT: u8 = 24;
/// A frame's pixel counts as the rock's (or as shadowed) when it moves by more than this.
const PRINTS: u8 = 12;
/// The share of a footprint that must print on every frame. A missing rock prints none of it (the
/// frame is the rock-less reference). The coarsest resident cut of the dense rock (the base band,
/// all a residency renderer has on its first frame) covered 90 of 181 body pixels on lavapipe: a
/// smaller, flatter silhouette. A quarter separates the two with room on both sides.
const PRINTS_FRACTION: f64 = 0.25;
/// Mean channel distance from the full reference a frame may have over the body pixels that print
/// the rock: a coarser cut's shading stays well inside it, a wrong material does not.
const BODY_MEAN: f64 = 48.0;

/// Checks `img` against `prints`; panics with `at` and a sketch when something is missing. Returns
/// the largest mean body distance from `full` and the smallest printing share of a body or shadow.
fn check_prints(img: &[u8], full: &[u8], prints: &[Footprint], width: u32, at: &str) -> (f64, f64) {
    let mut worst_mean = 0.0f64;
    let mut least = 1.0f64;
    for p in prints {
        let printing: Vec<usize> = p
            .body
            .iter()
            .copied()
            .filter(|&px| max_diff(img, &p.without, px) > PRINTS)
            .collect();
        let body = printing.len();
        let body_fraction = body as f64 / p.body.len() as f64;
        least = least.min(body_fraction);
        assert!(
            body_fraction >= PRINTS_FRACTION,
            "{at}: {} is missing: {body} of its {} pixels print\nframe:\n{}\nwithout it:\n{}",
            p.label,
            p.body.len(),
            sketch(img, width),
            sketch(&p.without, width)
        );
        // Over the pixels that print the rock: a coarser cut's smaller silhouette does not count,
        // its shading does.
        let mean = printing
            .iter()
            .map(|&px| f64::from(max_diff(img, full, px)))
            .sum::<f64>()
            / printing.len() as f64;
        worst_mean = worst_mean.max(mean);
        assert!(
            mean <= BODY_MEAN,
            "{at}: {} prints far from its reference (mean {mean:.1} levels): a wrong material?",
            p.label
        );
        if !p.shadow.is_empty() {
            let dark = p
                .shadow
                .iter()
                .filter(|&&px| luminance(&p.shadowless, px) - luminance(img, px) > i32::from(PRINTS))
                .count();
            let dark_fraction = dark as f64 / p.shadow.len() as f64;
            least = least.min(dark_fraction);
            assert!(
                dark_fraction >= PRINTS_FRACTION,
                "{at}: {}'s shadow is missing: {dark} of its {} pixels are dark\nframe:\n{}\nshadowless:\n{}",
                p.label,
                p.shadow.len(),
                sketch(img, width),
                sketch(&p.shadowless, width)
            );
        }
    }
    (worst_mean, least)
}

/// What one run checks besides the probes: how many instances of each class the path holds.
#[derive(Clone, Copy, Debug)]
struct Expect {
    /// The path runs at all (false: the classic path draws everything).
    path: bool,
    instances: u32,
    masked: u32,
    sway: u32,
    receivers: u32,
}

/// What [`run`] measured.
struct RunReport {
    frames: usize,
    cuts: usize,
    evictions: u64,
    refinements: u64,
    peak_resident: u64,
    budget: u64,
    /// The largest mean body distance from the full reference over all frames and rocks.
    worst_body_mean: f64,
    /// The smallest share of a body or shadow footprint that printed.
    least_share: f64,
}

/// Cycles of `FRAMES_PER_VIEW` frames at each view, with a camera cut at every switch. Checks
/// every frame against `expect` and the probes; from `WARM_CYCLES` on, that a frame creates no
/// path object and the arena and path buffers keep their size.
fn run(device: &RenderDevice, budget: Option<ClusterResidency>, expect: Expect) -> RunReport {
    const CYCLES: usize = 4;
    const WARM_CYCLES: usize = 2;
    const FRAMES_PER_VIEW: usize = 6;
    let f = fixture(1);
    let settings = post();
    let (rock, meshlets) = dense_rock();
    let uv1: Vec<[f32; 2]> = rock.0.iter().map(|v| v.uv).collect();
    let materials = materials(&f);
    let map = Arc::new(lightmap());
    let new_renderer = || {
        let mut r = comparison_renderer(device);
        r.set_meshlet_builder(Some(single_level_builder()));
        for key in [ROCK_NEAR, ROCK_SWAY, ROCK_RECEIVER] {
            r.insert_meshlets(key, Arc::clone(&meshlets));
        }
        r.set_lightmap(
            Some(Arc::clone(&map)),
            crate::lightmap::LightmapSettings {
                enabled: true,
                ..crate::lightmap::LightmapSettings::default()
            },
        );
        r.set_lightmap_receivers(&[(RECEIVER_OBJECT, 0)]);
        r.set_foliage_wind(Some([3.0, 0.0]));
        r
    };
    // Reference frames, each from a fresh renderer without residency (two frames: the shadow
    // caches fill on the first).
    let reference = |view: usize, omit: Omit| {
        let mut r = new_renderer();
        let s = close_scene(&f, &settings, &rock, &uv1, &materials, view, omit);
        render(&mut r, device, &s);
        render(&mut r, device, &s)
    };
    let full: Vec<Vec<u8>> = (0..VIEWS.len())
        .map(|view| reference(view, Omit::Nothing))
        .collect();
    let prints: Vec<Vec<Footprint>> = (0..VIEWS.len())
        .map(|view| {
            let mut out = Vec::new();
            for &(object, _, material, _, z) in &ROCKS {
                if (view == 0) != (z < 100.0) {
                    continue;
                }
                let without = reference(view, Omit::Rock(object));
                let shadowless = reference(view, Omit::Shadow(object));
                let body = differing(&shadowless, &without, FOOTPRINT);
                let shadow = differing(&full[view], &shadowless, FOOTPRINT);
                let label = format!(
                    "{} rock {object}",
                    ["opaque", "masked", "sway", "receiver"][material]
                );
                assert!(
                    body.len() >= 32,
                    "view {view}: {label} covers {} pixels",
                    body.len()
                );
                eprintln!(
                    "view {view}: {label}: body {} px, shadow {} px",
                    body.len(),
                    shadow.len()
                );
                out.push(Footprint {
                    label,
                    without,
                    shadowless,
                    body,
                    shadow,
                });
            }
            assert!(
                out.iter().any(|p| p.shadow.len() >= 8),
                "view {view}: no rock casts a visible shadow"
            );
            out
        })
        .collect();
    let mut renderer = new_renderer();
    renderer.set_visbuffer_settings(VisBufferSettings {
        residency: budget,
        ..VisBufferSettings::default()
    });
    assert!(
        renderer.visbuffer_settings().resolved_lod_hysteresis_px() > 0.0,
        "the LOD band is on (ADR-4631)"
    );
    let mut frame = 0usize;
    let mut cuts = 0usize;
    let mut warm: Option<(u64, u64)> = None;
    let mut worst_body_mean = 0.0f64;
    let mut least_share = 1.0f64;
    for cycle in 0..CYCLES {
        for view in 0..VIEWS.len() {
            if frame > 0 {
                renderer.camera_cut();
                cuts += 1;
            }
            let s = close_scene(&f, &settings, &rock, &uv1, &materials, view, Omit::Nothing);
            for step in 0..FRAMES_PER_VIEW {
                let t = frame as f32 / 30.0;
                renderer.set_material_clock(t, t - 1.0 / 30.0);
                let img = render(&mut renderer, device, &s);
                let at = format!("cycle {cycle} view {view} step {step} (frame {frame})");
                assert_eq!(
                    renderer.visbuffer_active(),
                    expect.path,
                    "{at}: the path runs as the device allows"
                );
                if expect.path {
                    let stats = renderer.visbuffer_stats();
                    let got = (
                        stats.instances,
                        stats.masked_instances,
                        stats.sway_instances,
                        stats.receiver_instances,
                    );
                    assert_eq!(
                        got,
                        (expect.instances, expect.masked, expect.sway, expect.receivers),
                        "{at}: (instances, masked, sway, receivers) on the path: {stats:?}"
                    );
                    let counters = renderer
                        .visbuffer_counters(device)
                        .expect("readback")
                        .expect("a visibility frame ran");
                    assert_eq!(counters.dropped_clusters, 0, "{at}: {counters:?}");
                    assert!(
                        counters.phase1_clusters + counters.phase2_clusters > 0,
                        "{at}: {counters:?}"
                    );
                    let shadow = renderer
                        .visbuffer_shadow_counters(device)
                        .expect("shadow readback")
                        .expect("cluster shadows ran");
                    assert!(shadow[1] > 0, "{at}: cluster shadows drew nothing: {shadow:?}");
                    assert_eq!(
                        (shadow[2], shadow[3]),
                        (0, 0),
                        "{at}: shadow clusters dropped: {shadow:?}"
                    );
                    if let Some(config) = budget {
                        let r = renderer.visbuffer_arena_stats().residency;
                        assert!(r.resident_bytes <= config.budget_bytes, "{at}: {r:?}");
                        assert!(r.upload_bytes <= config.upload_bytes_per_frame, "{at}: {r:?}");
                    }
                } else {
                    assert_eq!(
                        renderer.visbuffer_create_stats().total.objects(),
                        0,
                        "{at}: the classic fallback creates no path object"
                    );
                }
                let (mean, share) = check_prints(&img, &full[view], &prints[view], s.width, &at);
                worst_body_mean = worst_body_mean.max(mean);
                least_share = least_share.min(share);
                if cycle >= WARM_CYCLES {
                    let created = renderer.visbuffer_create_stats();
                    assert_eq!(
                        created.last_frame.objects(),
                        0,
                        "{at}: a warm frame created {:?}",
                        created.last_frame
                    );
                    let arena = renderer.visbuffer_arena_stats();
                    let size = (arena.capacity_bytes, created.total.buffer_bytes);
                    assert_eq!(
                        *warm.get_or_insert(size),
                        size,
                        "{at}: (arena capacity, path buffer bytes) grew after warm-up: {arena:?}"
                    );
                }
                frame += 1;
            }
        }
    }
    let r = renderer.visbuffer_arena_stats().residency;
    eprintln!(
        "close run: {frame} frames, {cuts} cuts, least printing share {least_share:.2}, worst body mean {worst_body_mean:.1}, \
         evictions {}, refinements {}, peak {} of budget {}",
        r.evictions,
        r.refinements,
        r.peak_resident_bytes,
        budget.map_or(0, |b| b.budget_bytes)
    );
    RunReport {
        frames: frame,
        cuts,
        evictions: r.evictions,
        refinements: r.refinements,
        peak_resident: r.peak_resident_bytes,
        budget: budget.map_or(0, |b| b.budget_bytes),
        worst_body_mean,
        least_share,
    }
}

/// A budget that holds the cooked bases and about one mesh's fine bands at the near view, so the
/// two groups' three meshes cannot all be fine at once: the cuts evict and reload.
fn tight_budget(device: &RenderDevice) -> ClusterResidency {
    let f = fixture(1);
    let settings = post();
    let (rock, meshlets) = dense_rock();
    let uv1: Vec<[f32; 2]> = rock.0.iter().map(|v| v.uv).collect();
    let materials = materials(&f);
    let mut probe = comparison_renderer(device);
    probe.set_meshlet_builder(Some(single_level_builder()));
    for key in [ROCK_NEAR, ROCK_SWAY, ROCK_RECEIVER] {
        probe.insert_meshlets(key, Arc::clone(&meshlets));
    }
    probe.set_visbuffer_settings(VisBufferSettings {
        residency: Some(ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: u64::MAX / 4,
            ..ClusterResidency::for_tier(crate::memory_budget::BudgetTier::Deck)
        }),
        ..VisBufferSettings::default()
    });
    // Far from both groups: only the bases.
    let mut away = close_scene(&f, &settings, &rock, &uv1, &materials, 0, Omit::Nothing);
    away.eye = [0.0, 1.6, 150.0];
    away.target = [0.0, 1.6, 140.0];
    render(&mut probe, device, &away);
    let base = probe.visbuffer_arena_stats().residency.resident_bytes;
    let near = close_scene(&f, &settings, &rock, &uv1, &materials, 0, Omit::Nothing);
    for _ in 0..8 {
        render(&mut probe, device, &near);
    }
    let settled = probe.visbuffer_arena_stats().residency;
    assert_eq!(settled.pending_bands, 0, "{settled:?}");
    let extra = settled.resident_bytes - base;
    assert!(extra > 0, "the near view loads finer bands: {settled:?}");
    ClusterResidency {
        enabled: true,
        budget_bytes: base + extra,
        upload_bytes_per_frame: 64 << 10,
        ..ClusterResidency::for_tier(crate::memory_budget::BudgetTier::Deck)
    }
}

/// The acceptance run on a device with the texture table: masked and swaying rocks on the path,
/// under residency pressure, through camera cuts. The receiver's mesh is residency-managed, so it
/// draws forward with its lightmap by design (its UV2 cannot follow non-contiguous vertices,
/// ADR-3118); its probe must still print.
#[test]
fn residency_pressure_and_camera_cuts_keep_every_receiver_shadow_and_material() {
    let Some((device, _)) = crate::texture_table::frame_tests::pair() else {
        return;
    };
    let budget = tight_budget(&device);
    let report = run(
        &device,
        Some(budget),
        Expect {
            path: true,
            instances: 5,
            masked: 1,
            sway: 1,
            receivers: 0,
        },
    );
    eprintln!(
        "close: {} frames, {} cuts, evictions {}, refinements {}, peak {} of budget {}, worst body mean {:.1}, least share {:.2}",
        report.frames,
        report.cuts,
        report.evictions,
        report.refinements,
        report.peak_resident,
        report.budget,
        report.worst_body_mean,
        report.least_share
    );
    assert_eq!(report.cuts, 7);
    assert!(
        report.evictions > 0 && report.refinements > 0,
        "the budget was under pressure: evictions {}, refinements {}",
        report.evictions,
        report.refinements
    );
    assert!(report.peak_resident <= report.budget);
}

/// Without residency the receiver takes the path too: every kind of instance on it at once, with
/// the LOD band and the camera cuts.
#[test]
fn without_residency_every_kind_of_instance_takes_the_path_through_the_cuts() {
    let Some((device, _)) = crate::texture_table::frame_tests::pair() else {
        return;
    };
    run(
        &device,
        None,
        Expect {
            path: true,
            instances: 6,
            masked: 1,
            sway: 1,
            receivers: 1,
        },
    );
}

/// The texture-table fallback (A15): on the twin device without the table's features the masked
/// rock leaves the path for the forward passes, and every rock and shadow still prints.
#[test]
fn without_the_texture_table_the_masked_rock_draws_forward_and_nothing_is_missing() {
    let Some((_, classic)) = crate::texture_table::frame_tests::pair() else {
        return;
    };
    let budget = tight_budget(&classic);
    run(
        &classic,
        Some(budget),
        Expect {
            path: true,
            instances: 4,
            masked: 0,
            sway: 1,
            receivers: 0,
        },
    );
}

/// `base`'s adapter, features and limits with at most `storage` storage buffers per shader stage
/// (the caps of Windows and Android adapters below the path's floors), or `None` (skip).
fn limited_device(base: &RenderDevice, storage: u32) -> Option<RenderDevice> {
    let mut limits = base.device.limits();
    limits.max_storage_buffers_per_shader_stage = limits.max_storage_buffers_per_shader_stage.min(storage);
    let (device, queue) = regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("visbuffer close limited caps"),
        required_limits: limits,
        required_features: base.device.features(),
        memory_hints: crate::device::device_memory_hints(&base.adapter.get_info()),
        ..Default::default()
    }))
    .ok()?;
    let capabilities = crate::RenderCapabilities::from_adapter(&base.adapter, &device);
    Some(RenderDevice::from_parts(
        base.instance.clone(),
        base.adapter.clone(),
        device,
        queue,
        capabilities,
    ))
}

/// The lightmap fallback: at exactly the path's storage-buffer floor (with and without the texture
/// table's features) the path runs but the receiver variant does not fit, so the receiver draws
/// forward with its lightmap and every rock and shadow still prints. Before ADR-4906 the floor was
/// one buffer short of the resolve's layout and this frame failed validation.
#[test]
fn at_the_storage_buffer_floor_the_receiver_draws_forward_and_nothing_is_missing() {
    let mut bases = vec![crate::test_device()];
    if let Some((table, _)) = crate::texture_table::frame_tests::pair() {
        bases.push(table);
    }
    for base in &bases {
        let Some(device) = limited_device(base, super::super::gpu::RESOLVE_STORAGE_BUFFERS) else {
            eprintln!("skipping: the adapter would not open a device with the lower limit");
            continue;
        };
        assert!(super::super::VisBufferPass::supported(&device.device));
        assert!(!super::super::VisBufferPass::lightmap_supported(&device.device));
        let masked = u32::from(super::super::VisBufferPass::masked_supported(&device.device));
        run(
            &device,
            None,
            Expect {
                path: true,
                instances: 4 + masked,
                masked,
                sway: 1,
                receivers: 0,
            },
        );
    }
}

/// The device-caps fallback (adapters below the path's storage-buffer floor): the whole frame
/// stays classic, creates no path object, and every rock and shadow prints.
#[test]
fn below_the_storage_buffer_floor_the_classic_path_draws_everything() {
    let base = crate::test_device();
    let Some(low) = limited_device(&base, super::super::gpu::RESOLVE_STORAGE_BUFFERS - 1) else {
        eprintln!("skipping: the adapter would not open a device with the lower limit");
        return;
    };
    assert!(!super::super::VisBufferPass::supported(&low.device));
    run(
        &low,
        None,
        Expect {
            path: false,
            instances: 0,
            masked: 0,
            sway: 0,
            receivers: 0,
        },
    );
}
