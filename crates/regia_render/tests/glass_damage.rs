// SPDX-License-Identifier: MPL-2.0
//! Per-instance glass on the GPU (`regia_render::glass`, `frame/glass_gpu.rs`):
//!
//! - no glass instances (or instances for absent keys) leave a frame byte-identical;
//! - an own-canopy pane (`ViewerInside`) in front of a refracting pane shows no bend itself while
//!   the far pane, seen through it, still bends;
//! - crack pixels are brighter and whiter than intact glass, from outside and in ViewerInside;
//! - D3 holes of a failed pressure pane are see-through (the frame without the pane, pixel for
//!   pixel) and cast no shadow;
//! - a pressure pane at D2 keeps transmitting between its cracks (the inner pane holds);
//! - a crack stamped on a rotated, moved pane shows where the hit projects on screen.
//!
//! `REGIA_WRITE_GLASS_EVIDENCE=1` writes the frames to `docs/evidence/glass/`.

use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_core::glam::{Mat4, Quat, Vec3, Vec4};
use regia_render::glass::{GlassCrackSets, GlassDamage, GlassInstance, GlassView, apply_one, glass_instance};
use regia_render::{
    ALPHA_PREMULTIPLIED, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 256;
const H: u32 = 192;
const FOV_Y: f32 = 0.7;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn slab(half: [f32; 3], model: Mat4, material: usize) -> LabMesh {
    let (vertices, indices) = regia_render::box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: model.to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn glass_material(thickness: f32, tint: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [tint[0], tint[1], tint[2], 1.0],
        roughness: 0.05,
        transmission: 1.0,
        ior: 1.5,
        transmission_thickness: thickness,
        alpha_mode: ALPHA_PREMULTIPLIED,
        ..LabMaterial::default()
    }
}

/// Materials: 0 dark floor, 1 emissive bar, 2 thick glass, 3 thin glass, 4 white emissive floor,
/// 5 tinted thin glass, 6 lit grey floor.
fn materials() -> Vec<LabMaterial> {
    vec![
        LabMaterial {
            base_color: [0.02, 0.02, 0.02, 1.0],
            roughness: 1.0,
            ..LabMaterial::default()
        },
        LabMaterial {
            base_color: [0.0, 0.0, 0.0, 1.0],
            emissive: [1.0, 0.9, 0.8],
            emissive_intensity: 3000.0,
            roughness: 1.0,
            ..LabMaterial::default()
        },
        glass_material(0.8, [1.0; 3]),
        glass_material(0.0, [1.0; 3]),
        LabMaterial {
            base_color: [0.0, 0.0, 0.0, 1.0],
            emissive: [1.0, 1.0, 1.0],
            emissive_intensity: 2000.0,
            roughness: 1.0,
            ..LabMaterial::default()
        },
        glass_material(0.0, [0.55, 0.85, 0.65]),
        LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 1.0,
            ..LabMaterial::default()
        },
    ]
}

/// Floor at y = −1 with bright bars along z every 0.5 m (`bars`), or a plain floor of `floor`.
fn floor(floor_material: usize, bars: bool) -> Vec<LabMesh> {
    let mut meshes = vec![slab(
        [10.0, 0.005, 10.0],
        Mat4::from_translation(Vec3::new(0.0, -1.005, 0.0)),
        floor_material,
    )];
    if bars {
        for i in -10..=10 {
            meshes.push(slab(
                [0.125, 0.005, 10.0],
                Mat4::from_translation(Vec3::new(i as f32 * 0.5, -0.995, 0.0)),
                1,
            ));
        }
    }
    meshes
}

struct Shot {
    eye: [f32; 3],
    meshes: Vec<LabMesh>,
    sun: f32,
    shadows: bool,
    exposure: f32,
}

impl Shot {
    fn top(meshes: Vec<LabMesh>) -> Self {
        Self {
            eye: [0.0, 4.0, 0.01],
            meshes,
            sun: 0.0,
            shadows: false,
            exposure: 12.0,
        }
    }

    fn scene(&self) -> LabScene {
        LabScene {
            width: W,
            height: H,
            eye: self.eye,
            target: [0.0, 0.0, 0.0],
            fov_y: FOV_Y,
            near: 0.05,
            far: 100.0,
            depth_prepass: true,
            directional_shadows: self.shadows,
            ibl: false,
            exposure_ev: self.exposure,
            ambient_lux: if self.sun > 0.0 { 3000.0 } else { 0.0 },
            double_sided: false,
            post: Some(self.post()),
            directional: DirectionalLightUniform::new([0.25, -0.9, 0.3], [1.0, 1.0, 1.0], self.sun),
            local_lights: Vec::new(),
            materials: materials(),
            meshes: self.meshes.clone(),
        }
    }

