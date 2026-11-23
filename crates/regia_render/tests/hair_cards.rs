// SPDX-License-Identifier: MPL-2.0
//! Hair card shading (B5, `regia_render::hair`): pixels move only for a material that enables
//! it, the two-lobe specular and the root-to-tip ramp show on a card mesh, the alpha strategies
//! draw, and a card casts its shadow through its alpha test.
//!
//! Measured on the linear HDR capture. `REGIA_PREVIEW=<dir>` also writes the look-dev image of
//! the card fan (hair on, hair off) as PNGs; no golden is compared or blessed here.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    HairAlpha, HairFactors, LabMaterial, LabTexture, LabVertex, PostSettings, RenderDevice, ShadowCast,
};

const W: u32 = 192;
const H: u32 = 128;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip hair cards: {err}");
            None
        }
    }
}

type Geometry = (Vec<LabVertex>, Vec<u32>);

/// A vertical strip `width` wide and `height` tall, root at the bottom (`v = 0`), tip at the top
/// (`v = 1`), bent toward +z with `bend`. The tangent runs along the strand, root to tip.
fn card(width: f32, height: f32, segments: u32, bend: f32) -> Geometry {
    let mut vertices = Vec::new();
    for i in 0..=segments {
        let v = i as f32 / segments as f32;
        let y = v * height;
        let z = bend * v * v;
        // d/dv of (y, z) normalised: the strand direction.
        let tangent = Vec3::new(0.0, height, 2.0 * bend * v).normalize();
        let normal = Vec3::new(0.0, -tangent.z, tangent.y).normalize();
        for (x, u) in [(-0.5 * width, 0.0), (0.5 * width, 1.0)] {
            let mut vertex = LabVertex::from_pos_normal_uv([x, y, z], normal.to_array(), [u, v]);
            vertex.tangent = [tangent.x, tangent.y, tangent.z, 1.0];
            vertices.push(vertex);
        }
    }
    let mut indices = Vec::new();
    for i in 0..segments {
        let a = i * 2;
        indices.extend_from_slice(&[a, a + 1, a + 2, a + 1, a + 3, a + 2]);
    }
    (vertices, indices)
}

/// A horizontal floor of half-size `h` at height `y`.
fn floor(h: f32, y: f32) -> Geometry {
    let corners = [[-h, y, -h], [h, y, -h], [h, y, h], [-h, y, h]];
    let vertices = corners
        .into_iter()
        .map(|p| LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.0, 0.0]))
        .collect();
    (vertices, vec![0, 2, 1, 0, 3, 2])
}

/// Strand-stripe albedo: `strands` opaque strands across `u`, each with a soft edge, plus a
/// fully opaque variant for the shadow test. Colour is a mid brown; alpha is the strand cover.
fn strand_texture(strands: u32, opaque: bool) -> LabTexture {
    let (w, h) = (64u32, 64u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _y in 0..h {
        for x in 0..w {
            let u = x as f32 / w as f32 * strands as f32;
            let d = (u.fract() - 0.5).abs() * 2.0;
            let cover = if opaque {
                1.0
            } else {
                (1.0 - d * 1.6).clamp(0.0, 1.0)
            };
            rgba.extend_from_slice(&[150, 100, 60, (cover * 255.0) as u8]);
        }
    }
    LabTexture {
        width: w,
        height: h,
        rgba,
    }
}

fn hair_material(hair: HairFactors) -> LabMaterial {
    let mut material = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.6,
        albedo: Some(strand_texture(6, false)),
        double_sided: true,
        hair,
        ..LabMaterial::default()
    };
    if hair.enabled {
        let (mut mode, mut cutoff) = (material.alpha_mode, material.alpha_cutoff);
        hair.apply_alpha_mode(&mut mode, &mut cutoff);
        material.alpha_mode = mode;
        material.alpha_cutoff = cutoff;
    } else {
        // Hair off on the same alpha-tested card: the comparison differs only in the feature.
        material.alpha_mode = ALPHA_MASK;
        material.alpha_cutoff = 0.5;
    }
    material
}

