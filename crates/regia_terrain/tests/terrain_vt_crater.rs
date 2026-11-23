// SPDX-License-Identifier: MPL-2.0
//! A height edit under the terrain virtual texture re-bakes only the pages it reaches, in place
//! (DO-4 leftover, ADR-2301): no flush, the pages far from the edit keep their texels, the
//! pages over it change, and the converged frame matches a cold bake of the edited map.
//!
//! The edit goes through the runtime height-patch queue a host uses (`height_patches_handle`,
//! `request` + `take_frame`), so the path is the one craters and tunnels take in a game:
//! band -> `TerrainSharedDraw::apply_height_patches` -> `TerrainGpu::update_heights` -> the
//! queued rectangle -> `VtPageManager::invalidate_rect` on the next prepare.
//!
//! The converged frame and every page both runs hold equal the cold bake's bit for bit
//! (ADR-2450). Two things need room for that, neither of them the terrain's in-place state:
//! - the virtual texture records feedback from one pixel in each 4×4 block per frame, rotating,
//!   so a page the edited view first needs is requested within one rotation (16 frames) plus
//!   the readback latency: `AFTER` covers it;
//! - the renderer's temporal GI is off: the irradiance probes' hysteresis and the screen-space
//!   GI's held gather carry the frames before the edit by design, which a cold load never saw.
//!
//! Runs on lavapipe and on a real card (`REGIA_GPU_UUID`); it skips (prints `SKIP`) on a device
//! without `SHADER_EARLY_DEPTH_TEST`, where the virtual texture stays off.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::gpu::vt::{HEIGHT_REACH_M, VtLayout};
use regia_terrain::{
    CdlodConfig, HeightBand, HeightPatch, LayerDesc, LayerTextures, MaterialParams, Rgba8Image,
    TerrainLayers, TerrainMap, TerrainMaterial, TerrainSharedDraw, TerrainVtHandle, VirtualTextureSettings,
    VtStats,
};

const W: u32 = 160;
const H: u32 = 90;
const EYE: [f32; 3] = [10.0, 15.0, -60.0];
/// 4 km, 4 m samples: big enough that the far edit's 450 m reach misses the pages near the eye.
const N: u32 = 1025;
const WORLD_M: f32 = 4096.0;
/// The crater: centre (x, z), radius and depth in metres, straight ahead of the eye.
const CRATER: ([f32; 2], f32, f32) = ([0.0, 700.0], 80.0, 20.0);
/// Frames before the edit (the cache converges) and after it (one feedback rotation of 16
/// frames, the readback latency and slack).
const SETTLE: usize = 10;
const AFTER: usize = 24;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

fn supported() -> bool {
    let ok = device()
        .device
        .features()
        .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST);
    if !ok {
        eprintln!("SKIP: the device lacks SHADER_EARLY_DEPTH_TEST (the virtual texture stays off)");
    }
    ok
}

fn world_of(sample: u32) -> f32 {
    sample as f32 / (N - 1) as f32 * WORLD_M - WORLD_M * 0.5
}

fn sample_of(world: f32) -> u32 {
    (((world + WORLD_M * 0.5) / WORLD_M * (N - 1) as f32).round() as u32).min(N - 1)
}

/// Gentle rolling ground (no ridge hides the crater from the eye).
fn map() -> TerrainMap {
    let mut heights = Vec::with_capacity((N * N) as usize);
    for row in 0..N {
        for col in 0..N {
            let (x, z) = (world_of(col), world_of(row));
            heights.push(1.5 * (x / 53.0).sin() * (z / 71.0).cos() + 0.7 * (x / 17.0 + z / 23.0).sin());
        }
    }
    TerrainMap::from_heights(N, N, heights, WORLD_M, 40.0, -20.0).expect("map")
}

/// The crater as a height patch over `map`: a bowl `depth` deep, flat outside `radius`.
fn crater_patch(map: &TerrainMap) -> HeightPatch {
    let ([cx, cz], radius, depth) = CRATER;
    let (c0, c1) = (sample_of(cx - radius - 4.0), sample_of(cx + radius + 4.0));
    let (r0, r1) = (sample_of(cz - radius - 4.0), sample_of(cz + radius + 4.0));
    let (width, height) = (c1 - c0 + 1, r1 - r0 + 1);
    let mut heights = Vec::with_capacity((width * height) as usize);
    for row in r0..=r1 {
        for col in c0..=c1 {
            let base = map.heights()[(row * N + col) as usize];
            let d = ((world_of(col) - cx).powi(2) + (world_of(row) - cz).powi(2)).sqrt() / radius;
            let dig = if d < 1.0 { depth * (1.0 - d * d) } else { 0.0 };
            heights.push(base - dig);
        }
    }
    HeightPatch {
        col: c0,
        row: r0,
        width,
        height,
        heights,
    }
}