    fn post(&self) -> PostSettings {
        PostSettings {
            exposure_ev: self.exposure,
            ..PostSettings::golden_deterministic()
        }
    }

    /// Renders with refraction on/off; `glass` builds the instances from the frame's object keys.
    fn render(&self, refraction: bool, glass: impl Fn(&[u64]) -> Option<Vec<GlassInstance>>) -> Vec<u8> {
        let scene = self.scene();
        let post = self.post();
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = false;
        // The tests project world points with `FOV_Y` as the vertical field.
        quality.horizontal_plus = false;
        quality.screen_space_refraction = refraction;
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        let keys: Vec<u64> = frame.meshes.iter().map(|m| m.object_key).collect();
        let mut renderer = FrameRenderer::new();
        if let Some(instances) = glass(&keys) {
            renderer.set_glass(instances);
        }
        renderer
            .render_offscreen(&device(), &frame)
            .expect("frame")
            .pixels
    }
}

/// A damaged instance of `mesh` (object `key`) with `impacts` = (local point, energy, level, size).
fn damaged(
    key: u64,
    mesh: &LabMesh,
    set: &str,
    impacts: &[(Vec3, f32, u8, f32)],
    inside: bool,
) -> GlassInstance {
    let sets = GlassCrackSets::default();
    let mut damage = GlassDamage::new(set);
    for &(point, energy, level, size) in impacts {
        damage.impact_at_level(point, energy, level, size);
    }
    let model = Mat4::from_cols_array_2d(&mesh.model);
    let cooked = sets.get(set).expect("built-in set");
    apply_one(&mut damage, cooked, &mesh.vertices, &mesh.indices, 1, model);
    let view = if inside {
        GlassView::viewer_inside()
    } else {
        GlassView::default()
    };
    glass_instance(key, Some(&view), Some(&damage), &sets, model).expect("instance")
}

fn px(img: &[u8], x: u32, y: u32) -> [f32; 3] {
    let i = ((y * W + x) * 4) as usize;
    [f32::from(img[i]), f32::from(img[i + 1]), f32::from(img[i + 2])]
}

fn luma(img: &[u8], x: u32, y: u32) -> f32 {
    let p = px(img, x, y);
    0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]
}

/// The integer x shift that best maps `unbent` onto `bent` over a window.
fn shift_in(bent: &[u8], unbent: &[u8], xs: std::ops::Range<u32>, ys: std::ops::Range<u32>) -> i32 {
    let mut best = (f32::MAX, 0i32);
    for s in -20i32..=20 {
        let mut ssd = 0.0;
        for y in ys.clone() {
            for x in xs.clone() {
                let xs = (x as i32 + s).clamp(0, W as i32 - 1) as u32;
                let d = luma(bent, x, y) - luma(unbent, xs, y);
                ssd += d * d;
            }
        }
        if ssd < best.0 - 1e-3 {
            best = (ssd, s);
        }
    }
    best.1
}

/// Screen pixel of a world point for a [`Shot`]'s camera (unjittered).
fn project(eye: [f32; 3], world: Vec3) -> (f32, f32) {
    let view = look_at_mat4(Vec3::from_array(eye), Vec3::ZERO, Vec3::Y);
    let proj = perspective(FOV_Y, W as f32 / H as f32, 0.05, 100.0);
    let clip = proj * view * Vec4::new(world.x, world.y, world.z, 1.0);
    let ndc = clip.truncate() / clip.w;
    ((ndc.x * 0.5 + 0.5) * W as f32, (0.5 - ndc.y * 0.5) * H as f32)
}

fn write_evidence(name: &str, img: &[u8]) {
    if regia_core::brand::env_var("REGIA_WRITE_GLASS_EVIDENCE")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/evidence/glass");
    std::fs::create_dir_all(&dir).expect("evidence dir");
    let file = std::fs::File::create(dir.join(name)).expect("png create");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(img).expect("png data");
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn no_glass_instances_leave_the_frame_byte_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut meshes = floor(0, true);
    meshes.push(slab([1.2, 0.005, 1.2], Mat4::from_rotation_z(0.4), 2));
    let shot = Shot::top(meshes);
    for refraction in [false, true] {
        let plain = shot.render(refraction, |_| None);
        assert!(
            shot.render(refraction, |_| Some(Vec::new())) == plain,
            "empty glass list changed the frame"
        );
        // An instance whose key is not in the frame changes nothing either.
        let stray = |_: &[u64]| {
            Some(vec![GlassInstance {
                object_key: 0xDEAD_BEEF,
                inside: true,
                specular_scale: 0.15,
                damage: None,
            }])
        };
        assert!(
            shot.render(refraction, stray) == plain,
            "a stray glass instance changed the frame"
        );
    }
}

