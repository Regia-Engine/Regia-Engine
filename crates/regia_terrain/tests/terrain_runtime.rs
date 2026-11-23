// SPDX-License-Identifier: MPL-2.0
//! Runtime terrain changes on the shared terrain draw (ADR-1595, DO-4).
//!
//! A tunnel mouth is a named hole opened through the draw's `TerrainHoles` handle after the
//! draw is boxed into the frame renderer. Checked on a real frame:
//!
//! - a closed named hole costs nothing: no node hole tests, and the frame is bit-identical to
//!   a terrain without holes;
//! - opened, it draws exactly the cut the same footprint gives as a plain shape (ADR-088), and
//!   every pixel is where a ray against the footprint predicts: ground outside, the floor slab
//!   lit through the opening, and the floor in the shadow of the ground around it (no light
//!   leaks through the uncut ground, the cut casts no shadow);
//! - closing it again gives back the uncut frame bit for bit (the frame hash is stable);
//! - a runtime height patch reaches the drawn terrain: the frame after the patch equals the
//!   frame of a draw built from the patched map, and the rendered surface reports the new
//!   heights.

use std::sync::OnceLock;

use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, HeightPatch, HoleShape, HolesMask, LayerDesc, LayerTextures, MaterialParams, NamedHole,
    Rgba8Image, TerrainDrawStats, TerrainHeightPatches, TerrainHoles, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainSharedDraw,
};

const W: u32 = 320;
const H: u32 = 180;
/// Top of the floor slab under the mouth.
const FLOOR_Y: f32 = -9.0;
/// Radius of the flat ground around the mouth.
const FLAT_R: f32 = 60.0;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// The mouth's footprint: 25 m × 20 m, rotated 0.4 rad, centred on the origin.
fn footprint() -> HoleShape {
    HoleShape::Rect {
        center: [0.0, 0.0],
        half_extents: [12.5, 10.0],
        rotation_radians: 0.4,
    }
}

fn named_mouth(open: bool) -> NamedHole {
    NamedHole {
        id: "tunnel_mouth".into(),
        open,
        shape: footprint(),
    }
}

/// 1024 m, 1 m samples: flat inside `FLAT_R`, rolling hills beyond.
fn map() -> TerrainMap {
    let n = 1025u32;
    let world = 1024.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            let r = (x * x + z * z).sqrt();
            let t = ((r - FLAT_R) / 40.0).clamp(0.0, 1.0);
            heights.push(6.0 * (x / 37.0).sin() * (z / 29.0).cos() * t * t * (3.0 - 2.0 * t));
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 40.0, -20.0).expect("map")
}

fn draw(map: TerrainMap) -> TerrainSharedDraw {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.4);
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
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
    draw
}

const EYE: [f32; 3] = [6.0, 45.0, 18.0];
const FOV_Y: f32 = 0.9;
const SUN_ELEVATION_DEG: f32 = 55.0;

fn sun_travel() -> [f32; 3] {
    let el = SUN_ELEVATION_DEG.to_radians();
    let az = 0.3f32;
    [-el.cos() * az.cos(), -el.sin(), -el.cos() * az.sin()]
}

/// A frame renderer with `draw` boxed into it, the handles kept, and the scene to render.
struct Rig {
    renderer: FrameRenderer,
    scene: LabScene,
    post: PostSettings,
    quality: FrameQuality,
    holes: TerrainHoles,
    patches: TerrainHeightPatches,
    stats: std::sync::Arc<TerrainDrawStats>,
    surface: regia_terrain::RenderedSurface,
}

