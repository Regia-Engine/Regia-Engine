// SPDX-License-Identifier: MPL-2.0
//! An in-place height edit leaves the frame a cold load of the edited map gives (ADR-2450).
//!
//! Every state a load derives from the heights must follow an edit through the runtime
//! height-patch queue (`height_patches_handle`): the height texture and its pyramid, the slope
//! pyramid, the draw's own map and min/max pyramid, the map's altitude range, and the virtual
//! texture's pages. Where that is cheap the contract is exact:
//!
//! - a patch that writes the heights the map already holds, over the whole map, is a frame
//!   no-op: no pixel moves, no page re-bakes, and nothing keyed on the heights' generation
//!   (shadow cascades, the probes' heightfield, the held screen-space gathers) is disturbed,
//!   with every renderer pass on;
//! - a whole-map patch that changes only a pit redraws only what the pit reaches: the same
//!   re-bakes, frame and pages as a patch of the pit alone;
//! - a patch that lowers the map's floor past a layer's altitude band moves the altitude range
//!   the shader tests the band against: the frame and the pages match a cold load of the
//!   edited map, far from the edit too.
//!
//! The renderer's temporal GI is off where an edited frame is compared with a cold load: the
//! irradiance probes' hysteresis and the screen-space GI's held gather carry the frames before
//! the edit by design (renderer state, not the terrain's), and a cold load has no such history.
//! The virtual texture needs one feedback rotation (16 frames) plus the readback latency after
//! an edit to request every page the edited view needs: `AFTER` covers it.
//!
//! Where the edit drops the virtual texture's pages, the re-baked pages land in other atlas slots
//! than a cold load's. They are bit-identical, but a real GPU's sampler (8-bit subtexel weights)
//! can filter a page 1 LSB differently at another slot. So the frame is compared bit for bit
//! with the terrain shaded in full, and with the virtual texture it may differ from the cold load
//! by no more than the same cold load does with only its atlas resized (the device's placement
//! floor; 0 on lavapipe, which keeps that comparison exact there).
//!
//! The crater against a cold bake with the virtual texture is `terrain_vt_crater`.
//!
//! Runs on lavapipe and on a real card (`REGIA_GPU_UUID`).

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, HeightPatch, LayerDesc, LayerTextures, MaterialParams, PatchTarget, Rgba8Image,
    TerrainLayers, TerrainMap, TerrainMaterial, TerrainSharedDraw, TerrainVtHandle, VirtualTextureSettings,
    VtStats,
};

const W: u32 = 160;
const H: u32 = 90;
const EYE: [f32; 3] = [10.0, 15.0, -60.0];
/// 2 km at 4 m: wide enough that a far edit's 450 m reach misses the pages near the eye, small
/// enough to patch whole in one frame.
const N: u32 = 513;
const WORLD_M: f32 = 2048.0;
/// Frames before the edit and after it (one feedback rotation of 16 frames, the readback
/// latency and slack).
const SETTLE: usize = 8;
const AFTER: usize = 24;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// The virtual texture needs early depth tests; without them only the frames are compared.
fn vt_supported() -> bool {
    device()
        .device
        .features()
        .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST)
}

fn world_of(sample: u32) -> f32 {
    sample as f32 / (N - 1) as f32 * WORLD_M - WORLD_M * 0.5
}

/// Gentle rolling ground between about -2.2 and 2.2 m, on the 16-bit sample grid (so a patch
/// of the same heights writes the same bits).
fn map() -> TerrainMap {
    let raw = |col: u32, row: u32| {
        let (x, z) = (world_of(col), world_of(row));
        1.5 * (x / 53.0).sin() * (z / 71.0).cos() + 0.7 * (x / 17.0 + z / 23.0).sin()
    };
    let probe = TerrainMap::from_heights(2, 2, vec![0.0; 4], WORLD_M, 40.0, -20.0).expect("map");
    let target = PatchTarget::of(&probe);
    let heights = (0..N)
        .flat_map(|row| (0..N).map(move |col| (col, row)))
        .map(|(col, row)| target.dequantize(target.quantize(raw(col, row))))
        .collect();
    TerrainMap::from_heights(N, N, heights, WORLD_M, 40.0, -20.0).expect("map")
}

