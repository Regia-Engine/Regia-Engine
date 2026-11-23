// SPDX-License-Identifier: MPL-2.0
//! Look-dev frames for the character shading models (Tincal B3 skin, B4 eye, B6 fabric sheen):
//! each model on a sphere beside a standard control sphere, rendered with and without the
//! renderer's material shading table ([`FrameRenderer::set_material_shading`]).
//!
//! - Only the material that enables a model changes: the control sphere's pixels are equal
//!   byte for byte in every pair of frames.
//! - Skin: the lit side's terminator turns warmer (red scatters furthest), and a sphere lit
//!   from behind glows red through thin skin.
//! - Eye: the cornea adds a specular glint, and the refracted iris moves only iris pixels.
//! - Sheen: the rim brightens more than the centre (the Charlie lobe peaks at grazing angles).
//!
//! `REGIA_LOOKDEV_B64=1` prints each frame pair as a base64 PNG (`LOOKDEV <name> <data>`) so a
//! reviewer on another machine can look at them; no golden is stored.

use regia_core::glam::{Mat4, Vec3};
use regia_render::shading_model::{EyeShading, FabricSheen, MaterialShading, SkinShading};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabTexture,
    LabVertex, RenderDevice, ShadowCast,
};

const W: u32 = 192;
const H: u32 = 96;
const CONTROL_KEY: u64 = 11;
const FEATURE_KEY: u64 = 12;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip shading models: {err}");
            None
        }
    }
}

/// A UV sphere whose front (+z) hemisphere is planar-mapped: UV (0.5, 0.5) faces +z, u runs
/// along +x and v along −y, with matching tangents. Eyes are mapped this way.
fn front_mapped_sphere(radius: f32, segments: u32, rings: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let (mut vertices, indices) = regia_render::primitives::uv_sphere(radius, segments, rings);
    for v in &mut vertices {
        let n = Vec3::from(v.normal);
        v.uv = [0.5 + 0.5 * n.x, 0.5 - 0.5 * n.y];
        let t = (Vec3::X - n * n.dot(Vec3::X)).normalize_or(Vec3::Z.cross(n).normalize_or(Vec3::Y));
        // cross(n, t) is +y on the front; +v runs along −y.
        v.tangent = [t.x, t.y, t.z, -1.0];
    }
    (vertices, indices)
}

struct Stage {
    vertices: Vec<LabVertex>,
    indices: Vec<u32>,
    radius: f32,
}

fn stage(radius: f32) -> Stage {
    let (vertices, indices) = front_mapped_sphere(radius, 64, 48);
    Stage {
        vertices,
        indices,
        radius,
    }
}

/// Two spheres side by side (control on the left, feature on the right), each turned by `turn`
/// radians about +y, camera on +z at `distance`.
fn scene<'a>(
    stage: &'a Stage,
    control: &'a LabMaterial,
    feature: &'a LabMaterial,
    sun_dir: [f32; 3],
    distance: f32,
    turn: f32,
) -> FrameScene<'a> {
    let r = stage.radius;
    let eye = [0.0, 0.0, distance];
    let mesh = |key: u64, material: &'a LabMaterial, x: f32| FrameMesh {
        is_static: false,
        mesh_key: 1,
        material_key: key,
        object_key: key + 100,
        vertices: &stage.vertices,
        indices: &stage.indices,
        material,
        model: (Mat4::from_translation(Vec3::new(x, 0.0, 0.0)) * Mat4::from_rotation_y(turn))
            .to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    };
    let target = [0.0, 0.0, 0.0];
    FrameScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 2.0 * (1.3 * r / distance).atan(),
        near: r * 0.05,
        far: r * 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 4_000.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new(sun_dir, [1.0; 3], 60_000.0),
        local_lights: &[],
        quality: FrameQuality::default(),
        meshes: vec![
            mesh(CONTROL_KEY, control, -1.1 * r),
            mesh(FEATURE_KEY, feature, 1.1 * r),
        ],
    }
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, scene: &FrameScene<'_>) -> Vec<u8> {
    renderer
        .render_offscreen(device, scene)
        .expect("offscreen frame")
        .pixels
}

fn px(pixels: &[u8], x: u32, y: u32) -> [f64; 3] {
    let i = ((y * W + x) * 4) as usize;
    [
        f64::from(pixels[i]),
        f64::from(pixels[i + 1]),
        f64::from(pixels[i + 2]),
    ]
}