impl Rig {
    fn new(draw: TerrainSharedDraw) -> Self {
        let holes = draw.holes_handle();
        let patches = draw.height_patches_handle();
        let stats = draw.stats();
        let surface = draw.surface();
        let post = PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        };
        let (verts, idx) = regia_render::box_mesh([40.0, 0.5, 40.0]);
        let verts: Vec<_> = verts
            .iter()
            .map(|v| regia_render::LabVertex {
                position: [v.position[0], v.position[1] + FLOOR_Y - 0.5, v.position[2]],
                ..*v
            })
            .collect();
        let scene = LabScene {
            width: W,
            height: H,
            eye: EYE,
            target: [0.0, FLOOR_Y * 0.5, 0.0],
            fov_y: FOV_Y,
            near: 0.5,
            far: 4000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 12.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: Some(post.clone()),
            directional: DirectionalLightUniform::new(sun_travel(), [1.0, 1.0, 1.0], 20_000.0),
            local_lights: Vec::new(),
            materials: vec![regia_render::LabMaterial {
                base_color: [0.8, 0.04, 0.04, 1.0],
                metallic: 0.0,
                roughness: 1.0,
                ..regia_render::LabMaterial::default()
            }],
            meshes: vec![regia_render::LabMesh {
                vertices: verts,
                indices: idx,
                material: 0,
                ..regia_render::LabMesh::unskinned()
            }],
        };
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = false;
        let mut renderer = FrameRenderer::new();
        renderer.set_outdoor(OutdoorParams {
            planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
            ..OutdoorParams::default()
        });
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        Self {
            renderer,
            scene,
            post,
            quality,
            holes,
            patches,
            stats,
            surface,
        }
    }

    /// A frame after a camera cut: the renderer's per-frame state (noise index, temporal
    /// history) restarts, so the frame equals the first frame of a fresh renderer drawing the
    /// same content, bit for bit. Frames compared across rigs are taken this way.
    fn cut_frame(&mut self) -> (Vec<u8>, u32) {
        self.renderer.camera_cut();
        self.frame()
    }

    /// One frame: its pixels and the frame's node hole tests.
    fn frame(&mut self) -> (Vec<u8>, u32) {
        let mut frame = self.scene.as_frame_scene_with_quality(self.quality);
        frame.post = Some(&self.post);
        let pixels = self
            .renderer
            .render_offscreen(device(), &frame)
            .expect("frame")
            .pixels;
        (pixels, self.stats.hole_tests())
    }
}

fn hash(pixels: &[u8]) -> u64 {
    // FNV-1a: stable across runs and platforms.
    pixels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Ground,
    FloorLit,
    FloorShadow,
    Skip,
}

fn inverse_view_proj() -> Mat4 {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(EYE),
        Vec3::new(0.0, FLOOR_Y * 0.5, 0.0),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, 0.5, 4000.0);
    (proj * view).inverse()
}

/// What pixel (x, y) shows, from its ray against the exact footprint.
fn predict(inv: Mat4, mask: &HolesMask, x: f32, y: f32) -> Class {
    let ndc = [x / W as f32 * 2.0 - 1.0, 1.0 - y / H as f32 * 2.0];
    let a = inv * Vec4::new(ndc[0], ndc[1], 0.0, 1.0);
    let b = inv * Vec4::new(ndc[0], ndc[1], 1.0, 1.0);
    let (a, b) = (a.truncate() / a.w, b.truncate() / b.w);
    let dir = (b - a).normalize();
    let eye = Vec3::from_array(EYE);
    if dir.y >= -1e-4 {
        return Class::Skip;
    }
    let ground = eye + dir * (-eye.y / dir.y);
    if ground.x.hypot(ground.z) > FLAT_R - 5.0 {
        return Class::Skip;
    }
    if !mask.contains(ground.x, ground.z) {
        return Class::Ground;
    }
    let floor = eye + dir * ((FLOOR_Y - eye.y) / dir.y);
    let to_sun = -Vec3::from_array(sun_travel());
    let up = floor + to_sun * ((0.0 - FLOOR_Y) / to_sun.y);
    if mask.contains(up.x, up.z) {
        Class::FloorLit
    } else {
        Class::FloorShadow
    }
}

fn classify(px: &[u8], x: u32, y: u32) -> Option<Class> {
    let i = ((y * W + x) * 4) as usize;
    let (r, g, b) = (i32::from(px[i]), i32::from(px[i + 1]), i32::from(px[i + 2]));
    let max = r.max(g).max(b);
    if max < 28 {
        Some(Class::FloorShadow)
    } else if r > 2 * g && r > 2 * b {
        Some(Class::FloorLit)
    } else if (r - g).abs() * 4 < max && (r - b).abs() * 4 < max {
        Some(Class::Ground)
    } else {
        None
    }
}

