// SPDX-License-Identifier: MPL-2.0
//! ADR-1386: an emitter whose light the baked GI volume holds is counted once on the tiers that
//! also draw emissive meshes as area lights (Desktop High and Ultra).
//!
//! A closed grey room (albedo 0.5, fully rough) lit only by a small ceiling panel, baked by the path tracer
//! from the very frame the renderer draws (one interior brick on the room's air box), compared
//! in linear HDR (cd/m², `FrameRenderer::take_hdr_capture`):
//!
//! - **fixed**: Desktop High with the format v3 volume (it names the panel);
//! - **no area lights**: the same frame with the emissive light budget at 0 (the Desktop Low,
//!   Deck and Mobile behavior, which matched the path-traced reference in the v0.2.0 look
//!   review);
//! - **double count**: the volume with its emitter list stripped (what a format v2 bake reads
//!   as): every area light stays, the bug this lane fixes;
//! - **direct only**: no volume, the area lights alone (no ambient, no sun).
//!
//! The fixed frame is the frame without area lights plus at most the area lights' specular (the
//! bake holds diffuse light only, and with no IBL the probes do not stand in for the lobe), adds
//! no more than the indirect light a closed room of that albedo holds over the direct-only frame,
//! and sits a full direct light under the double count. An emitter added after the bake keeps its
//! area light inside the volume. A baked emitter keeps its area light where the volume does not
//! reach (an open floor half covered by a brick). On the tiers without emissive area lights the
//! output is the same bytes with and without the emitter list.
//! No golden. Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{BakedGiVolume, VolumeLayout};
use regia_core::glam::Mat4;
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume, detail_brick};
use regia_pt::{PathTracer, PtScene};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, box_mesh,
};

const W: u32 = 128;
const H: u32 = 96;
/// The walls' albedo.
const ALBEDO: f32 = 0.5;
/// The room's air box (the walls' inner faces).
const AIR_MIN: [f32; 3] = [-1.0, 0.0, -1.0];
const AIR_MAX: [f32; 3] = [1.0, 2.0, 1.0];

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("baked GI needs a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

fn block(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn quality(preset: RenderProfilePreset) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    q.taau = false;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.baked_gi = true;
    q
}

/// The closed room, lit by a 0.4 m ceiling panel (material 1); with `strip`, also a thin
/// emissive strip low on the back wall (material 2), which the bake never saw.
fn room(strip: bool) -> LabScene {
    let grey = LabMaterial {
        base_color: [ALBEDO, ALBEDO, ALBEDO, 1.0],
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let emitter = |intensity: f32| LabMaterial {
        base_color: [0.78, 0.78, 0.78, 1.0],
        roughness: 1.0,
        emissive: [1.0, 0.9, 0.75],
        emissive_intensity: intensity,
        ..LabMaterial::default()
    };
    let mut meshes = vec![
        block([0.0, -0.05, 0.0], [1.1, 0.05, 1.1], 0),
        block([0.0, 2.05, 0.0], [1.1, 0.05, 1.1], 0),
        block([0.0, 1.0, -1.05], [1.1, 1.1, 0.05], 0),
        block([0.0, 1.0, 1.05], [1.1, 1.1, 0.05], 0),
        block([-1.05, 1.0, 0.0], [0.05, 1.1, 1.1], 0),
        block([1.05, 1.0, 0.0], [0.05, 1.1, 1.1], 0),
        block([0.0, 1.99, 0.0], [0.2, 0.01, 0.2], 1),
    ];
    if strip {
        meshes.push(block([0.4, 0.35, -0.98], [0.3, 0.02, 0.02], 2));
    }
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 5.0;
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.1, 0.8],
        target: [0.0, 0.7, -1.0],
        fov_y: 1.0,
        near: 0.05,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 5.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.3, -0.8, -0.5], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![grey, emitter(3000.0), emitter(6000.0)],
        meshes,
    }
}

fn layout() -> VolumeLayout {
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick(AIR_MIN, AIR_MAX, 0.5)],
        moment_resolution: 8,
        max_distance: 4.0,
    };
    assert_eq!(layout.flag_interior_bricks(&[(AIR_MIN, AIR_MAX)]), 1);
    layout.assign_brick_offsets();
    layout
}

fn pt_scene(scene: &LabScene) -> PtScene {
    from_frame(
        &scene.as_frame_scene_with_quality(quality(RenderProfilePreset::DesktopHigh)),
        &ExtractOptions::default(),
    )
}