fn settings() -> VirtualTextureSettings {
    VirtualTextureSettings {
        // 3 m pages: 4 km needs 1366 a side, inside the 2048 limit.
        lod0_texel_m: 0.1,
        page_texels: 30,
        atlas_slots: 24,
        near_m: 0.0,
        bakes_per_frame: 600,
        // No distance re-bakes: every re-bake after the edit is the edit's.
        rebakes_per_frame: 0,
        stochastic_lod: false,
        ..VirtualTextureSettings::default()
    }
}

fn draw(map: TerrainMap) -> TerrainSharedDraw {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [0.5, 0.45, 0.4],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
    // The virtual texture serves the photoreal material path only (`material.params0.x`): without a
    // material the draw shades the splat path, records no feedback and no page ever becomes resident.
    let v = regia_terrain::linear_to_srgb8(0.4);
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw.set_virtual_texture(Some(settings()))
        .expect("virtual texture");
    draw
}

/// A page key `(lod, x, y)` and its albedo texels.
type PageTexels = ((u32, u32, u32), Vec<[f32; 4]>);

/// What a run saw.
struct Run {
    /// The last frame.
    pixels: Vec<u8>,
    /// Per frame: the virtual texture's counters after it.
    stats: Vec<VtStats>,
    /// The bands the edit wrote (empty without an edit).
    bands: Vec<HeightBand>,
    /// Pages resident just before the edit, with their texels (map 0, the albedo).
    before: Vec<PageTexels>,
    /// The same pages after the last frame (`None` when evicted).
    after: Vec<Option<Vec<[f32; 4]>>>,
    /// Every page resident after the last frame, with its texels.
    last: Vec<PageTexels>,
    layout: Option<VtLayout>,
}

/// `frames` frames of a held view over `map`; with `edit`, the crater is queued before frame
/// `SETTLE` (so the draw applies it in that frame's prepare).
fn run(map: TerrainMap, frames: usize, edit: bool) -> Run {
    let draw = draw(map.clone());
    let handle: TerrainVtHandle = draw.vt_handle();
    let patches = draw.height_patches_handle();
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, 200.0],
        fov_y: 0.9,
        near: 0.5,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.terrain_budget.anisotropy = 0;
    // Temporal GI keeps the frames before the edit (module docs); the terrain's state is compared.
    quality.gi_probes = false;
    quality.screen_space_gi = false;
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let mut out = Run {
        pixels: Vec::new(),
        stats: Vec::new(),
        bands: Vec::new(),
        before: Vec::new(),
        after: Vec::new(),
        last: Vec::new(),
        layout: None,
    };
    for i in 0..frames {
        if edit && i == SETTLE {
            for page in handle.resident_pages() {
                if let Ok(maps) = handle.read_page(page.0, page.1, page.2) {
                    let [albedo, ..] = maps;
                    out.before.push((page, albedo));
                }
            }
            patches.request(crater_patch(&map)).expect("crater patch");
            patches.set_budget(1 << 20);
            out.bands = patches.take_frame();
            assert!(!out.bands.is_empty(), "the queue wrote no band");
        }
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        out.pixels = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
        out.stats.push(handle.stats().unwrap_or_default());
    }
    let mut keys = handle.resident_pages();
    keys.sort_unstable();
    out.last = keys
        .iter()
        .filter_map(|&(lod, x, y)| {
            handle
                .read_page(lod, x, y)
                .ok()
                .map(|[albedo, ..]| ((lod, x, y), albedo))
        })
        .collect();
    let resident: std::collections::HashSet<(u32, u32, u32)> = keys.into_iter().collect();
    out.after = out
        .before
        .iter()
        .map(|(page, _)| {
            resident
                .contains(page)
                .then(|| handle.read_page(page.0, page.1, page.2).ok())
                .flatten()
                .map(|[albedo, ..]| albedo)
        })
        .collect();
    out.layout = handle.layout();
    out
}

/// Whether page `(lod, x, y)` meets the crater's rectangle grown by `grow` metres.
fn meets(layout: &VtLayout, (lod, x, y): (u32, u32, u32), grow: f32) -> bool {
    let ([cx, cz], radius, _) = CRATER;
    let r = radius + 4.0 + grow;
    let (x0, z0, size) = layout.page_rect(lod, x, y);
    x0 <= cx + r && x0 + size >= cx - r && z0 <= cz + r && z0 + size >= cz - r
}