/// Per class (ground, lit floor, shadowed floor): (pixels judged, pixels matching), judging
/// only pixels whose neighbourhood agrees (2 px around the cut edge, 5 px around the shadow's).
fn agreement(px: &[u8], mask: &HolesMask) -> [(u32, u32); 3] {
    let inv = inverse_view_proj();
    let grid: Vec<Class> = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| predict(inv, mask, x as f32 + 0.5, y as f32 + 0.5))
        .collect();
    let at = |x: i32, y: i32| {
        grid[(y.clamp(0, H as i32 - 1) as u32 * W + x.clamp(0, W as i32 - 1) as u32) as usize]
    };
    let mut out = [(0u32, 0u32); 3];
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let c = at(x, y);
            let slot = match c {
                Class::Ground => 0,
                Class::FloorLit => 1,
                Class::FloorShadow => 2,
                Class::Skip => continue,
            };
            let margin = if c == Class::Ground { 2 } else { 5 };
            if !(-margin..=margin).all(|dy| (-margin..=margin).all(|dx| at(x + dx, y + dy) == c)) {
                continue;
            }
            out[slot].0 += 1;
            if classify(px, x as u32, y as u32) == Some(c) {
                out[slot].1 += 1;
            }
        }
    }
    out
}

fn red_pixels(px: &[u8]) -> usize {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .filter(|&(x, y)| classify(px, x, y) == Some(Class::FloorLit))
        .count()
}

#[test]
fn a_named_tunnel_mouth_opens_without_light_leaks_and_closes_back_bit_for_bit() {
    // The reference: the same footprint cut as a plain shape (ADR-088) from the first frame.
    let mut plain = draw(map());
    plain.set_holes(Some(&HolesMask::from_shapes(vec![footprint()]).expect("mask")));
    let (reference, _) = Rig::new(plain).frame();

    let mut rig = Rig::new(draw(map()));
    let (uncut, tests) = rig.frame();
    assert_eq!(tests, 0, "no holes: no node hole tests");
    assert_eq!(red_pixels(&uncut), 0, "the floor is hidden under uncut ground");

    // Closed: the mask cuts nothing, costs nothing, and the frame does not change.
    rig.holes
        .edit(|m| m.set_named(named_mouth(false)).map(|_| ()))
        .expect("add closed");
    let (closed, tests) = rig.cut_frame();
    assert_eq!(tests, 0, "a closed hole is not tested against any node");
    assert_eq!(
        hash(&closed),
        hash(&uncut),
        "a closed hole leaves the frame bit-identical"
    );
    assert!(rig.holes.state().requested.is_none(), "the draw took the request");

    // Opened by name: the same cut as the plain shape, every pixel where the footprint says.
    rig.holes
        .edit(|m| m.set_open("tunnel_mouth", true))
        .expect("open");
    let (open, tests) = rig.cut_frame();
    assert!(tests > 0, "the open hole classifies the nodes it touches");
    assert_eq!(
        hash(&open),
        hash(&reference),
        "named and plain cuts draw the same frame"
    );
    let mask = rig.holes.state().applied;
    assert_eq!(mask.shape_count(), 1);
    let [ground, lit, shadow] = agreement(&open, &mask);
    let pct = |(n, ok): (u32, u32)| f64::from(ok) * 100.0 / f64::from(n.max(1));
    eprintln!(
        "ground {:.2} % of {}, floor lit {:.2} % of {}, floor shadow {:.2} % of {}",
        pct(ground),
        ground.0,
        pct(lit),
        lit.0,
        pct(shadow),
        shadow.0
    );
    assert!(
        ground.0 > 2000 && lit.0 > 300 && shadow.0 > 150,
        "{ground:?} {lit:?} {shadow:?}"
    );
    assert!(
        pct(ground) > 99.5,
        "ground where the mouth is not: {:.2} %",
        pct(ground)
    );
    assert!(
        pct(lit) > 99.0,
        "the floor under the mouth is hidden or shadowed by it: {:.2} %",
        pct(lit)
    );
    assert!(
        pct(shadow) > 98.0,
        "light leaks onto the floor under the uncut ground: {:.2} % dark",
        pct(shadow)
    );
    assert!(rig.surface.in_hole(0.0, 0.0) && !rig.surface.in_hole(30.0, 0.0));

    // Closed again: back to the uncut frame, bit for bit.
    rig.holes
        .edit(|m| m.set_open("tunnel_mouth", false))
        .expect("close");
    let (reclosed, _) = rig.cut_frame();
    assert_eq!(hash(&reclosed), hash(&uncut), "closing restores the uncut frame");
    let (held, tests) = rig.cut_frame();
    assert_eq!(tests, 0);
    assert_eq!(
        hash(&held),
        hash(&uncut),
        "a held closed mask stays free and identical"
    );
}