#[test]
fn an_own_canopy_does_not_bend_but_glass_seen_through_it_still_does() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The far pane (thick, tilted) sits at y = 0 over x in −0.6..0.6; the canopy (thick, tilted
    // the other way) at y = 2 covers the left half of the view and the far pane's left part.
    let mut meshes = floor(0, true);
    meshes.push(slab([0.6, 0.005, 1.5], Mat4::from_rotation_z(0.4), 2));
    meshes.push(slab(
        [0.9, 0.005, 1.5],
        Mat4::from_translation(Vec3::new(-0.75, 2.0, 0.0)) * Mat4::from_rotation_z(-0.4),
        2,
    ));
    let canopy = meshes.len() - 1;
    let shot = Shot::top(meshes);
    let inside = |keys: &[u64]| {
        Some(vec![GlassInstance {
            object_key: keys[canopy],
            inside: true,
            specular_scale: 0.15,
            damage: None,
        }])
    };
    let unbent = shot.render(false, inside);
    let canopy_inside = shot.render(true, inside);
    let canopy_outside = shot.render(true, |_| None);
    write_evidence("canopy_inside.png", &canopy_inside);
    write_evidence("canopy_outside.png", &canopy_outside);
    // Screen columns: the canopy covers x < W/2; the far pane about W/2 ± 38 px.
    let (left, right) = project(shot.eye, Vec3::new(-0.6, 0.0, 0.0));
    let (pane_l, pane_r) = (left as u32, project(shot.eye, Vec3::new(0.6, 0.0, 0.0)).0 as u32);
    let _ = right;
    let rows = H / 2 - 20..H / 2 + 20;
    let canopy_only = 6..pane_l.saturating_sub(16).max(20);
    let both = pane_l + 6..W / 2 - 14;
    let pane_only = W / 2 + 14..pane_r - 4;
    let s_inside = shift_in(&canopy_inside, &unbent, canopy_only.clone(), rows.clone());
    let s_outside = shift_in(&canopy_outside, &unbent, canopy_only.clone(), rows.clone());
    eprintln!("canopy-only columns {canopy_only:?}: inside shift {s_inside}, exterior shift {s_outside}");
    assert_eq!(s_inside, 0, "an own-canopy pane must not bend what is behind it");
    assert!(s_outside != 0, "the same pane from outside must bend (control)");
    let s_through = shift_in(&canopy_inside, &unbent, both.clone(), rows.clone());
    let s_far = shift_in(&canopy_inside, &unbent, pane_only.clone(), rows.clone());
    eprintln!(
        "far pane through the canopy {both:?}: shift {s_through}; far pane alone {pane_only:?}: {s_far}"
    );
    assert!(s_far != 0, "the far pane must bend (control)");
    assert!(
        s_through != 0,
        "the far pane seen through the own canopy must still bend"
    );
}

/// Mean luma and the chroma of the brightest tenth over the pixels within `r` px of `c`.
fn region_stats(img: &[u8], c: (f32, f32), r: f32) -> (f32, f32) {
    let mut lumas: Vec<(f32, f32)> = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let (dx, dy) = (x as f32 + 0.5 - c.0, y as f32 + 0.5 - c.1);
            if dx * dx + dy * dy <= r * r {
                let p = px(img, x, y);
                let max = p[0].max(p[1]).max(p[2]);
                let min = p[0].min(p[1]).min(p[2]);
                lumas.push((luma(img, x, y), (max - min) / max.max(1.0)));
            }
        }
    }
    let mean = lumas.iter().map(|l| l.0).sum::<f32>() / lumas.len().max(1) as f32;
    lumas.sort_by(|a, b| b.0.total_cmp(&a.0));
    let top = &lumas[..(lumas.len() / 10).max(1)];
    let chroma = top.iter().map(|l| l.1).sum::<f32>() / top.len() as f32;
    (mean, chroma)
}