/// `layout` baked in `scene` (format v3: the volume names the scene's emitters).
fn bake(scene: &LabScene, layout: &VolumeLayout) -> Arc<BakedGiVolume> {
    let settings = GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 128,
            samples: 8,
            max_bounces: 6,
            seed: 3,
            samples_per_submit: 8,
            ..ProbeTraceSettings::default()
        },
        ..GiBakeSettings::default()
    };
    let bake = bake_gi_volume(tracer(), &pt_scene(scene), layout, &settings, |_, _, _| true).expect("bake");
    eprintln!("baked {:?}", bake.report);
    assert_eq!(bake.report.rejected_rays, 0);
    Arc::new(bake.volume)
}

/// The room without the strip, baked once (it names the panel).
fn volume() -> Arc<BakedGiVolume> {
    static V: OnceLock<Arc<BakedGiVolume>> = OnceLock::new();
    V.get_or_init(|| bake(&room(false), &layout())).clone()
}

/// The same volume as a format v2 bake reads: no emitter list.
fn unnamed(volume: &BakedGiVolume) -> Arc<BakedGiVolume> {
    let mut v = volume.clone();
    v.emitters = None;
    Arc::new(v)
}

struct Frame {
    /// Linear HDR, cd/m² (`None` when the tier's frame has no linear post input).
    hdr: Option<Vec<[f32; 3]>>,
    /// The display pixels (RGBA8).
    pixels: Vec<u8>,
    baked_active: bool,
    /// Uploaded / tagged emissive area lights.
    lights: (u32, u32),
}