fn post() -> PostSettings {
    PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        exposure_ev: 12.0,
        ..PostSettings::default()
    }
}

struct Frame {
    rgb: Vec<[f32; 3]>,
    pixels: Vec<u8>,
}

impl Frame {
    fn luma(&self, x: u32, y: u32) -> f32 {
        let c = self.rgb[(y * W + x) as usize];
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }
    /// Mean luma of a pixel rectangle.
    fn mean(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
        let mut sum = 0.0;
        let mut n = 0.0;
        for y in y0..y1 {
            for x in x0..x1 {
                sum += self.luma(x, y);
                n += 1.0;
            }
        }
        sum / n
    }
    fn max(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
        let mut best = 0.0f32;
        for y in y0..y1 {
            for x in x0..x1 {
                best = best.max(self.luma(x, y));
            }
        }
        best
    }
}

struct Cards<'a> {
    geometry: &'a Geometry,
    floor: &'a Geometry,
    floor_material: &'a LabMaterial,
    /// `(material, x offset, yaw)` per card; each card is its own mesh, material and object.
    cards: Vec<(&'a LabMaterial, f32, f32)>,
    sun_dir: [f32; 3],
    quality: FrameQuality,
}

fn render(device: &RenderDevice, c: &Cards<'_>) -> Frame {
    let post = post();
    let mut meshes = vec![FrameMesh {
        is_static: false,
        mesh_key: 1,
        material_key: 100,
        object_key: 200,
        vertices: &c.floor.0,
        indices: &c.floor.1,
        material: c.floor_material,
        model: Mat4::IDENTITY.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }];
    for (i, (material, x, yaw)) in c.cards.iter().enumerate() {
        let model = Mat4::from_translation(Vec3::new(*x, 0.0, 0.0)) * Mat4::from_rotation_y(*yaw);
        meshes.push(FrameMesh {
            is_static: false,
            mesh_key: 2,
            material_key: 101 + i as u64,
            object_key: 201 + i as u64,
            vertices: &c.geometry.0,
            indices: &c.geometry.1,
            material,
            model: model.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
    }
    let scene = FrameScene {
        width: W,
        height: H,
        eye: [0.0, 0.9, 3.2],
        target: [0.0, 0.8, 0.0],
        fov_y: 0.7,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: Some(&post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new(c.sun_dir, [1.0; 3], 50_000.0),
        local_lights: &[],
        quality: c.quality,
        meshes,
    };
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    let result = renderer.render_offscreen(device, &scene).expect("frame");
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("the post stack ran");
    Frame {
        rgb: hdr.rgb,
        pixels: result.pixels,
    }
}

fn write_png(path: &std::path::Path, pixels: &[u8]) {
    let file = std::fs::File::create(path).expect("create preview");
    let mut encoder = png::Encoder::new(file, W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png pixels");
    writer.finish().expect("png finish");
}

struct Fixture {
    geometry: Geometry,
    floor: Geometry,
    floor_material: LabMaterial,
}

fn fixture() -> Fixture {
    Fixture {
        geometry: card(0.7, 1.6, 8, 0.25),
        floor: floor(6.0, 0.0),
        floor_material: LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 1.0,
            ..LabMaterial::default()
        },
    }
}

fn cards<'a>(f: &'a Fixture, materials: Vec<(&'a LabMaterial, f32, f32)>) -> Cards<'a> {
    Cards {
        geometry: &f.geometry,
        floor: &f.floor,
        floor_material: &f.floor_material,
        cards: materials,
        // From the upper left behind the camera: the cards' faces catch a glancing specular.
        sun_dir: [0.4, -0.5, -0.75],
        quality: FrameQuality::LAB,
    }
}

/// From the upper right behind the camera: every card's shadow falls to its left.
const SUN_RIGHT: [f32; 3] = [-0.4, -0.5, -0.75];
/// Low from the upper left behind the camera: the half vector stands nearly square to the
/// strands, where the Kajiya-Kay lobes peak.
const SUN_LOW: [f32; 3] = [0.4, -0.2, -0.9];

/// `c` lit by a sun travelling along `sun_dir`.
fn lit(mut c: Cards<'_>, sun_dir: [f32; 3]) -> Cards<'_> {
    c.sun_dir = sun_dir;
    c
}

const LEFT: (u32, u32, u32, u32) = (0, 0, W / 2 - 4, H);
const RIGHT: (u32, u32, u32, u32) = (W / 2 + 4, 0, W, H);

fn different(a: &[[f32; 3]], b: &[[f32; 3]], x0: u32, x1: u32) -> usize {
    let mut n = 0;
    for y in 0..H {
        for x in x0..x1 {
            let i = (y * W + x) as usize;
            if a[i] != b[i] {
                n += 1;
            }
        }
    }
    n
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hair on one card moves its pixels and leaves a neighbouring hair-off card, and the floor,
/// byte for byte as they were.
///
/// The sun comes from the upper right here, so each card's shadow falls to its left: the left
/// card's own shadow is one of the pixels hair legitimately moves (a hair card casts through
/// the translucency maps, not the opaque cascades), and from the upper left it reached past
/// x = 100 into the right half this test holds still.
#[test]
fn pixels_change_only_for_materials_that_enable_hair() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let f = fixture();
    let off = hair_material(HairFactors::OFF);
    // Off with different tuning: a disabled material ignores every factor.
    let off_tuned = hair_material(HairFactors {
        primary_strength: 5.0,
        tip_color: [0.0; 3],
        tip_alpha: 0.0,
        transmission: 1.0,
        ..HairFactors::OFF
    });
    let on = hair_material(HairFactors::strands());

    let base = render(
        &device,
        &lit(cards(&f, vec![(&off, -0.9, 0.0), (&off, 0.9, 0.0)]), SUN_RIGHT),
    );
    let tuned = render(
        &device,
        &lit(
            cards(&f, vec![(&off_tuned, -0.9, 0.0), (&off, 0.9, 0.0)]),
            SUN_RIGHT,
        ),
    );
    assert_eq!(base.rgb, tuned.rgb, "a disabled hair block changed pixels");
    assert_eq!(base.pixels, tuned.pixels);

    let mixed = render(
        &device,
        &lit(cards(&f, vec![(&on, -0.9, 0.0), (&off, 0.9, 0.0)]), SUN_RIGHT),
    );
    let left = different(&base.rgb, &mixed.rgb, LEFT.0, LEFT.2);
    let right = different(&base.rgb, &mixed.rgb, RIGHT.0, RIGHT.2);
    assert!(left > 200, "hair on the left card changed only {left} pixels");
    assert_eq!(right, 0, "the hair-off card on the right changed {right} pixels");
}

/// The two Kajiya-Kay lobes make a highlight along the strand direction: the lit card's peak
/// is brighter than the same card without them, and zero strengths take it back to the base.
///
/// The sun sits low in front (upper left), where the half vector stands nearly square to the
/// strands (T·H about 0.14 on the primary lobe), so the lobe peaks on the card, and the floor
/// (n·l 0.2) stays darker than the lit card. With the fan's higher sun T·H was about 0.31
/// everywhere on the card (an exponent-120 lobe at 0.2 % of its peak) and the plain frame's
/// brightest pixel was the floor, so the peaks compared the lobed card to the floor.
#[test]
fn the_two_lobe_specular_adds_a_highlight() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let f = fixture();
    let plain = HairFactors {
        primary_strength: 0.0,
        secondary_strength: 0.0,
        transmission: 0.0,
        shift_noise: 0.0,
        ..HairFactors::strands()
    };
    let lobes = HairFactors {
        primary_strength: 1.5,
        secondary_strength: 1.0,
        ..plain
    };
    let m_plain = hair_material(plain);
    let m_lobes = hair_material(lobes);
    let a = render(&device, &lit(cards(&f, vec![(&m_plain, 0.0, 0.0)]), SUN_LOW));
    let b = render(&device, &lit(cards(&f, vec![(&m_lobes, 0.0, 0.0)]), SUN_LOW));
    let (x0, y0, x1, y1) = (W / 2 - 30, 10, W / 2 + 30, H - 10);
    let peak_plain = a.max(x0, y0, x1, y1);
    let peak_lobes = b.max(x0, y0, x1, y1);
    assert!(
        peak_lobes > peak_plain * 1.2,
        "lobes peak {peak_lobes}, without {peak_plain}"
    );
    assert!(b.mean(x0, y0, x1, y1) > a.mean(x0, y0, x1, y1));
}

/// The ramp darkens the tip: the card's top rows are darker than its root rows, and with a
/// flat ramp they are not.
#[test]
fn the_root_to_tip_ramp_darkens_the_tip() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let f = fixture();
    let flat = HairFactors {
        primary_strength: 0.0,
        secondary_strength: 0.0,
        shift_noise: 0.0,
        ..HairFactors::strands()
    };
    let ramped = HairFactors {
        tip_color: [0.15; 3],
        ..flat
    };
    let a = render(&device, &cards(&f, vec![(&hair_material(flat), 0.0, 0.0)]));
    let b = render(&device, &cards(&f, vec![(&hair_material(ramped), 0.0, 0.0)]));
    // The card spans roughly rows 25..105 of the frame; compare its upper and lower fifth.
    let col = (W / 2 - 14, W / 2 + 14);
    let tip = |fr: &Frame| fr.mean(col.0, 28, col.1, 44);
    let root = |fr: &Frame| fr.mean(col.0, 86, col.1, 102);
    let flat_ratio = tip(&a) / root(&a);
    let ramp_ratio = tip(&b) / root(&b);
    assert!(
        ramp_ratio < flat_ratio * 0.6,
        "ramp tip/root {ramp_ratio}, flat tip/root {flat_ratio}"
    );
}

/// Hashed alpha cuts the tip away (tip alpha 0 leaves the floor behind it), and the sorted
/// strategy draws the card through the blend pass with a soft tip.
#[test]
fn alpha_strategies_draw_the_card() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let f = fixture();
    let base = HairFactors {
        primary_strength: 0.0,
        secondary_strength: 0.0,
        ..HairFactors::strands()
    };
    let empty_scene = {
        let none: Vec<(&LabMaterial, f32, f32)> = Vec::new();
        render(&device, &cards(&f, none))
    };
    let solid = render(&device, &cards(&f, vec![(&hair_material(base), 0.0, 0.0)]));
    let vanishing = HairFactors {
        tip_alpha: 0.0,
        ..base
    };
    let hashed = render(&device, &cards(&f, vec![(&hair_material(vanishing), 0.0, 0.0)]));
    let tip_region = (W / 2 - 14, 28, W / 2 + 14, 44);
    let (x0, y0, x1, y1) = tip_region;
    let bg = empty_scene.mean(x0, y0, x1, y1);
    let card_tip = solid.mean(x0, y0, x1, y1);
    let cut_tip = hashed.mean(x0, y0, x1, y1);
    assert!(
        (card_tip - bg).abs() > bg * 0.05,
        "the card is visible at its tip: {card_tip} vs {bg}"
    );
    assert!(
        (cut_tip - bg).abs() < (card_tip - bg).abs() * 0.5,
        "a zero tip alpha leaves the tip mostly gone: {cut_tip} (bg {bg}, card {card_tip})"
    );

    let sorted_factors = HairFactors {
        alpha: HairAlpha::Sorted,
        tip_alpha: 0.3,
        ..base
    };
    let sorted_mat = hair_material(sorted_factors);
    assert_eq!(sorted_mat.alpha_mode, regia_render::ALPHA_BLEND);
    let sorted = render(&device, &cards(&f, vec![(&sorted_mat, 0.0, 0.0)]));
    let sorted_tip = sorted.mean(x0, y0, x1, y1);
    assert!(
        (sorted_tip - bg).abs() > bg * 0.01,
        "sorted card draws: {sorted_tip} vs {bg}"
    );
    // A sorted tip at 30 % alpha sits between the background and the solid hashed tip.
    assert!((sorted_tip - bg).abs() < (card_tip - bg).abs());

    // Coverage resolves to hashed on this build: identical pixels.
    let coverage = HairFactors {
        alpha: HairAlpha::Coverage,
        ..base
    };
    let cov = render(&device, &cards(&f, vec![(&hair_material(coverage), 0.0, 0.0)]));
    assert_eq!(cov.rgb, solid.rgb);
}

/// A hair card casts its shadow through its alpha test: a card of opaque strands shades the
/// floor behind it, a card whose tip alpha is 0 does not, and the card is not a square.
#[test]
fn a_card_casts_its_shadow_through_the_alpha_test() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let f = fixture();
    // Sun from above and slightly behind the cards, so the card's shadow falls forward on the
    // floor in front of it (toward the camera).
    let base = HairFactors {
        primary_strength: 0.0,
        secondary_strength: 0.0,
        ..HairFactors::strands()
    };
    let mut casting = hair_material(base);
    casting.albedo = Some(strand_texture(6, true));
    let mut clear = hair_material(HairFactors {
        root_alpha: 0.0,
        tip_alpha: 0.0,
        ..base
    });
    clear.albedo = Some(strand_texture(6, true));
    let mut scene = cards(&f, vec![(&casting, 0.0, 0.0)]);
    scene.sun_dir = [0.0, -0.8, 0.6];
    let shadowed = render(&device, &scene);
    let mut scene = cards(&f, vec![(&clear, 0.0, 0.0)]);
    scene.sun_dir = [0.0, -0.8, 0.6];
    let open = render(&device, &scene);
    // The floor in front of the card, below it on screen.
    let (x0, y0, x1, y1) = (W / 2 - 16, H - 14, W / 2 + 16, H - 4);
    let lit = open.mean(x0, y0, x1, y1);
    let dark = shadowed.mean(x0, y0, x1, y1);
    assert!(lit > 0.0);
    assert!(
        dark < lit * 0.8,
        "shadow under the card: {dark} vs open floor {lit}"
    );
}

/// The look-dev image: a fan of cards, hair off and on, and the PNGs of both when
/// `REGIA_PREVIEW=<dir>` is set. Assertions: the on image is lit and differs from the off one
/// where the cards are.
#[test]
fn look_dev_card_fan() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let f = fixture();
    let off = hair_material(HairFactors::OFF);
    let on = hair_material(HairFactors {
        tip_color: [0.35, 0.28, 0.22],
        tip_alpha: 0.35,
        ..HairFactors::strands()
    });
    fn fan(m: &LabMaterial) -> Vec<(&LabMaterial, f32, f32)> {
        vec![
            (m, -0.9, -0.5),
            (m, -0.45, -0.25),
            (m, 0.0, 0.0),
            (m, 0.45, 0.25),
            (m, 0.9, 0.5),
        ]
    }
    let a = render(&device, &cards(&f, fan(&off)));
    let b = render(&device, &cards(&f, fan(&on)));
    let mean_a = a.mean(0, 0, W, H);
    let mean_b = b.mean(0, 0, W, H);
    assert!(mean_a > 0.0 && mean_b > 0.0);
    assert!(different(&a.rgb, &b.rgb, 0, W) > 1_000);
    if let Ok(dir) = regia_core::brand::env_var("REGIA_PREVIEW") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).expect("preview dir");
        write_png(&dir.join("hair_fan_off.png"), &a.pixels);
        write_png(&dir.join("hair_fan_on.png"), &b.pixels);
    }
}