#[test]
fn cracks_are_brighter_and_whiter_than_intact_glass_outside_and_inside() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut meshes = floor(0, false);
    meshes.push(slab([1.2, 0.005, 1.2], Mat4::IDENTITY, 3));
    let pane = meshes.len() - 1;
    let mut shot = Shot::top(meshes);
    shot.sun = 60_000.0;
    shot.exposure = 14.0;
    let mesh = shot.meshes[pane].clone();
    let hit = Vec3::new(0.2, 0.005, -0.1);
    let centre = project(shot.eye, hit);
    let exterior = shot.render(false, |_| None);
    let inside_intact = shot.render(false, |keys| {
        Some(vec![GlassInstance {
            object_key: keys[pane],
            inside: true,
            specular_scale: 0.15,
            damage: None,
        }])
    });
    for (level, set) in [(2u8, "laminate"), (2, "pressure_pane")] {
        for inside in [false, true] {
            // Against the intact pane in the same view mode (inside lowers the glare).
            let intact = if inside { &inside_intact } else { &exterior };
            let cracked = shot.render(false, |keys| {
                Some(vec![damaged(
                    keys[pane],
                    &mesh,
                    set,
                    &[(hit, 5.0, level, 0.8)],
                    inside,
                )])
            });
            write_evidence(&format!("cracks_{set}_inside_{inside}.png"), &cracked);
            // 0.8 m stamp ≈ 53 px across at this distance: the inner 20 px radius.
            let (m0, _) = region_stats(intact, centre, 20.0);
            let (m1, chroma) = region_stats(&cracked, centre, 20.0);
            eprintln!(
                "{set} inside={inside}: mean luma intact {m0:.1}, cracked {m1:.1}; crack chroma {chroma:.3}"
            );
            assert!(
                m1 > m0 * 1.5 + 10.0,
                "{set} inside={inside}: cracks must brighten the glass"
            );
            assert!(
                chroma < 0.25,
                "{set} inside={inside}: the brightest crack pixels must be near white"
            );
        }
    }
}

#[test]
fn failed_pressure_pane_holes_are_see_through_and_cast_no_shadow() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Lit grey floor with sun shadows; a tinted pane at y = 0 fails (D3) around its centre.
    let pane_mesh = slab([1.2, 0.005, 1.2], Mat4::IDENTITY, 5);
    let mut with_pane = floor(6, false);
    with_pane.push(pane_mesh.clone());
    let pane = with_pane.len() - 1;
    let mut shot = Shot::top(with_pane);
    shot.sun = 40_000.0;
    shot.shadows = true;
    shot.exposure = 14.0;
    let mut no_pane = Shot::top(floor(6, false));
    no_pane.sun = shot.sun;
    no_pane.shadows = true;
    no_pane.exposure = shot.exposure;
    let bare = no_pane.render(false, |_| None);
    let failed = shot.render(false, |keys| {
        Some(vec![damaged(
            keys[pane],
            &pane_mesh,
            "pressure_pane",
            &[(Vec3::new(0.0, 0.005, 0.0), 20.0, 3, 1.6)],
            false,
        )])
    });
    let intact = shot.render(false, |_| None);
    write_evidence("pressure_d3.png", &failed);
    let centre = project(shot.eye, Vec3::ZERO);
    // The hole: well inside the stamp's mean radius (0.6 m of the 1.6 m footprint ≈ 190/512).
    // The intact pane around it casts a translucent shadow (sun 25° off vertical, 0.43 m across
    // the 1 m drop to the floor), so only floor seen through the hole whose sun ray also
    // crosses the hole must match the frame without the pane: the hole casts nothing.
    let sun = Vec3::new(0.25, -0.9, 0.3).normalize();
    let inv = (perspective(FOV_Y, W as f32 / H as f32, 0.05, 100.0)
        * look_at_mat4(Vec3::from_array(shot.eye), Vec3::ZERO, Vec3::Y))
    .inverse();
    let sun_through_hole = |x: u32, y: u32| {
        let ndc = Vec4::new(
            (x as f32 + 0.5) / W as f32 * 2.0 - 1.0,
            1.0 - (y as f32 + 0.5) / H as f32 * 2.0,
            0.5,
            1.0,
        );
        let p = inv * ndc;
        let eye = Vec3::from_array(shot.eye);
        let dir = (p.truncate() / p.w - eye).normalize();
        let floor = eye + dir * ((-1.0 - eye.y) / dir.y);
        let crossing = floor - sun * (1.0 / -sun.y);
        crossing.x * crossing.x + crossing.z * crossing.z < 0.38 * 0.38
    };
    let (mut same, mut total) = (0, 0);
    for y in 0..H {
        for x in 0..W {
            let (dx, dy) = (x as f32 + 0.5 - centre.0, y as f32 + 0.5 - centre.1);
            if dx * dx + dy * dy < 25.0 * 25.0 && sun_through_hole(x, y) {
                total += 1;
                let (a, b) = (px(&failed, x, y), px(&bare, x, y));
                if (0..3).all(|c| (a[c] - b[c]).abs() <= 2.0) {
                    same += 1;
                }
            }
        }
    }
    eprintln!("hole: {same}/{total} pixels match the frame without the pane");
    assert!(total > 150, "too few hole pixels lit through the hole: {total}");
    assert!(
        same as f32 > 0.97 * total as f32,
        "a hole must show exactly what is behind the pane"
    );
    // Control: the intact tinted pane changes the same pixels.
    let (mut differ, mut n) = (0, 0);
    for y in centre.1 as u32 - 10..centre.1 as u32 + 10 {
        for x in centre.0 as u32 - 10..centre.0 as u32 + 10 {
            n += 1;
            let (a, b) = (px(&intact, x, y), px(&bare, x, y));
            if (0..3).any(|c| (a[c] - b[c]).abs() > 4.0) {
                differ += 1;
            }
        }
    }
    assert!(
        differ > n / 2,
        "the intact tinted pane must change what shows through it (control)"
    );
}