fn render(scene: &LabScene, volume: Option<Arc<BakedGiVolume>>, q: FrameQuality) -> Frame {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    let baked = volume.is_some();
    renderer.set_baked_gi(volume);
    renderer.set_baked_gi_enabled(Some(baked));
    let frame = scene.as_frame_scene_with_quality(q);
    renderer.render_offscreen(device(), &frame).expect("render");
    renderer.request_hdr_capture();
    let pixels = renderer
        .render_offscreen(device(), &frame)
        .expect("render")
        .pixels;
    let hdr = renderer.take_hdr_capture(device()).expect("HDR readback");
    if let Some(h) = &hdr {
        assert_eq!((h.width, h.height), (W, H));
    }
    let report = renderer.emissive_lights();
    Frame {
        hdr: hdr.map(|h| h.rgb),
        pixels,
        baked_active: renderer.baked_gi_status().active,
        lights: (report.uploaded, report.baked),
    }
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Mean luminance (cd/m²) of the rows `y0..y1` of `frame`, leaving out the pixels `skip` names.
fn mean_luminance(frame: &Frame, y0: u32, y1: u32, skip: &[bool]) -> f32 {
    let hdr = frame
        .hdr
        .as_ref()
        .expect("an HDR capture on Desktop High / Ultra");
    let (mut sum, mut n) = (0.0f64, 0u32);
    let (start, end) = ((y0 * W) as usize, (y1 * W) as usize);
    for (i, c) in hdr.iter().enumerate().take(end).skip(start) {
        if !skip.get(i).copied().unwrap_or(false) {
            sum += f64::from(luminance(*c));
            n += 1;
        }
    }
    (sum / f64::from(n.max(1))) as f32
}

#[test]
fn a_baked_emitter_is_counted_once_on_the_area_light_tiers() {
    let scene = room(false);
    let volume = volume();
    assert_eq!(
        volume.emitters.as_ref().map(Vec::len),
        Some(1),
        "the bake names the panel"
    );
    for preset in [
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopUltra,
    ] {
        let q = quality(preset);
        let fixed = render(&scene, Some(volume.clone()), q);
        let no_area = render(
            &scene,
            Some(volume.clone()),
            FrameQuality {
                emissive_lights: 0,
                ..q
            },
        );
        let double = render(&scene, Some(unnamed(&volume)), q);
        let direct = render(&scene, None, q);
        assert!(fixed.baked_active && no_area.baked_active && double.baked_active);
        assert!(!direct.baked_active);
        // The panel's light is fitted and uploaded; only the named volume tags it.
        assert!(fixed.lights.0 >= 1, "{preset:?}: {:?}", fixed.lights);
        assert_eq!(
            fixed.lights.1, fixed.lights.0,
            "{preset:?}: every panel light is tagged"
        );
        assert_eq!(double.lights.1, 0, "{preset:?}: an unnamed volume tags nothing");
        assert_eq!(no_area.lights, (0, 0));
        let [f, n, d, a] = [&fixed, &no_area, &double, &direct].map(|fr| mean_luminance(fr, 0, H, &[]));
        eprintln!(
            "{preset:?}: fixed {f:.2} cd/m², no area lights {n:.2}, double count {d:.2}, direct only {a:.2}"
        );
        // Counted once: the frame without area lights plus at most the area lights' specular
        // (a fully rough dielectric reflects ~5 % specularly against 50 % diffuse, and the direct
        // light is about half of the frame), never less.
        assert!(f >= n * 0.999, "{preset:?}: fixed {f} under the bake alone {n}");
        assert!(f <= n * 1.12, "{preset:?}: fixed {f} vs no area lights {n}");
        // Over the direct-only frame, no more than the indirect light a closed room of albedo ρ
        // holds: an integrating sphere has E_indirect = ρ / (1 − ρ) · E_direct on average; twice
        // that covers this view (the back wall gets less direct light than the floor) and the
        // probes' L1. And not darker than the direct light alone.
        let ceiling = a * (1.0 + 2.0 * ALBEDO / (1.0 - ALBEDO));
        assert!(
            f <= ceiling,
            "{preset:?}: fixed {f} over the direct {a} plus the indirect bound ({ceiling})"
        );
        assert!(f >= a * 0.95, "{preset:?}: fixed {f} under the direct light {a}");
        // The bug: the volume's emitter light plus the area lights' diffuse again, which is
        // nearly all of the direct-only frame.
        assert!(
            d - f >= a * 0.8,
            "{preset:?}: the double count {d} is not a direct light ({a}) over {f}"
        );
    }
}

#[test]
fn an_emitter_added_after_the_bake_keeps_its_area_light_inside_the_volume() {
    let volume = volume();
    let q = quality(RenderProfilePreset::DesktopHigh);
    let (without, with) = (room(false), room(true));
    let baked = [
        render(&without, Some(volume.clone()), q),
        render(&with, Some(volume.clone()), q),
    ];
    let live = [render(&without, None, q), render(&with, None, q)];
    // The strip is fitted and uploaded, untagged; the panel stays tagged.
    assert_eq!(
        baked[1].lights.0,
        baked[0].lights.0 + 1,
        "{:?} {:?}",
        baked[0].lights,
        baked[1].lights
    );
    assert_eq!(baked[1].lights.1, baked[0].lights.1);
    // What the strip adds to the lower half of the frame (the floor and the back wall's foot),
    // its own glowing pixels left out (they glow the same either way): the same over the bake
    // as with no volume at all, so the bake takes none of its light away.
    let glow: Vec<bool> = live[1]
        .hdr
        .as_ref()
        .expect("HDR")
        .iter()
        .map(|c| luminance(*c) > 1000.0)
        .collect();
    assert!(glow.iter().any(|&g| g), "the strip is in view");
    let added = |pair: &[Frame; 2]| {
        mean_luminance(&pair[1], H / 2, H, &glow) - mean_luminance(&pair[0], H / 2, H, &glow)
    };
    let (over_bake, alone) = (added(&baked), added(&live));
    eprintln!("the strip adds {over_bake:.2} cd/m² over the bake, {alone:.2} with no volume");
    assert!(alone > 1.0, "the strip lights the floor ({alone})");
    assert!(
        (over_bake / alone - 1.0).abs() < 0.15,
        "the strip's light over the bake {over_bake} vs alone {alone}"
    );
}

#[test]
fn tiers_without_emissive_area_lights_render_the_same_bytes() {
    let scene = room(false);
    let volume = volume();
    for preset in [
        RenderProfilePreset::DesktopLow,
        RenderProfilePreset::Deck,
        RenderProfilePreset::MobileHigh,
        RenderProfilePreset::MobileLow,
    ] {
        let q = quality(preset);
        assert_eq!(q.emissive_lights, 0, "{preset:?} draws no emissive area lights");
        let named = render(&scene, Some(volume.clone()), q);
        let old = render(&scene, Some(unnamed(&volume)), q);
        eprintln!("{preset:?}: baked active {}", named.baked_active);
        assert_eq!(named.lights, (0, 0));
        assert!(
            named.pixels == old.pixels,
            "{preset:?}: the emitter list changed the pixels"
        );
        assert!(
            named.hdr == old.hdr,
            "{preset:?}: the emitter list changed the HDR frame"
        );
    }
}

/// An open floor with a lamp over its right half; the bake's one (outdoor) brick covers only the
/// left half. The bake traced the lamp, so the volume names it and its area light is tagged.
fn half_covered_yard() -> LabScene {
    let mut scene = room(false);
    scene.meshes = vec![
        block([0.0, -0.05, 0.0], [6.0, 0.05, 4.0], 0),
        block([3.0, 1.0, 0.0], [0.3, 0.02, 0.3], 1),
    ];
    scene.eye = [0.0, 5.0, 7.0];
    scene.target = [0.0, 0.0, 0.0];
    scene.far = 100.0;
    scene
}

/// The world (X, Z) where the view ray through pixel `(x, y)` meets the floor (y = 0), for the
/// lab camera without Hor+.
fn floor_xz(scene: &LabScene, x: u32, y: u32) -> Option<[f32; 2]> {
    use regia_core::glam::Vec3;
    let eye = Vec3::from_array(scene.eye);
    let fwd = (Vec3::from_array(scene.target) - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let aspect = W as f32 / H as f32;
    let t = (scene.fov_y * 0.5).tan();
    let ndc = [
        (x as f32 + 0.5) / W as f32 * 2.0 - 1.0,
        1.0 - (y as f32 + 0.5) / H as f32 * 2.0,
    ];
    let dir = (fwd + right * ndc[0] * t * aspect + up * ndc[1] * t).normalize();
    (dir.y < -1e-3).then(|| {
        let p = eye + dir * (-eye.y / dir.y);
        [p.x, p.z]
    })
}

/// Mean luminance (cd/m²) of the floor pixels whose X lies in `xs` and Z in `zs`, the lamp's
/// glowing pixels (`glow`) left out.
fn floor_mean(
    scene: &LabScene,
    frame: &Frame,
    xs: std::ops::Range<f32>,
    zs: std::ops::Range<f32>,
    glow: &[bool],
) -> f32 {
    let hdr = frame.hdr.as_ref().expect("an HDR capture on Desktop High");
    let (mut sum, mut n) = (0.0f64, 0u32);
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) as usize;
            let on = floor_xz(scene, x, y).is_some_and(|[fx, fz]| xs.contains(&fx) && zs.contains(&fz));
            if glow[i] || !on {
                continue;
            }
            sum += f64::from(luminance(hdr[i]));
            n += 1;
        }
    }
    assert!(n > 100, "{n} floor pixels in {xs:?} × {zs:?}");
    (sum / f64::from(n)) as f32
}