/// A pit `depth` metres deep and 24 m across at `(x, z)`, as a patch over `map`.
fn pit(map: &TerrainMap, [x, z]: [f32; 2], depth: f32) -> HeightPatch {
    let sample = |w: f32| (((w + WORLD_M * 0.5) / WORLD_M * (N - 1) as f32).round() as u32).min(N - 1);
    let (c0, c1, r0, r1) = (
        sample(x - 16.0),
        sample(x + 16.0),
        sample(z - 16.0),
        sample(z + 16.0),
    );
    let mut heights = Vec::new();
    for row in r0..=r1 {
        for col in c0..=c1 {
            let d = ((world_of(col) - x).powi(2) + (world_of(row) - z).powi(2)).sqrt() / 12.0;
            let dig = if d < 1.0 { depth * (1.0 - d * d) } else { 0.0 };
            heights.push(map.heights()[(row * N + col) as usize] - dig);
        }
    }
    HeightPatch {
        col: c0,
        row: r0,
        width: c1 - c0 + 1,
        height: r1 - r0 + 1,
        heights,
    }
}

/// The whole map as a patch of the heights it holds.
fn same_heights(map: &TerrainMap) -> HeightPatch {
    HeightPatch {
        col: 0,
        row: 0,
        width: N,
        height: N,
        heights: map.heights().to_vec(),
    }
}

/// Two layers: `ground` everything, `low` the ground from `low_floor` up to -1 m with a soft
/// 0.5 m edge; contrasting tints so a weight change shows.
fn stack(low_floor: f32) -> TerrainLayers {
    TerrainLayers {
        layers: vec![
            LayerDesc {
                name: "ground".into(),
                albedo: [0.5, 0.45, 0.4],
                roughness: 1.0,
                slope_min: 0.0,
                slope_max: 1.6,
                altitude_min: -100.0,
                altitude_max: 100.0,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "low".into(),
                albedo: [0.15, 0.3, 0.6],
                roughness: 1.0,
                slope_min: 0.0,
                slope_max: 1.6,
                altitude_min: low_floor,
                altitude_max: -1.0,
                blend_sharpness: 0.5,
                ..LayerDesc::ground()
            },
        ],
        ..Default::default()
    }
}

/// Atlas slots per side of the runs' virtual texture (24 × 32 texels: 768² texels).
const ATLAS_SLOTS: u32 = 24;