#[test]
fn a_crater_under_the_virtual_texture_rebakes_only_the_pages_it_reaches() {
    if !supported() {
        return;
    }
    let edited = run(map(), SETTLE + AFTER, true);
    let layout = edited.layout.expect("the virtual texture ran");
    let resident_before = edited.before.len();
    assert!(
        resident_before > 20,
        "only {resident_before} pages resident before the edit"
    );

    // No flush after the cache settled; the edit only re-bakes.
    let flushed: Vec<usize> = (3..edited.stats.len())
        .filter(|&i| edited.stats[i].flushed)
        .collect();
    assert!(flushed.is_empty(), "the cache flushed at frames {flushed:?}");
    let rebaked_after: u32 = edited.stats[SETTLE..].iter().map(|s| s.rebaked).sum();
    let rebaked_before: u32 = edited.stats[3..SETTLE].iter().map(|s| s.rebaked).sum();
    assert_eq!(
        rebaked_before, 0,
        "re-bakes before the edit (distance re-bakes are off)"
    );
    let reached = edited
        .before
        .iter()
        .filter(|(page, _)| meets(&layout, *page, HEIGHT_REACH_M + 8.0))
        .count();
    println!(
        "resident {resident_before}, reached by the edit {reached}, re-baked {rebaked_after}, stats after: {:?}",
        edited.stats.last()
    );
    assert!(rebaked_after >= 1, "nothing re-baked after the edit");
    assert!(
        (rebaked_after as usize) <= reached && reached < resident_before,
        "re-baked {rebaked_after}, reached {reached}, resident {resident_before}: the edit must reach only part"
    );

    // Far pages keep their texels bit for bit; the finest page over the crater changed.
    let mut far_same = 0usize;
    let mut near_changed = 0usize;
    for ((page, before), after) in edited.before.iter().zip(&edited.after) {
        let Some(after) = after else { continue };
        let far = !meets(&layout, *page, HEIGHT_REACH_M + 12.0);
        let over = meets(&layout, *page, 0.0);
        let same = before == after;
        if far {
            assert!(
                same,
                "page {page:?} is beyond the edit's reach but its texels changed"
            );
            far_same += 1;
        } else if over && !same {
            near_changed += 1;
        }
    }
    println!("far pages kept: {far_same}; pages over the crater that changed: {near_changed}");
    assert!(far_same > 0, "no far page to check");

    // The converged frame matches a cold bake of the edited map.
    let mut cold_map = map();
    for band in &edited.bands {
        band.apply(&mut cold_map).expect("band inside the map");
    }
    let cold = run(cold_map, SETTLE + AFTER, false);
    let untouched = run(map(), SETTLE + AFTER, false);
    let diff = |a: &[u8], b: &[u8]| {
        a.iter()
            .zip(b)
            .map(|(x, y)| u32::from(x.abs_diff(*y)))
            .fold((0u32, 0usize), |(m, n), d| (m.max(d), n + usize::from(d > 0)))
    };
    let (crater_max, crater_px) = diff(&untouched.pixels, &cold.pixels);
    let (max, px) = diff(&edited.pixels, &cold.pixels);
    println!(
        "crater vs untouched: max {crater_max}, {crater_px} channels differ; in-place vs cold bake: max {max}, {px} channels differ"
    );
    assert!(
        crater_px > 0,
        "the crater is not visible: the frame comparison would prove nothing"
    );
    // Every page both runs hold, the re-baked crater pages included, is the cold bake's.
    let mut common = 0;
    let mut over_crater = 0;
    let mut differ = Vec::new();
    for (key, texels) in &edited.last {
        if let Some((_, cold_texels)) = cold.last.iter().find(|(k, _)| k == key) {
            common += 1;
            over_crater += usize::from(meets(&layout, *key, 0.0));
            if texels != cold_texels {
                differ.push(*key);
            }
        }
    }
    println!("pages in both runs {common} ({over_crater} over the crater); differing {differ:?}");
    assert!(over_crater > 0, "no page over the crater to compare");
    assert!(
        differ.is_empty(),
        "in-place re-baked pages differ from the cold bake: {differ:?}"
    );
    assert!(
        max == 0 && px == 0,
        "the in-place re-bake differs from a cold bake: max {max}, {px} channels (crater moved {crater_px})"
    );
}