#[test]
fn a_baked_emitter_keeps_its_area_light_where_the_volume_does_not_reach() {
    let scene = half_covered_yard();
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick([-4.0, 0.0, -3.0], [0.0, 2.0, 3.0], 0.5)],
        moment_resolution: 8,
        max_distance: 8.0,
    };
    layout.assign_brick_offsets();
    let volume = bake(&scene, &layout);
    assert_eq!(
        volume.emitters.as_ref().map(Vec::len),
        Some(1),
        "the bake names the lamp"
    );
    let q = quality(RenderProfilePreset::DesktopHigh);
    let fixed = render(&scene, Some(volume.clone()), q);
    let no_area = render(
        &scene,
        Some(volume),
        FrameQuality {
            emissive_lights: 0,
            ..q
        },
    );
    let direct = render(&scene, None, q);
    let unlit = render(
        &scene,
        None,
        FrameQuality {
            emissive_lights: 0,
            ..q
        },
    );
    assert!(fixed.baked_active);
    assert!(
        fixed.lights.0 >= 1 && fixed.lights.1 == fixed.lights.0,
        "{:?}",
        fixed.lights
    );
    let glow: Vec<bool> = direct
        .hdr
        .as_ref()
        .expect("HDR")
        .iter()
        .map(|c| luminance(*c) > 1000.0)
        .collect();
    // Outside the brick (x > 1, past its edge and the half-resolution lookup), and inside it,
    // clear of its fading last cell (x in −3.5..−1, |z| < 2.5).
    let (out, all_z) = (1.0..6.0, -4.0..4.0);
    let [f, n, d, u] = [&fixed, &no_area, &direct, &unlit]
        .map(|fr| floor_mean(&scene, fr, out.clone(), all_z.clone(), &glow));
    let (xs, zs) = (-3.5..-1.0, -2.5..2.5);
    let [fi, ni] = [&fixed, &no_area].map(|fr| floor_mean(&scene, fr, xs.clone(), zs.clone(), &glow));
    eprintln!(
        "uncovered floor: fixed {f:.2} cd/m², bake alone {n:.2}, no volume {d:.2}, no volume and no area lights {u:.2}"
    );
    eprintln!("covered floor: fixed {fi:.2} cd/m², bake alone {ni:.2}");
    // Uncovered: the area light lights the floor exactly as with no volume (within 5 %: the
    // frame without a volume may run SSGI), it is most of the light there, and the volume adds
    // nothing of its own.
    assert!(
        (f / d - 1.0).abs() < 0.05,
        "uncovered: fixed {f} vs no volume {d}"
    );
    assert!(
        f - u > 0.5 * f,
        "uncovered: the area light ({f} − {u}) should be most of it"
    );
    assert!(
        (n - u).abs() < 0.05 * f,
        "uncovered: the bake alone {n} vs no light {u}"
    );
    // Covered: the bake's light, as with the area lights off, plus at most the lamp's specular.
    // This floor gets no bounce light, so the specular (about a tenth of the lamp's diffuse on a
    // rough dielectric) is a larger share than in the room.
    assert!(
        fi >= ni * 0.999 && fi <= ni * 1.25,
        "covered: fixed {fi} vs the bake alone {ni}"
    );
}