fn luma(c: [f64; 3]) -> f64 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Whether the left (control) half of two frames is byte-identical.
fn control_half_equal(a: &[u8], b: &[u8]) -> bool {
    (0..H).all(|y| {
        let row = (y * W * 4) as usize;
        let half = (W / 2 * 4) as usize;
        a[row..row + half] == b[row..row + half]
    })
}

/// Right-half pixels that differ between the frames.
fn feature_changes(a: &[u8], b: &[u8]) -> usize {
    let mut count = 0;
    for y in 0..H {
        for x in W / 2..W {
            if px(a, x, y) != px(b, x, y) {
                count += 1;
            }
        }
    }
    count
}

/// Pixel centre and radius of the feature sphere (right half), from the frame's projection.
fn feature_disc() -> ([f64; 2], f64) {
    // The camera looks at the origin across both spheres; the feature sphere's centre lands at
    // x = W/2 + W/4 · (1.1 / 1.3) of the half-width, y = H/2. Measured from the frame instead
    // of derived, the tests below only use the disc loosely (rim ring vs centre).
    let cx = f64::from(W) / 2.0 + f64::from(H) / 2.0 * (1.1 / 1.3);
    let cy = f64::from(H) / 2.0;
    let r = f64::from(H) / 2.0 / 1.3;
    ([cx, cy], r)
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Prints `frames` stacked vertically as one PNG when `REGIA_LOOKDEV_B64` is set.
fn lookdev(name: &str, frames: &[&[u8]]) {
    if regia_core::brand::env_var_os("REGIA_LOOKDEV_B64").is_none() {
        return;
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, W, H * frames.len() as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        let data: Vec<u8> = frames.iter().flat_map(|f| f.iter().copied()).collect();
        writer.write_image_data(&data).expect("png data");
    }
    println!("LOOKDEV {name} {}", base64(&bytes));
}

fn skin_material() -> LabMaterial {
    LabMaterial {
        base_color: [0.80, 0.52, 0.42, 1.0],
        roughness: 0.45,
        ..LabMaterial::default()
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn skin_warms_the_terminator_and_transmits_red_through_thin_skin() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    // A 15 mm sphere: an ear rim or a nose tip, where pre-integrated scattering shows.
    let stage = stage(0.015);
    let control = skin_material();
    let feature = skin_material();
    // The sun from +x and a little toward the camera: the terminator crosses the sphere.
    let side_sun = [-1.0, -0.15, -0.35];
    let mut renderer = FrameRenderer::new();
    let lit = scene(&stage, &control, &feature, side_sun, 0.11, 0.0);
    let off = render(&mut renderer, &device, &lit);
    // The measured profile scatters ~1 mm: at a 15 mm radius its terminator shift is under one
    // display level, so the look-dev frame softens the skin five times (a 3 mm effective radius).
    renderer.set_material_shading([(
        FEATURE_KEY,
        MaterialShading::skin(SkinShading {
            transmission: 0.0,
            scatter_scale: 5.0,
            ..SkinShading::default()
        }),
    )]);
    let on = render(&mut renderer, &device, &lit);
    assert_eq!(renderer.material_shading_count(), 1);
    assert!(
        control_half_equal(&off, &on),
        "the standard control sphere changed"
    );
    let changed = feature_changes(&off, &on);
    assert!(changed > 200, "skin changed only {changed} pixels");
    // Where the standard sphere falls to dark (the terminator band), skin is redder.
    let mut warm = 0.0;
    let mut count = 0usize;
    for y in 0..H {
        for x in W / 2..W {
            let a = px(&off, x, y);
            let b = px(&on, x, y);
            let l = luma(a);
            if l > 6.0 && l < 60.0 {
                warm += (b[0] - b[1]) - (a[0] - a[1]);
                count += 1;
            }
        }
    }
    assert!(count > 20, "no terminator band ({count} pixels)");
    let warm = warm / count as f64;
    eprintln!("skin terminator: {count} pixels, red − green gain {warm:.2}");
    assert!(
        warm > 0.5,
        "the skin terminator is not warmer (red − green gain {warm:.2})"
    );

    // Lit from behind (the sun travels toward the camera): thin skin (2 mm at a 15 mm radius)
    // passes red light.
    let back_sun = [0.15, -0.1, 1.0];
    let behind = scene(&stage, &control, &feature, back_sun, 0.11, 0.0);
    renderer.set_material_shading([(
        FEATURE_KEY,
        MaterialShading::skin(SkinShading {
            transmission: 0.0,
            ..SkinShading::default()
        }),
    )]);
    let dark = render(&mut renderer, &device, &behind);
    renderer.set_material_shading([(
        FEATURE_KEY,
        MaterialShading::skin(SkinShading {
            transmission: 1.0,
            thickness_scale: 2.0 / 15.0,
            ..SkinShading::default()
        }),
    )]);
    let glow = render(&mut renderer, &device, &behind);
    assert!(
        control_half_equal(&dark, &glow),
        "the control sphere changed under transmission"
    );
    let (mut red, mut green, mut n) = (0.0, 0.0, 0usize);
    for y in 0..H {
        for x in W / 2..W {
            let a = px(&dark, x, y);
            let b = px(&glow, x, y);
            if b != a {
                red += b[0] - a[0];
                green += b[1] - a[1];
                n += 1;
            }
        }
    }
    assert!(n > 200, "transmission changed only {n} pixels");
    eprintln!(
        "skin transmission: {n} pixels, red +{:.1}, green +{:.1}",
        red / n as f64,
        green / n as f64
    );
    assert!(
        red > 3.0 * green.max(0.0) && red / n as f64 > 2.0,
        "transmitted light is not red"
    );
    lookdev("skin", &[&off, &on, &dark, &glow]);
}

/// An eyeball texture: white sclera, a green-brown iris with radial fibres, a black pupil.
fn eye_texture() -> LabTexture {
    let size = 128u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let u = (x as f32 + 0.5) / size as f32 - 0.5;
            let v = (y as f32 + 0.5) / size as f32 - 0.5;
            let r = (u * u + v * v).sqrt();
            let a = v.atan2(u);
            let c: [u8; 3] = if r < 0.055 {
                [10, 10, 12]
            } else if r < 0.16 {
                let fibre = 0.75 + 0.25 * (a * 24.0).sin();
                let t = (r - 0.055) / 0.105;
                [
                    ((70.0 + 90.0 * t) * fibre) as u8,
                    ((110.0 + 40.0 * t) * fibre) as u8,
                    ((60.0 - 20.0 * t) * fibre) as u8,
                ]
            } else {
                [235, 228, 222]
            };
            rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

#[test]
fn the_eye_adds_a_cornea_glint_and_refracts_only_the_iris() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let stage = stage(0.012);
    let material = LabMaterial {
        base_color: [1.0; 4],
        roughness: 0.35,
        albedo: Some(eye_texture()),
        ..LabMaterial::default()
    };
    // Turned 35° away, so the recessed iris visibly shifts under the cornea.
    let sun = [-0.3, -0.5, -1.0];
    let shot = scene(&stage, &material, &material, sun, 0.09, 35f32.to_radians());
    let mut renderer = FrameRenderer::new();
    // Both spheres share one LabMaterial but draw under their own keys: only the feature key
    // takes the eye model.
    let standard = render(&mut renderer, &device, &shot);
    let flat_eye = EyeShading {
        iris_depth: 0.0,
        ..EyeShading::default()
    };
    renderer.set_material_shading([(FEATURE_KEY, MaterialShading::eye(flat_eye))]);
    let flat = render(&mut renderer, &device, &shot);
    renderer.set_material_shading([(FEATURE_KEY, MaterialShading::eye(EyeShading::default()))]);
    let refracted = render(&mut renderer, &device, &shot);
    assert!(control_half_equal(&standard, &flat) && control_half_equal(&flat, &refracted));
    // The wet cornea's glint: the brightest feature pixel is brighter than any standard one.
    let peak = |frame: &[u8]| {
        let mut best = 0.0f64;
        for y in 0..H {
            for x in W / 2..W {
                best = best.max(luma(px(frame, x, y)));
            }
        }
        best
    };
    let (p_std, p_eye) = (peak(&standard), peak(&flat));
    eprintln!("eye peak luma: standard {p_std:.1}, cornea {p_eye:.1}");
    assert!(p_eye > p_std + 15.0, "no cornea glint ({p_std:.1} → {p_eye:.1})");
    // Refraction moves the iris image only: pixels that read as sclera in the flat frame (bright,
    // neutral) and are away from the iris stay put.
    let mut iris_moved = 0usize;
    let mut sclera = 0usize;
    let mut sclera_moved = 0usize;
    for y in 0..H {
        for x in W / 2..W {
            let a = px(&flat, x, y);
            let b = px(&refracted, x, y);
            let neutral = (a[0] - a[2]).abs() < 30.0 && luma(a) > 150.0;
            if neutral {
                sclera += 1;
                if a != b {
                    sclera_moved += 1;
                }
            } else if a != b {
                iris_moved += 1;
            }
        }
    }
    eprintln!("eye refraction: {iris_moved} iris pixels moved; sclera {sclera_moved}/{sclera}");
    assert!(
        iris_moved > 30,
        "the refracted iris did not move ({iris_moved} pixels)"
    );
    assert!(sclera > 100, "no sclera in view ({sclera} pixels)");
    assert!(
        (sclera_moved as f64) < 0.05 * sclera as f64,
        "refraction changed {sclera_moved} of {sclera} sclera pixels"
    );
    lookdev("eye", &[&standard, &flat, &refracted]);
}

#[test]
fn sheen_brightens_the_rim_more_than_the_centre() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let stage = stage(0.25);
    let velvet = LabMaterial {
        base_color: [0.30, 0.04, 0.08, 1.0],
        roughness: 0.85,
        ..LabMaterial::default()
    };
    let sun = [-0.4, -0.6, -0.7];
    let shot = scene(&stage, &velvet, &velvet, sun, 1.8, 0.0);
    let mut renderer = FrameRenderer::new();
    let plain = render(&mut renderer, &device, &shot);
    renderer.set_material_shading([(
        FEATURE_KEY,
        MaterialShading::sheen(FabricSheen {
            color: [1.0, 0.55, 0.65],
            roughness: 0.4,
        }),
    )]);
    let sheen = render(&mut renderer, &device, &shot);
    assert!(control_half_equal(&plain, &sheen), "the control sphere changed");
    assert!(feature_changes(&plain, &sheen) > 500);
    let ([cx, cy], r) = feature_disc();
    let (mut rim, mut rim_n, mut centre, mut centre_n) = (0.0, 0usize, 0.0, 0usize);
    for y in 0..H {
        for x in W / 2..W {
            let d = ((f64::from(x) + 0.5 - cx).powi(2) + (f64::from(y) + 0.5 - cy).powi(2)).sqrt() / r;
            let gain = luma(px(&sheen, x, y)) - luma(px(&plain, x, y));
            if (0.75..0.92).contains(&d) {
                rim += gain;
                rim_n += 1;
            } else if d < 0.35 {
                centre += gain;
                centre_n += 1;
            }
        }
    }
    let (rim, centre) = (rim / rim_n.max(1) as f64, centre / centre_n.max(1) as f64);
    eprintln!("sheen gain: rim {rim:.1} ({rim_n} px), centre {centre:.1} ({centre_n} px)");
    assert!(rim_n > 50 && centre_n > 50);
    assert!(
        rim > centre + 3.0,
        "the sheen does not favour the rim (rim {rim:.1}, centre {centre:.1})"
    );
    lookdev("sheen", &[&plain, &sheen]);
}

#[test]
fn clearing_the_table_restores_the_standard_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let stage = stage(0.02);
    let control = skin_material();
    let feature = skin_material();
    let shot = scene(&stage, &control, &feature, [-1.0, -0.2, -0.4], 0.15, 0.0);
    let mut renderer = FrameRenderer::new();
    let before = render(&mut renderer, &device, &shot);
    renderer.set_material_shading([
        (FEATURE_KEY, MaterialShading::skin(SkinShading::default())),
        (CONTROL_KEY, MaterialShading::default()),
    ]);
    // Standard entries are dropped: only the skin entry is kept.
    assert_eq!(renderer.material_shading_count(), 1);
    let skin = render(&mut renderer, &device, &shot);
    assert_ne!(before, skin);
    renderer.set_material_shading(std::iter::empty());
    assert_eq!(renderer.material_shading_count(), 0);
    let after = render(&mut renderer, &device, &shot);
    assert_eq!(
        before, after,
        "clearing the table must restore the standard frame exactly"
    );
}