#[test]
fn a_pressure_pane_at_d2_keeps_transmitting_between_its_cracks() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pane_mesh = slab([1.2, 0.005, 1.2], Mat4::IDENTITY, 3);
    let mut meshes = floor(4, false);
    meshes.push(pane_mesh.clone());
    let pane = meshes.len() - 1;
    let shot = Shot::top(meshes);
    let bare = Shot::top(floor(4, false)).render(false, |_| None);
    let hit = Vec3::new(0.0, 0.005, 0.0);
    let d2 = shot.render(false, |keys| {
        Some(vec![damaged(
            keys[pane],
            &pane_mesh,
            "pressure_pane",
            &[(hit, 6.0, 2, 1.2)],
            false,
        )])
    });
    write_evidence("pressure_d2.png", &d2);
    let centre = project(shot.eye, hit);
    let (mut clear, mut total) = (0, 0);
    for y in 0..H {
        for x in 0..W {
            let (dx, dy) = (x as f32 + 0.5 - centre.0, y as f32 + 0.5 - centre.1);
            if dx * dx + dy * dy < 40.0 * 40.0 {
                total += 1;
                if luma(&d2, x, y) >= 0.6 * luma(&bare, x, y) {
                    clear += 1;
                }
            }
        }
    }
    eprintln!("D2 pressure pane: {clear}/{total} pixels transmit at least 60 % of the backdrop");
    assert!(
        clear as f32 > 0.6 * total as f32,
        "the inner pane must stay clear behind the cracked outer pane"
    );
}

#[test]
fn a_crack_on_a_rotated_moved_pane_shows_where_the_hit_projects() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let model = Mat4::from_rotation_translation(Quat::from_rotation_y(0.6), Vec3::new(0.35, 0.0, -0.2));
    let pane_mesh = slab([0.9, 0.005, 0.6], model, 3);
    let mut meshes = floor(0, false);
    meshes.push(pane_mesh.clone());
    let pane = meshes.len() - 1;
    let mut shot = Shot::top(meshes);
    shot.eye = [0.0, 3.2, 2.4];
    shot.sun = 60_000.0;
    shot.exposure = 14.0;
    let local = Vec3::new(0.45, 0.005, 0.2);
    let world = model.transform_point3(local);
    let expected = project(shot.eye, world);
    let intact = shot.render(false, |_| None);
    let cracked = shot.render(false, |keys| {
        Some(vec![damaged(
            keys[pane],
            &pane_mesh,
            "laminate",
            &[(local, 1.0, 1, 0.12)],
            false,
        )])
    });
    write_evidence("stamp_rotated.png", &cracked);
    // Centroid of the brightened pixels.
    let (mut sx, mut sy, mut sw) = (0.0f32, 0.0f32, 0.0f32);
    for y in 0..H {
        for x in 0..W {
            let gain = luma(&cracked, x, y) - luma(&intact, x, y);
            if gain > 20.0 {
                sx += (x as f32 + 0.5) * gain;
                sy += (y as f32 + 0.5) * gain;
                sw += gain;
            }
        }
    }
    assert!(sw > 0.0, "no crack visible");
    let got = (sx / sw, sy / sw);
    eprintln!("crack centroid {got:?}, hit projects to {expected:?}");
    assert!(
        (got.0 - expected.0).abs() < 3.0 && (got.1 - expected.1).abs() < 3.0,
        "crack at {got:?}, hit at {expected:?}"
    );
}