/// A runtime patch raises a 12 m block of the flat ground by 6 m through the draw's patch
/// queue, over several frames' budgets. The frame afterwards equals the frame of a draw built
/// from the patched map; the rendered surface reports the block's new top.
#[test]
fn a_runtime_height_patch_reaches_the_drawn_terrain() {
    let base = map();
    let mut rig = Rig::new(draw(base.clone()));
    let (before, _) = rig.frame();
    // Samples 520..532 (x, z from 8 m to 19 m), 6 m above the flat ground's 0 m.
    let patch = HeightPatch {
        col: 520,
        row: 520,
        width: 12,
        height: 12,
        heights: vec![6.0; 144],
    };
    rig.patches.set_budget(60);
    rig.patches.request(patch).expect("patch");
    let mut patched = base;
    let mut frames = 0;
    while !rig.patches.is_idle() {
        // What `apply_terrain_height_patches` does each frame, minus the ECS map.
        for band in rig.patches.take_frame() {
            assert!(band.len() <= 60);
            band.apply(&mut patched).expect("same grid");
        }
        rig.frame();
        frames += 1;
        assert_eq!(
            rig.patches.state().draw_pending,
            0,
            "each frame applies its bands"
        );
    }
    assert_eq!(frames, 3, "144 samples at 60 a frame: 5 rows, 5 rows, 2 rows");
    assert!(rig.patches.is_idle() && rig.patches.state().completed == 1);
    let after = rig.cut_frame().0;
    assert_ne!(hash(&after), hash(&before), "the block shows");
    let top = rig
        .surface
        .heights(&patched, &[[13.5, 13.5], [-30.0, -30.0]])
        .expect("selected");
    assert!((top[0] - 6.0).abs() < 0.01, "the rendered block top: {top:?}");
    assert!(top[1].abs() < 0.01, "untouched ground: {top:?}");

    let mut fresh = Rig::new(draw(patched));
    let (direct, _) = fresh.frame();
    assert_eq!(
        hash(&after),
        hash(&direct),
        "the patched draw renders what a draw built from the patched map renders"
    );
}

/// The mouth holds with the terrain virtual texture on: the cached surface pages never draw
/// ground inside the cut, and the floor shows and is shadowed exactly as without the cache.
#[test]
fn the_mouth_holds_with_the_virtual_texture_on() {
    let mut vt_draw = draw(map());
    if !device()
        .device
        .features()
        .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST)
    {
        // The virtual texture needs forced early depth tests (Vulkan, not Metal): the draw
        // leaves it off on such a device, so there is nothing to hold (ADR-2010).
        eprintln!("skipped: this device cannot run the terrain virtual texture");
        return;
    }
    vt_draw
        .set_virtual_texture(Some(regia_terrain::VirtualTextureSettings {
            lod0_texel_m: 0.05,
            page_texels: 30,
            atlas_slots: 24,
            near_m: 0.0,
            bakes_per_frame: 600,
            rebakes_per_frame: 0,
            stochastic_lod: false,
            ..regia_terrain::VirtualTextureSettings::default()
        }))
        .expect("virtual texture");
    let vt = vt_draw.vt_handle();
    let mut rig = Rig::new(vt_draw);
    rig.holes
        .edit(|m| m.set_named(named_mouth(true)).map(|_| ()))
        .expect("open");
    let mut px = Vec::new();
    for _ in 0..3 {
        px = rig.frame().0;
    }
    let resident = vt.stats().map_or(0, |s| s.resident);
    assert!(resident > 0, "the virtual texture is in use: {resident} pages");
    let [ground, lit, shadow] = agreement(&px, &rig.holes.state().applied);
    let pct = |(n, ok): (u32, u32)| f64::from(ok) * 100.0 / f64::from(n.max(1));
    eprintln!(
        "VT on ({resident} pages): ground {:.2} %, floor lit {:.2} %, floor shadow {:.2} %",
        pct(ground),
        pct(lit),
        pct(shadow)
    );
    assert!(
        ground.0 > 2000 && lit.0 > 300 && shadow.0 > 150,
        "{ground:?} {lit:?} {shadow:?}"
    );
    assert!(
        pct(ground) > 99.5,
        "ground where the mouth is not: {:.2} %",
        pct(ground)
    );
    assert!(
        pct(lit) > 99.0,
        "cached ground inside the mouth: {:.2} % lit floor",
        pct(lit)
    );
    assert!(
        pct(shadow) > 98.0,
        "light leaks under the uncut ground: {:.2} %",
        pct(shadow)
    );
}