/// The draw, with the virtual texture on in an atlas of `vt_slots` slots per side.
fn draw(map: TerrainMap, layers: &TerrainLayers, vt_slots: Option<u32>) -> TerrainSharedDraw {
    let mut draw = TerrainSharedDraw::new(device(), map, layers, CdlodConfig::default(), 3).expect("terrain");
    let v = regia_terrain::linear_to_srgb8(0.4);
    let set = || {
        Some(LayerTextures {
            albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
            normal: Rgba8Image::neutral_normal(64),
            height: Rgba8Image::neutral_height(64),
            orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
            use_orm: false,
        })
    };
    draw.set_material(
        &TerrainMaterial {
            layers: layers.layers.iter().map(|_| set()).collect(),
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    if let Some(atlas_slots) = vt_slots {
        draw.set_virtual_texture(Some(VirtualTextureSettings {
            lod0_texel_m: 0.1,
            page_texels: 30,
            atlas_slots,
            near_m: 0.0,
            bakes_per_frame: 600,
            // No distance re-bakes: every re-bake after the edit is the edit's.
            rebakes_per_frame: 0,
            stochastic_lod: false,
            ..VirtualTextureSettings::default()
        }))
        .expect("virtual texture");
    }
    draw
}

/// A page key `(lod, x, y)` and its texels (map 0, the albedo).
type PageTexels = ((u32, u32, u32), Vec<[f32; 4]>);

/// What a run saw.
struct Run {
    pixels: Vec<u8>,
    stats: Vec<VtStats>,
    /// The resident pages after the last frame with their texels (sorted).
    pages: Vec<PageTexels>,
}

fn pages_of(handle: &TerrainVtHandle) -> Vec<PageTexels> {
    let mut keys = handle.resident_pages();
    keys.sort_unstable();
    keys.into_iter()
        .filter_map(|page| {
            handle
                .read_page(page.0, page.1, page.2)
                .ok()
                .map(|[albedo, ..]| (page, albedo))
        })
        .collect()
}

/// `SETTLE + AFTER` frames of a held view; `edit` is queued before frame `SETTLE`. `temporal_gi`
/// keeps the irradiance probes and the screen-space GI on (Desktop High has both).
fn run(
    map: TerrainMap,
    layers: &TerrainLayers,
    vt: bool,
    temporal_gi: bool,
    edit: Option<HeightPatch>,
) -> Run {
    run_with(map, layers, vt.then_some(ATLAS_SLOTS), temporal_gi, edit)
}

/// [`run`] with the virtual texture's atlas at `vt_slots` slots per side (`None` = off).
fn run_with(
    map: TerrainMap,
    layers: &TerrainLayers,
    vt_slots: Option<u32>,
    temporal_gi: bool,
    edit: Option<HeightPatch>,
) -> Run {
    let draw = draw(map, layers, vt_slots);
    let handle = draw.vt_handle();
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
    if !temporal_gi {
        quality.gi_probes = false;
        quality.screen_space_gi = false;
    }
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let mut out = Run {
        pixels: Vec::new(),
        stats: Vec::new(),
        pages: Vec::new(),
    };
    let mut edit = edit;
    for i in 0..SETTLE + AFTER {
        if i == SETTLE
            && let Some(patch) = edit.take()
        {
            patches.request(patch).expect("patch");
            patches.set_budget(1 << 20);
            assert!(!patches.take_frame().is_empty(), "the queue wrote no band");
            assert!(patches.is_idle(), "the patch did not fit one frame");
        }
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        out.pixels = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
        out.stats.push(handle.stats().unwrap_or_default());
    }
    out.pages = pages_of(&handle);
    out
}

/// Largest channel difference and the number of channels that differ.
fn diff(a: &[u8], b: &[u8]) -> (u32, usize) {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| u32::from(x.abs_diff(*y)))
        .fold((0, 0), |(m, n), d| (m.max(d), n + usize::from(d > 0)))
}

/// The pages two runs hold in common that differ, and how many they hold in common.
fn page_diff(a: &[PageTexels], b: &[PageTexels]) -> (Vec<(u32, u32, u32)>, usize) {
    let mut common = 0;
    let mut differ = Vec::new();
    for (key, texels) in a {
        if let Some((_, other)) = b.iter().find(|(k, _)| k == key) {
            common += 1;
            if texels != other {
                differ.push(*key);
            }
        }
    }
    (differ, common)
}

#[test]
fn a_whole_map_patch_of_the_same_heights_is_a_frame_no_op() {
    let vt = vt_supported();
    let layers = stack(map().min_max_height().0 - 0.02);
    // Every pass on, the temporal GI included: a no-op must not even restart its history.
    let untouched = run(map(), &layers, vt, true, None);
    let patched = run(map(), &layers, vt, true, Some(same_heights(&map())));
    let (max, channels) = diff(&untouched.pixels, &patched.pixels);
    println!("same-heights patch vs untouched: max {max}, {channels} channels differ");
    if vt {
        let rebaked: u32 = patched.stats[SETTLE..].iter().map(|s| s.rebaked).sum();
        let flushed = patched.stats[SETTLE..].iter().any(|s| s.flushed);
        let (differ, common) = page_diff(&untouched.pages, &patched.pages);
        println!("re-baked {rebaked}, flushed {flushed}, pages in common {common}, differing {differ:?}");
        assert!(!flushed, "a no-op edit dropped the virtual texture");
        assert_eq!(rebaked, 0, "a no-op edit re-baked pages");
        assert!(common > 20, "only {common} pages to compare");
        assert!(differ.is_empty(), "pages changed texels: {differ:?}");
    }
    assert_eq!(
        channels, 0,
        "a patch of the same heights moved {channels} channels (max {max})"
    );
}

#[test]
fn a_whole_map_patch_that_digs_one_pit_redraws_only_what_the_pit_reaches() {
    if !vt_supported() {
        eprintln!("SKIP: the device lacks SHADER_EARLY_DEPTH_TEST (the virtual texture stays off)");
        return;
    }
    // Bands far below the floor: the pit flips no band's reach (that would drop every page).
    let layers = stack(-50.0);
    let base = map();
    // The whole map, with a 6 m pit dug 760 m ahead of the eye: only the pit's samples change.
    let pit = pit(&base, [0.0, 700.0], 6.0);
    let mut whole = same_heights(&base);
    for r in 0..pit.height {
        for c in 0..pit.width {
            whole.heights[((pit.row + r) * N + pit.col + c) as usize] =
                pit.heights[(r * pit.width + c) as usize];
        }
    }
    let whole_run = run(base.clone(), &layers, true, false, Some(whole));
    let pit_run = run(base, &layers, true, false, Some(pit));
    let rebaked = |r: &Run| r.stats[SETTLE..].iter().map(|s| s.rebaked).sum::<u32>();
    let resident = pit_run.stats[SETTLE - 1].resident;
    println!(
        "resident {resident}; re-baked: whole-map patch {}, pit patch {}",
        rebaked(&whole_run),
        rebaked(&pit_run)
    );
    assert!(rebaked(&pit_run) > 0, "the pit re-baked no page");
    assert!(
        rebaked(&pit_run) < resident,
        "the pit re-baked every page: the comparison proves nothing"
    );
    // Narrowed to the changed samples, the whole-map patch is the pit: the same re-bakes, and
    // the same frame and pages bit for bit.
    assert_eq!(
        rebaked(&whole_run),
        rebaked(&pit_run),
        "the whole-map patch re-baked more than the pit reaches"
    );
    let (max, channels) = diff(&whole_run.pixels, &pit_run.pixels);
    let (differ, _) = page_diff(&whole_run.pages, &pit_run.pages);
    assert_eq!(
        channels, 0,
        "whole-map patch vs pit patch: {channels} channels (max {max})"
    );
    assert!(differ.is_empty(), "pages differ: {differ:?}");
}

#[test]
fn a_pit_below_a_bands_floor_matches_a_cold_load_of_the_edited_map() {
    let vt = vt_supported();
    // The low band starts 2 cm under the loaded map's floor (about -2.2 m), so it reaches it:
    // full weight on the lowest ground. A 10 m pit far from the eye lowers the floor past the
    // band: a cold load of the pitted map fades the band in over its soft edge everywhere,
    // not only in the pit.
    let base = map();
    let floor = base.min_max_height().0 - 0.02;
    let layers = stack(floor);
    let edit = pit(&base, [200.0, 400.0], 10.0);
    let mut cold_map = base.clone();
    let mut heights = cold_map.heights().to_vec();
    let target = PatchTarget::of(&base);
    for r in 0..edit.height {
        for c in 0..edit.width {
            let h = edit.heights[(r * edit.width + c) as usize];
            heights[((edit.row + r) * N + edit.col + c) as usize] = target.dequantize(target.quantize(h));
        }
    }
    cold_map = TerrainMap::from_heights(N, N, heights, WORLD_M, 40.0, -20.0).expect("map");
    assert!(
        cold_map.min_max_height().0 < floor - 1.0,
        "the pit must lower the floor past the band"
    );

    // Shading in full (no virtual texture): every state the shader reads besides the pages (the
    // altitude range above all) must equal a cold load bit for bit, on any device.
    let full_in_place = run(base.clone(), &layers, false, false, Some(edit.clone()));
    let full_cold = run(cold_map.clone(), &layers, false, false, None);
    let (full_max, full_channels) = diff(&full_in_place.pixels, &full_cold.pixels);
    println!("shaded in full, in-place vs cold: max {full_max}, {full_channels} channels");
    assert_eq!(
        full_channels, 0,
        "shaded in full, in-place differs from a cold load of the edited map: {full_channels} channels (max {full_max})"
    );

    let untouched = run(base.clone(), &layers, vt, false, None);
    let in_place = run(base, &layers, vt, false, Some(edit));
    let cold = run(cold_map.clone(), &layers, vt, false, None);
    let (moved_max, moved) = diff(&untouched.pixels, &cold.pixels);
    let (max, channels) = diff(&in_place.pixels, &cold.pixels);
    println!(
        "cold pitted vs untouched: max {moved_max}, {moved} channels; in-place vs cold: max {max}, {channels} channels"
    );
    // The floor moving re-weights the band across the view: the comparison means something. Shaded
    // in full (no virtual texture: a device without forced early depth, Metal or DX12) the change
    // moves fewer channels: 160 on the Apple M5 and on an RTX 5080 withholding the feature, against
    // more than 200 through the pages (ADR-3373).
    let meaningful = if vt { 200 } else { 100 };
    assert!(
        moved > meaningful,
        "the floor change moved only {moved} channels (virtual texture {vt})"
    );
    if !vt {
        assert_eq!(
            channels, 0,
            "in-place differs from a cold load of the edited map: {channels} channels (max {max})"
        );
        return;
    }
    let flushed = in_place.stats[SETTLE..].iter().any(|s| s.flushed);
    let (differ, common) = page_diff(&in_place.pages, &cold.pages);
    println!("in-place flushed {flushed}; pages in common {common}, differing {differ:?}");
    assert!(
        flushed,
        "the band stopped reaching the floor but the pages stayed"
    );
    let keys = |r: &Run| r.pages.iter().map(|p| p.0).collect::<Vec<_>>();
    assert_eq!(
        keys(&in_place),
        keys(&cold),
        "the resident pages differ from a cold load"
    );
    assert!(common > 20, "only {common} pages to compare");
    assert!(differ.is_empty(), "pages differ from a cold bake: {differ:?}");
    // With the same pages, the frame can still depend on where the atlas holds them: the flush
    // re-bakes every page in one frame, a cold load bakes them as its feedback names them, so
    // they land in other slots, and a sampler with 8-bit subtexel weights (real GPUs) can round
    // a page's filter weights differently at another slot. The same cold load with only its
    // atlas resized moves every page: that difference is the device's placement floor (0 on a
    // device whose sampling does not depend on the slot, such as lavapipe: then the frames must be
    // bit-identical). Nothing the terrain derives from the heights may add to it.
    let resized = run_with(cold_map, &layers, Some(ATLAS_SLOTS + 1), false, None);
    let (floor_max, floor_channels) = diff(&cold.pixels, &resized.pixels);
    println!(
        "placement floor (cold, atlas {ATLAS_SLOTS} vs {} slots): max {floor_max}, {floor_channels} channels",
        ATLAS_SLOTS + 1
    );
    assert_eq!(
        keys(&resized),
        keys(&cold),
        "the resized atlas holds other pages: the floor proves nothing"
    );
    if floor_channels == 0 {
        assert_eq!(
            channels, 0,
            "in-place differs from a cold load of the edited map: {channels} channels (max {max})"
        );
    } else {
        assert!(
            max <= floor_max,
            "in-place differs from a cold load of the edited map by {max} (placement floor {floor_max}): {channels} channels"
        );
    }
}
