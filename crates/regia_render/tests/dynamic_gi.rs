// SPDX-License-Identifier: MPL-2.0
//! Dynamic GI tiers (`specs/79-dynamic-gi.md`, ADR-1679; `specs/27` § 4.2 stage 2a).
//!
//! The SDF-traced probes with visibility must keep light out of a closed room at every tier,
//! not only at Desktop High's: the Deck tier has fewer, coarser-reaching cascades and re-traces
//! a settled probe half as often, and leaks are the failure mode of probe GI (`specs/27` § 2.2:
//! interior ≤ 0.5 % of the exterior ground). The tier switch must resize the field without a
//! device, keep the host's own fields, and leave High equal to the probes' defaults so frames
//! that never name a tier do not change.

use regia_core::glam::Mat4;
use regia_render::{
    DdgiSettings, DirectionalLightUniform, DynamicGiTier, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, RenderSettings, box_mesh,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip dynamic_gi: {err}");
            None
        }
    }
}

const W: u32 = 160;
const H: u32 = 96;

fn block(centre: [f32; 3], half: [f32; 3]) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material: 0,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn grey(v: f32) -> LabMaterial {
    LabMaterial {
        base_color: [v, v, v, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

/// A tier's probe field as the test traces it: every probe re-traced every frame (so the field
/// converges in the frames a test can afford on lavapipe) and bricks baked on the caller. The
/// probe layout (cascades, spacing, probes per axis, bricks) is the tier's.
fn tier_settings(tier: DynamicGiTier) -> DdgiSettings {
    let mut s = DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DdgiSettings::default()
    };
    tier.apply(&mut s);
    s.update_divisor = 1;
    s
}

fn sun(elevation_deg: f32, lux: f32) -> DirectionalLightUniform {
    let e = elevation_deg.to_radians();
    let az = 0.6f32;
    DirectionalLightUniform::new(
        [-(e.cos() * az.cos()), -e.sin(), -(e.cos() * az.sin())],
        [1.0; 3],
        lux,
    )
}

/// Mean BT.709 luma of a pixel rectangle, decoded to linear light.
fn linear_luma(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let lin = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * lin(pixels[i]) + 0.7152 * lin(pixels[i + 1]) + 0.0722 * lin(pixels[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

fn scene(eye: [f32; 3], target: [f32; 3], meshes: Vec<LabMesh>) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 14.5;
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post),
        directional: sun(50.0, 100_000.0),
        local_lights: Vec::new(),
        materials: vec![grey(0.6)],
        meshes,
    }
}

/// Renders `frames` frames and returns the last one.
fn converge(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, frames: u32) -> Vec<u8> {
    let mut last = Vec::new();
    for _ in 0..frames {
        let frame = lab.as_frame_scene_with_quality(quality());
        last = renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels;
    }
    last
}

/// Ground slab plus a 6 × 3 × 6 m room with walls and roof `2 t` thick, sealed.
fn sealed_room(t: f32) -> Vec<LabMesh> {
    sealed_room_of_height(t, 3.0)
}

/// [`sealed_room`], `h` metres tall.
fn sealed_room_of_height(t: f32, h: f32) -> Vec<LabMesh> {
    let wh = h / 2.0;
    vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0]),
        block([3.0 + t, wh, 0.0], [t, wh, 3.0 + 2.0 * t]),
        block([-3.0 - t, wh, 0.0], [t, wh, 3.0 + 2.0 * t]),
        block([0.0, wh, 3.0 + t], [3.0, wh, t]),
        block([0.0, wh, -3.0 - t], [3.0, wh, t]),
        block([0.0, h + t, 0.0], [3.0 + 2.0 * t, t, 3.0 + 2.0 * t]),
    ]
}

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Exterior ground and interior floor luma of the sealed room with walls `2 t` thick, traced
/// with `tier`'s probe layout. The interior is the steady state: a renderer whose probes were
/// placed with the eye inside (`specs/27` § 2.2 scores a closed box, not a walk-in). `None`
/// without a device.
fn sealed_room_luma(tier: DynamicGiTier, t: f32) -> Option<(f32, f32)> {
    let device = device()?;
    let outside = scene([10.0, 2.5, 10.0], [6.0, 0.0, 6.0], sealed_room(t));
    let inside = scene([0.0, 1.6, -1.5], [0.0, 0.0, 1.5], sealed_room(t));
    let mut out_renderer = FrameRenderer::new();
    out_renderer.set_ddgi_settings(tier_settings(tier));
    out_renderer.set_ddgi_enabled(Some(true));
    let out_px = converge(&mut out_renderer, &device, &outside, 40);
    assert!(out_renderer.ddgi_active(), "{tier:?}: the probes run");
    let exterior = linear_luma(&out_px, 40, 50, 120, 90);
    drop(out_renderer);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(tier_settings(tier));
    renderer.set_ddgi_enabled(Some(true));
    let in_px = converge(&mut renderer, &device, &inside, 100);
    let interior = linear_luma(&in_px, 40, 50, 120, 90);
    let status = renderer.dynamic_gi_status();
    eprintln!(
        "{tier:?} walls {:.2} m: exterior ground {exterior:.4}, interior floor {interior:.6} ({:.3} %), \
         probes {}",
        2.0 * t,
        interior / exterior.max(1e-9) * 100.0,
        status.budget.probes
    );
    Some((exterior, interior))
}

#[test]
fn a_sealed_room_stays_dark_inside_at_the_deck_tier() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((exterior, interior)) = sealed_room_luma(DynamicGiTier::Low, 0.1) else {
        return;
    };
    assert!(exterior > 0.1, "the exterior ground must be lit ({exterior})");
    // specs/27 § 2.2: ≤ 0.5 % of the exterior ground.
    assert!(
        interior <= 0.005 * exterior,
        "light leaks into the sealed room at the Low tier: {interior:.5} vs {exterior:.4} ({:.2} %)",
        interior / exterior * 100.0
    );
}

#[test]
fn a_sealed_room_with_thin_walls_stays_dark_at_the_medium_tier() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // 10 cm walls: a twentieth of the finest probe spacing.
    let Some((exterior, interior)) = sealed_room_luma(DynamicGiTier::Medium, 0.05) else {
        return;
    };
    assert!(exterior > 0.1, "the exterior ground must be lit ({exterior})");
    assert!(
        interior <= 0.005 * exterior,
        "light leaks through 10 cm walls at the Medium tier: {interior:.5} vs {exterior:.4} ({:.2} %)",
        interior / exterior * 100.0
    );
}

/// Luma of the sealed room's floor, ceiling and walls over the exterior ground, each from the same
/// eye, with the probes placed inside (`sealed_room_luma`). The floor alone hides the leak: at the
/// old placement the Low tier read 6.8 % on the floor and 15.2 % on the ceiling (ADR-1949). `None`
/// without a device.
fn sealed_room_surfaces(tier: DynamicGiTier, t: f32) -> Option<[(&'static str, f32); 4]> {
    room_surfaces(tier, 3.0, &|| sealed_room(t))
}

/// `sealed_room_surfaces` for any 6 m wide room `h` metres tall.
fn room_surfaces(
    tier: DynamicGiTier,
    h: f32,
    room: &dyn Fn() -> Vec<LabMesh>,
) -> Option<[(&'static str, f32); 4]> {
    room_surfaces_with(&format!("{tier:?}"), &tier_settings(tier), h, room)
}

/// `room_surfaces` under any probe settings (`name` labels the printout).
fn room_surfaces_with(
    name: &str,
    settings: &DdgiSettings,
    h: f32,
    room: &dyn Fn() -> Vec<LabMesh>,
) -> Option<[(&'static str, f32); 4]> {
    let device = device()?;
    let outside = scene([10.0, 2.5, 10.0], [6.0, 0.0, 6.0], room());
    let mut out_renderer = FrameRenderer::new();
    out_renderer.set_ddgi_settings(settings.clone());
    out_renderer.set_ddgi_enabled(Some(true));
    let out_px = converge(&mut out_renderer, &device, &outside, 40);
    let exterior = linear_luma(&out_px, 40, 50, 120, 90);
    assert!(exterior > 0.1, "the exterior ground must be lit ({exterior})");
    drop(out_renderer);
    let eye = [0.0, 1.6, -1.5];
    let views = [
        ("floor", [0.0, 0.0, 1.5]),
        ("ceiling", [0.0, h, 1.5]),
        ("wall", [3.0, 1.6, -1.5]),
        ("upper wall", [3.0, h - 0.2, -1.5]),
    ];
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(settings.clone());
    renderer.set_ddgi_enabled(Some(true));
    let mut out = [("", 0.0); 4];
    for (i, (name, target)) in views.into_iter().enumerate() {
        // The eye holds still, so the probes keep converging while the view turns.
        let lab = scene(eye, target, room());
        let px = converge(&mut renderer, &device, &lab, if i == 0 { 100 } else { 3 });
        out[i] = (name, linear_luma(&px, 40, 20, 120, 76) / exterior);
    }
    eprintln!("{name} room {h} m tall: {out:?}");
    Some(out)
}

/// `specs/27` § 2.2 on every surface of the room at the shipped layouts, not only on the floor.
/// Before ADR-1949 the upper walls and ceiling read the 18 and 54 m cascades.
#[test]
fn every_surface_of_a_sealed_room_stays_dark_at_the_deck_and_desktop_high_tiers() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for tier in [DynamicGiTier::Low, DynamicGiTier::High] {
        let Some(surfaces) = sealed_room_surfaces(tier, 0.1) else {
            return;
        };
        for (name, share) in surfaces {
            assert!(
                share <= 0.005,
                "light leaks onto the {name} at {tier:?}: {:.2} % of the exterior ground",
                share * 100.0
            );
        }
    }
}

/// `specs/27` § 2.2 on every surface of a sealed room 6 m tall (ADR-2605, `specs/79` § 9.2). Its
/// ceiling lies 4.4 m over the eye, above the finest cascade's full-weight span (2 m), where the
/// blend window handed it to the 6 m cascade, whose probes stand outside the roof: before the
/// visibility blend the ceiling read 10.9 % at Low and the upper wall 7.7 %.
#[test]
fn every_surface_of_a_6_m_sealed_room_stays_dark_at_the_deck_tier() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(surfaces) = room_surfaces(DynamicGiTier::Low, 6.0, &|| sealed_room_of_height(0.1, 6.0)) else {
        return;
    };
    for (name, share) in surfaces {
        assert!(
            share <= 0.005,
            "light leaks onto the {name} of the 6 m room at Low: {:.2} % of the exterior ground",
            share * 100.0
        );
    }
}

/// The same 6 m room on High's layout with the visibility blend (on since ADR-5001). On ADR-5001's ten 1 m
/// layers the ceiling read 2.16 %: it lay past the finest cascade's blend window, where the 3.8 m cascade
/// lit it alone; twelve layers holding 3.5 m over the eye read 0.0002 % (ADR-5290, RTX 5090).
#[test]
fn the_visibility_blend_keeps_a_6_m_room_dark_on_the_desktop_high_layout() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let settings = DdgiSettings {
        visibility_blend: true,
        ..tier_settings(DynamicGiTier::High)
    };
    let Some(surfaces) = room_surfaces_with("High + visibility blend", &settings, 6.0, &|| {
        sealed_room_of_height(0.1, 6.0)
    }) else {
        return;
    };
    for (name, share) in surfaces {
        assert!(
            share <= 0.005,
            "light leaks onto the {name} of the 6 m room at High with the visibility blend: {:.2} %",
            share * 100.0
        );
    }
}

/// A room like the sealed one with a 1.6 m wide opening in its +z wall, `h` metres tall.
fn room_with_door(t: f32, h: f32) -> Vec<LabMesh> {
    let wh = h / 2.0;
    vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0]),
        block([3.0 + t, wh, 0.0], [t, wh, 3.0 + 2.0 * t]),
        block([-3.0 - t, wh, 0.0], [t, wh, 3.0 + 2.0 * t]),
        block([0.0, wh, -3.0 - t], [3.0, wh, t]),
        block([-1.9, wh, 3.0 + t], [1.1, wh, t]),
        block([1.9, wh, 3.0 + t], [1.1, wh, t]),
        block([0.0, h + t, 0.0], [3.0 + 2.0 * t, t, 3.0 + 2.0 * t]),
    ]
}

/// The floor of the room with a door, with the eye inside, under `settings`.
fn door_room_floor(device: &RenderDevice, settings: DdgiSettings) -> f32 {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(settings);
    renderer.set_ddgi_enabled(Some(true));
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.0, 1.5], room_with_door(0.1, 3.0));
    let px = converge(&mut renderer, device, &lab, 100);
    linear_luma(&px, 40, 50, 120, 90)
}

/// The fix must keep light out of closed rooms without starving open ones: a room with a door
/// still takes its daylight. The reference is the 1 m, 2-cascade layout, which never leaked.
#[test]
fn a_door_still_lets_the_daylight_into_the_room() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let fine = door_room_floor(
        &device,
        DdgiSettings {
            spacing: 1.0,
            cascades: 2,
            dims: [12, 8, 12],
            ..tier_settings(DynamicGiTier::Low)
        },
    );
    for tier in [DynamicGiTier::Low, DynamicGiTier::High] {
        let floor = door_room_floor(&device, tier_settings(tier));
        eprintln!("door room floor: {tier:?} {floor:.5}, 1 m layout {fine:.5}");
        assert!(fine > 0.0005, "the reference room is lit ({fine})");
        assert!(
            floor > 0.5 * fine && floor < 2.0 * fine,
            "{tier:?} reads {floor:.5} on the floor of a room with a door, the 1 m layout {fine:.5}"
        );
    }
}

/// [`room_with_door`] 3 m tall with a lintel over the door: the opening is 1.6 m wide and 2.2 m tall, so the
/// wall runs on over it and the ceiling by the door is hidden from outside. The door wall's outer side
/// wears a 20 cm cladding of material 1 (the caller's second material), which the sun lights.
fn room_with_clad_lintel_door(t: f32) -> Vec<LabMesh> {
    let (h, door_h) = (3.0f32, 2.2f32);
    let mut meshes = room_with_door(t, h);
    let lintel = (h - door_h) / 2.0;
    meshes.push(block([0.0, door_h + lintel, 3.0 + t], [0.8, lintel, t]));
    let clad = |centre: [f32; 3], half: [f32; 3]| LabMesh {
        material: 1,
        ..block(centre, half)
    };
    let z = 3.0 + 2.0 * t + 0.1;
    meshes.push(clad([-1.9, h / 2.0, z], [1.1, h / 2.0, 0.1]));
    meshes.push(clad([1.9, h / 2.0, z], [1.1, h / 2.0, 0.1]));
    meshes.push(clad([0.0, door_h + lintel, z], [0.8, lintel, 0.1]));
    meshes
}

/// Mean linear RGB of a pixel rectangle.
fn linear_rgb(pixels: &[u8], rect: [u32; 4]) -> [f32; 3] {
    let lin = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let mut sum = [0.0f32; 3];
    let mut n = 0.0;
    for y in rect[1]..rect[3] {
        for x in rect[0]..rect[2] {
            let i = ((y * W + x) * 4) as usize;
            for (c, s) in sum.iter_mut().enumerate() {
                *s += lin(pixels[i + c]);
            }
            n += 1.0;
        }
    }
    sum.map(|s| s / n)
}

/// The room with a clad lintel door from inside, with the static-screen cache off (`door_room_settled`):
/// the mean linear RGB of the door wall's inner face beside the door and of the ceiling strip along that
/// wall. Neither takes direct light: the sun travels away from the wall's inner face and the roof covers
/// the ceiling, so what they show is the probes' light. Everything is grey but the cladding on the wall's
/// outer side, which is red.
fn door_wall_and_ceiling(device: &RenderDevice, settings: DdgiSettings) -> [[f32; 3]; 2] {
    let mut q = quality();
    q.paged_sun_shadows = false;
    assert!(!q.reuse_static_screen());
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(settings);
    renderer.set_ddgi_enabled(Some(true));
    let eye = [0.0, 1.6, -1.5];
    // The wall's inner face right of the door (the door ends at x = 0.8), then the ceiling 0.5 m from it.
    let views = [
        ([1.9, 1.5, 3.0], [62, 28, 98, 68]),
        ([0.0, 3.0, 2.5], [55, 45, 105, 51]),
    ];
    let mut out = [[0.0; 3]; 2];
    for (i, (target, rect)) in views.into_iter().enumerate() {
        let mut lab = scene(eye, target, room_with_clad_lintel_door(0.1));
        lab.materials.push(LabMaterial {
            base_color: [0.85, 0.05, 0.05, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        });
        let mut px = Vec::new();
        // The eye holds still, so the probes keep converging while the view turns.
        for _ in 0..if i == 0 { 120 } else { 3 } {
            let frame = lab.as_frame_scene_with_quality(q);
            px = renderer
                .render_offscreen(device, &frame)
                .unwrap_or_else(|err| panic!("frame: {err}"))
                .pixels;
        }
        out[i] = linear_rgb(&px, rect);
    }
    out
}

/// ADR-5566: on the Low tier's 2 m probes the probe outside a door stands a metre behind the door wall's
/// plane and sees that wall's inner face through the door, and it stands under no roof and sees the
/// ceiling by the door past the lintel. Its light is the sunlit ground's and the wall's sunlit outer
/// side. The wall's outer side is red here and nothing else is: without the front weights the grey inner
/// face of that wall turns red, the color of a surface it cannot see (red over green 2.17 on an RTX 5090;
/// 0.90 on the 1 m, 2-cascade layout, whose probes do not straddle the wall). With them the red excess falls
/// to 0.36 of that (1.42 red over green: what is left comes over the floor inside the door, which the
/// outside probe still shares), and the ceiling strip loses the outside probe's share (luma 0.081 to 0.057;
/// 0.029 on the 1 m layout). Where the lookup blends the probes in a lightness space (ADR-5506) the leak's
/// tint is smaller to begin with (excess 0.54 without the option) and the option leaves 0.58 of it, so the
/// bound is a quarter less red, which either lookup meets and an option that does nothing does not. The
/// inner face itself reads darker than the 1 m layout with or without the leak (2 m probes do not resolve
/// the sun patch at its foot), so its brightness only has a floor here.
#[test]
fn the_probe_outside_a_door_does_not_light_the_rooms_wall_and_ceiling_on_the_low_tier() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let low = tier_settings(DynamicGiTier::Low);
    assert!(
        low.front_weights && low.edge_visibility,
        "the Low tier takes the front weights and the edge visibility"
    );
    let with = door_wall_and_ceiling(&device, low.clone());
    let without = door_wall_and_ceiling(
        &device,
        DdgiSettings {
            front_weights: false,
            edge_visibility: false,
            ..low.clone()
        },
    );
    let fine = door_wall_and_ceiling(
        &device,
        DdgiSettings {
            spacing: 1.0,
            cascades: 2,
            dims: [12, 8, 12],
            front_weights: false,
            edge_visibility: false,
            ..low
        },
    );
    // How much redder than green a patch is: 0 for grey light on the grey wall.
    let red = |rgb: [f32; 3]| rgb[0] / rgb[1].max(1e-6) - 1.0;
    let luma = |rgb: [f32; 3]| 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    eprintln!(
        "door wall inner face (rgb, red excess): Low {:?} {:.3}, without the front weights {:?} {:.3}, 1 m layout {:?} {:.3}",
        with[0],
        red(with[0]),
        without[0],
        red(without[0]),
        fine[0],
        red(fine[0])
    );
    eprintln!(
        "ceiling strip by the door (luma): Low {:.5}, without the front weights {:.5}, 1 m layout {:.5}",
        luma(with[1]),
        luma(without[1]),
        luma(fine[1])
    );
    assert!(
        luma(fine[0]) > 0.0005 && luma(fine[1]) > 0.0005,
        "the reference room is lit: {fine:?}"
    );
    // The wall's inner face: the outer side's color is the leak's mark.
    assert!(
        red(without[0]) > red(fine[0]) + 0.05,
        "without the front weights the inner face should carry the outer side's red (the test's premise): \
         {} against the 1 m layout's {}",
        red(without[0]),
        red(fine[0])
    );
    assert!(
        red(with[0]) < 0.75 * red(without[0]),
        "the door wall's inner face keeps the red of its outer side: red excess {} with the front weights, {} \
         without, {} on the 1 m layout",
        red(with[0]),
        red(without[0]),
        red(fine[0])
    );
    assert!(
        luma(with[0]) > 0.15 * luma(fine[0]),
        "the door wall's inner face lost the room's own light: {} against the 1 m layout's {}",
        luma(with[0]),
        luma(fine[0])
    );
    // The ceiling strip: the outside probe's downward face holds the sunlit ground.
    let (c_with, c_without, c_fine) = (luma(with[1]), luma(without[1]), luma(fine[1]));
    assert!(
        c_with < 0.9 * c_without,
        "the ceiling strip by the door keeps the outside probe's light: {c_with} with the front weights, \
         {c_without} without"
    );
    assert!(
        (c_with - c_fine).abs() < (c_without - c_fine).abs(),
        "the ceiling strip is no nearer the 1 m layout's {c_fine}: {c_with} with the front weights, {c_without} \
         without"
    );
    assert!(
        c_with > 0.5 * c_fine,
        "the ceiling strip lost the room's own light: {c_with} against the 1 m layout's {c_fine}"
    );
}

/// ADR-5566: the front weights change nothing where every probe that sees a point stands in front of its
/// surface and before it: open ground with a free-standing block. Two renderers trace the same rays, so
/// the frames differ only by the weights.
#[test]
fn the_front_weights_leave_open_ground_alone() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let mut q = quality();
    q.paged_sun_shadows = false;
    let meshes = || {
        vec![
            block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0]),
            block([0.0, 1.0, 0.0], [1.0, 1.0, 1.0]),
        ]
    };
    let lab = scene([6.0, 3.0, 6.0], [0.0, 1.0, 0.0], meshes());
    let frames = |front_weights: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_settings(DdgiSettings {
            front_weights,
            edge_visibility: front_weights,
            ..tier_settings(DynamicGiTier::Low)
        });
        renderer.set_ddgi_enabled(Some(true));
        let mut px = Vec::new();
        for _ in 0..60 {
            let frame = lab.as_frame_scene_with_quality(q);
            px = renderer
                .render_offscreen(&device, &frame)
                .unwrap_or_else(|err| panic!("frame: {err}"))
                .pixels;
        }
        px
    };
    let with = frames(true);
    let without = frames(false);
    let (a, b) = (linear_luma(&with, 0, 0, W, H), linear_luma(&without, 0, 0, W, H));
    let worst = with
        .iter()
        .zip(&without)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0);
    eprintln!(
        "open ground: mean luma {a:.5} with the front weights, {b:.5} without; worst channel step {worst}"
    );
    assert!(b > 0.01, "the frame is lit ({b})");
    assert!(
        (a - b).abs() < 0.01 * b,
        "the front weights moved open ground: {a} against {b}"
    );
    assert!(worst <= 8, "a pixel moved by {worst} of 255 on open ground");
}

/// ADR-5566, the cover word (probe record word 11, `ddgi_common.wgsl` `ddgi_cover_word`): five bits per cube
/// direction, how much of the probe's distance lobe toward the surface behind that face ends within reach. In
/// the room with a door a probe under the ceiling holds the ceiling behind its downward face and a probe before
/// the door wall holds that wall; the probe outside the door, under the sky, holds no ceiling. Both hold the
/// ground under them. A layout without the front weights writes no cover word.
#[test]
fn a_probe_under_a_roof_holds_its_cover_and_one_under_the_sky_holds_none() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    const COVER_WORD: usize = 11;
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.0, 1.5], room_with_door(0.1, 3.0));
    let covers = |front_weights: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_settings(DdgiSettings {
            front_weights,
            edge_visibility: front_weights,
            ..tier_settings(DynamicGiTier::Low)
        });
        renderer.set_ddgi_enabled(Some(true));
        converge(&mut renderer, &device, &lab, 40);
        let rb = renderer
            .ddgi_read_back(&device)
            .unwrap_or_else(|err| panic!("read back: {err}"))
            .unwrap_or_else(|| panic!("the probes ran"));
        // The finest cascade's probes at (0, 2, 0) m in the room, (2, 2, 2) m before the door wall and
        // (0, 2, 4) m outside the door: cells of 2 m.
        [[0, 1, 0], [1, 1, 1], [0, 1, 2]].map(|cell| {
            let slot = rb.slot(0, cell);
            let (offset, tag) = rb.probe(slot);
            assert_eq!([tag[0], tag[1], tag[2]], cell, "the slot holds the cell");
            assert!(offset[3] > 0.5, "the probe at {cell:?} is in the open");
            let word = rb.data[slot as usize * PROBE_WORDS + COVER_WORD];
            // +x −x +y −y +z −z.
            std::array::from_fn::<u32, 6, _>(|j| (word >> (5 * j)) & 31)
        })
    };
    let [room, by_wall, outside] = covers(true);
    eprintln!(
        "cover (+x -x +y -y +z -z): room {room:?}, before the door wall {by_wall:?}, outside {outside:?}"
    );
    // −y: the surface behind the face lies over the probe.
    assert!(room[3] >= 29, "the room's probe holds its ceiling: {room:?}");
    assert!(
        by_wall[3] >= 29,
        "the probe before the wall holds its ceiling: {by_wall:?}"
    );
    assert!(outside[3] <= 2, "the probe outside holds no ceiling: {outside:?}");
    // −z: the surface behind the face lies toward +z, the door wall a metre from (2, 2, 2).
    assert!(
        by_wall[5] >= 29,
        "the probe before the door wall holds it: {by_wall:?}"
    );
    assert!(
        outside[5] <= 2,
        "the probe outside has open ground at its back: {outside:?}"
    );
    // +y: every one of them stands 2 m over the ground.
    for cover in [room, by_wall, outside] {
        assert!(cover[2] >= 29, "a probe 2 m over the ground holds it: {cover:?}");
    }
    assert_eq!(
        covers(false),
        [[0; 6]; 3],
        "a layout without the front weights keeps word 11 as it was"
    );
}

/// ADR-5796: the frame's first row and first column read the probes as the rows and columns beside them do.
/// The screen lookup takes a texel's normal from the depth of the pixels around its own. A texel of the frame's
/// first row or column has no pixel above or left of its own, and that missing neighbor was the texel's own
/// pixel reconstructed a second time and subtracted: an exact zero where both evaluations round alike
/// (lavapipe), a rounding remainder where they do not (an NVIDIA card), and the normal's rule took that
/// remainder for the nearer neighbor. Such a texel's normal pointed anywhere. In a room on the Low tier the 2 m
/// cell around the ceiling holds the probes over the roof, so single texels of the first row read the daylight
/// outside (display value 240 beside 185 in the Low door room, 83 beside 10 in a sealed box).
///
/// Counted: the pixels of the first row (column) that differ from the pixel two rows (columns) in, the next
/// texel of the half-resolution lookup, against the same count one texel further in, which holds what a crease
/// or an edge between two rows may differ by.
#[test]
fn the_frames_first_row_and_column_read_the_probes_as_the_rows_beside_them() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let luma = |px: &[u8], x: u32, y: u32| linear_luma(px, x, y, x + 1, y + 1);
    let differs = |a: f32, b: f32| (a - b).abs() > 0.25 * a.max(b) + 0.002;
    for (name, tier, meshes) in [
        ("sealed room, Low", DynamicGiTier::Low, sealed_room(0.1)),
        (
            "room with a door, Low",
            DynamicGiTier::Low,
            room_with_door(0.1, 3.0),
        ),
        (
            "room with a door, High",
            DynamicGiTier::High,
            room_with_door(0.1, 3.0),
        ),
    ] {
        // The ceiling fills the frame's first rows and the −x wall its first columns.
        let lab = scene([0.0, 1.6, -1.5], [0.0, 1.4, 1.5], meshes);
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_settings(tier_settings(tier));
        renderer.set_ddgi_enabled(Some(true));
        let px = converge(&mut renderer, &device, &lab, 48);
        assert!(renderer.ddgi_active(), "{name}: the probes run");
        let rows = |y: u32| {
            (0..W)
                .filter(|&x| differs(luma(&px, x, y), luma(&px, x, y + 2)))
                .count()
        };
        let columns = |x: u32| {
            (0..H)
                .filter(|&y| differs(luma(&px, x, y), luma(&px, x + 2, y)))
                .count()
        };
        let (row, row_in, column, column_in) = (rows(0), rows(2), columns(0), columns(2));
        eprintln!(
            "{name}: {row} of {W} pixels of the first row differ from the row two in ({row_in} one texel \
             further in); {column} of {H} pixels of the first column ({column_in} further in)"
        );
        // A crease that crosses the first rows moves by a pixel or two from one row to the next; the defect
        // put a wrong texel under every second to fourth pixel of the row.
        assert!(
            row <= row_in + 8,
            "{name}: {row} pixels of the frame's first row read other light than the row two in ({row_in} \
             one texel further in)"
        );
        assert!(
            column <= column_in + 6,
            "{name}: {column} pixels of the frame's first column read other light than the column two in \
             ({column_in} one texel further in)"
        );
    }
}

/// ADR-5796: a crease near the eye holds no lone texels on the Low tier. The screen lookup's normal used to be
/// built from the nearer depth neighbor on each axis whatever surface each lay on. At a crease one may be the
/// wall's and the other the roof's: their cross product points along the crease, no probe stands behind that
/// normal's plane, and on 2 m probes the probes over the roof passed the front weight. Single lookup texels along
/// the roof's creases read the daylight outside (three times their neighbors' light), and single texels along
/// the floor's read the cell under the floor (half).
///
/// The scene is `gi_acceptance`'s tunnel (16 m long, 3 m wide and tall inside, open at both ends, the eye inside
/// looking at a mouth) at 480 × 288. Counted: the pixels brighter than 1.6 times every one of the eight pixels two
/// away from them, or darker than 0.55 times every one. A crease or an edge is a line and has pixels of its own
/// kind two away along it; a lone lookup texel (two pixels wide) has none.
#[test]
fn a_crease_near_the_eye_holds_no_lone_texels_on_the_low_tier() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    const CW: u32 = 480;
    const CH: u32 = 288;
    let wall = |centre: [f32; 3], half: [f32; 3]| LabMesh {
        material: 1,
        ..block(centre, half)
    };
    let surface = |rgb: [f32; 3]| LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        roughness: 0.95,
        ..LabMaterial::default()
    };
    let mut lab = scene(
        [-2.0, 1.5, 0.6],
        [6.0, 1.0, -0.3],
        vec![
            block([0.0, -0.25, 0.0], [40.0, 0.25, 40.0]),
            wall([0.0, 1.5, 1.6], [8.0, 1.5, 0.1]),
            wall([0.0, 1.5, -1.6], [8.0, 1.5, 0.1]),
            wall([0.0, 3.1, 0.0], [8.0, 0.1, 1.7]),
        ],
    );
    lab.width = CW;
    lab.height = CH;
    lab.materials = vec![surface([0.4; 3]), surface([0.55, 0.52, 0.48])];
    lab.directional = DirectionalLightUniform::new([-0.45, -0.819, -0.355], [1.0, 0.97, 0.92], 100_000.0);
    lab.ambient_lux = 8_000.0;
    lab.exposure_ev = 11.5;
    if let Some(post) = lab.post.as_mut() {
        post.exposure_ev = 11.5;
    }
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(tier_settings(DynamicGiTier::Low));
    renderer.set_ddgi_enabled(Some(true));
    let px = converge(&mut renderer, &device, &lab, 48);
    assert!(renderer.ddgi_active(), "the probes run");
    assert_eq!(px.len(), (CW * CH * 4) as usize, "the frame is {CW} × {CH}");
    let decode = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let luma = |x: u32, y: u32| {
        let i = ((y * CW + x) * 4) as usize;
        0.2126 * decode(px[i]) + 0.7152 * decode(px[i + 1]) + 0.0722 * decode(px[i + 2])
    };
    let (mut bright, mut dark) = (Vec::new(), Vec::new());
    for y in 2..CH - 2 {
        for x in 2..CW - 2 {
            let c = luma(x, y);
            let (mut hi, mut lo) = (0.0f32, f32::MAX);
            for (ox, oy) in [(0, 0), (2, 0), (4, 0), (0, 2), (4, 2), (0, 4), (2, 4), (4, 4)] {
                let v = luma(x + ox - 2, y + oy - 2);
                hi = hi.max(v);
                lo = lo.min(v);
            }
            if c > 1.6 * hi + 0.004 {
                bright.push((x, y));
            } else if c < 0.55 * lo - 0.004 {
                dark.push((x, y));
            }
        }
    }
    eprintln!(
        "Low tunnel at {CW} × {CH}: {} lone bright pixels {:?}, {} lone dark pixels {:?}",
        bright.len(),
        &bright[..bright.len().min(16)],
        dark.len(),
        &dark[..dark.len().min(16)]
    );
    assert!(
        bright.len() + dark.len() <= 3,
        "{} lone bright and {} lone dark pixels along the tunnel's creases (first: {:?} {:?})",
        bright.len(),
        dark.len(),
        bright.first(),
        dark.first()
    );
}

#[test]
fn the_tier_switch_resizes_the_probes_and_keeps_the_hosts_fields() {
    // No device: the switch is CPU state until the next frame.
    let mut renderer = FrameRenderer::new();
    assert_eq!(renderer.dynamic_gi_tier(), DynamicGiTier::High);
    // A fresh renderer holds the High tier's settings (ADR-5001: no longer the defaults' layout).
    assert_eq!(*renderer.ddgi_settings(), DynamicGiTier::High.settings());
    assert!(!renderer.dynamic_gi_status().custom);
    assert!(
        !renderer.set_dynamic_gi_tier(DynamicGiTier::High),
        "High is already applied"
    );
    // A host's own biases survive a tier change; its own sizes are replaced by the tier's.
    renderer.set_ddgi_settings(DdgiSettings {
        normal_bias: 0.2,
        dims: [8, 8, 8],
        ..DdgiSettings::default()
    });
    let status = renderer.dynamic_gi_status();
    assert!(status.custom, "the host's sizes are not High's");
    assert!(!status.active, "no frame has run");
    assert!(renderer.set_dynamic_gi_tier(DynamicGiTier::Low));
    let s = renderer.ddgi_settings().clone();
    assert_eq!(s.dims, [12, 8, 12]);
    assert_eq!(s.update_divisor, 16);
    assert!((s.normal_bias - 0.2).abs() < f32::EPSILON);
    let status = renderer.dynamic_gi_status();
    assert_eq!(status.tier, DynamicGiTier::Low);
    assert!(!status.custom);
    assert_eq!(status.budget, DynamicGiTier::Low.budget());
    // The same tier again is a no-op, so a host's later settings survive the runtime's calls.
    renderer.set_ddgi_settings(DdgiSettings {
        hysteresis: 0.95,
        ..s
    });
    assert!(!renderer.set_dynamic_gi_tier(DynamicGiTier::Low));
    assert!((renderer.ddgi_settings().hysteresis - 0.95).abs() < f32::EPSILON);
    assert!(renderer.set_dynamic_gi_tier(DynamicGiTier::Ultra));
    assert_eq!(renderer.ddgi_settings().max_instances_per_cascade, 128);
}

#[test]
fn presets_carry_their_tier_and_the_player_overrides_it() {
    let deck = RenderSettings::from_profile(RenderProfilePreset::Deck);
    assert_eq!(deck.profile.gi_tier, DynamicGiTier::Low);
    assert_eq!(deck.effective_dynamic_gi_tier(), DynamicGiTier::Low);
    let high = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    assert_eq!(high.effective_dynamic_gi_tier(), DynamicGiTier::High);
    let ultra = RenderSettings::from_profile(RenderProfilePreset::DesktopUltra);
    assert_eq!(ultra.effective_dynamic_gi_tier(), DynamicGiTier::Ultra);
    let cinematic = RenderProfile::from_preset(RenderProfilePreset::Cinematic);
    assert_eq!(cinematic.gi_tier, DynamicGiTier::Ultra);
    let mut chosen = deck;
    chosen.dynamic_gi_tier = Some(DynamicGiTier::Medium);
    assert_eq!(chosen.effective_dynamic_gi_tier(), DynamicGiTier::Medium);
    // A player's tier does not make the profile a custom one.
    assert_eq!(chosen.preset(), Some(RenderProfilePreset::Deck));
}

/// Frame cost per tier on this device (lavapipe for correctness runs; a real GPU for budgets):
/// `cargo test -p regia_render --test dynamic_gi -- --ignored --nocapture tier_cost`. Prints the
/// probe update's GPU time per stage (when the device has timestamp queries) and the CPU time
/// of a whole frame, settled, at each tier's real update divisor. No time is asserted.
#[test]
#[ignore = "measurement: prints per-tier cost, asserts no time"]
fn tier_cost() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let frames: u32 = regia_core::brand::env_var_os("DYNAMIC_GI_COST_FRAMES")
        .and_then(|v| v.to_str().and_then(|s| s.parse().ok()))
        .unwrap_or(48);
    let outside = scene([10.0, 2.5, 10.0], [6.0, 0.0, 6.0], sealed_room(0.1));
    eprintln!(
        "tier,probes,rays_per_frame,gpu_ms_mean,trace,relocate,irradiance,distance,lookup,cpu_ms_mean,off_cpu_ms_mean"
    );
    let run = |renderer: &mut FrameRenderer| -> (f32, [f32; 5], f32) {
        // Settle first (new probes trace every frame for 8 updates), then measure.
        converge(renderer, &device, &outside, 24);
        let mut gpu = 0.0;
        let mut stages = [0.0f32; 5];
        let mut cpu = 0.0;
        for _ in 0..frames {
            let t0 = std::time::Instant::now();
            converge(renderer, &device, &outside, 1);
            cpu += t0.elapsed().as_secs_f32() * 1000.0;
            let t = renderer.gpu_timings();
            gpu += t.gi_ms;
            for (a, b) in stages.iter_mut().zip(t.gi_stages_ms) {
                *a += b;
            }
        }
        let n = frames as f32;
        (gpu / n, stages.map(|s| s / n), cpu / n)
    };
    let mut off = FrameRenderer::new();
    off.set_gpu_timing(&device, true);
    off.set_ddgi_enabled(Some(false));
    let (_, _, off_cpu) = run(&mut off);
    for tier in DynamicGiTier::ALL {
        let mut renderer = FrameRenderer::new();
        renderer.set_gpu_timing(&device, true);
        let mut s = DdgiSettings {
            bake_on_caller: true,
            ..DdgiSettings::default()
        };
        tier.apply(&mut s);
        renderer.set_ddgi_settings(s);
        renderer.set_ddgi_enabled(Some(true));
        let (gpu, st, cpu) = run(&mut renderer);
        let b = tier.budget();
        eprintln!(
            "{},{},{},{gpu:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{cpu:.1},{off_cpu:.1}",
            tier.name(),
            b.probes,
            b.rays_per_frame,
            st[0],
            st[1],
            st[2],
            st[3],
            st[4]
        );
    }
}

/// Diagnostics for the sealed room: interior/exterior with the probes off, then on at a 1 m and
/// a 2 m finest spacing (the tiers' layouts otherwise).
/// `cargo test -p regia_render --test dynamic_gi -- --ignored --nocapture sealed_room_diagnostics`.
#[test]
#[ignore = "diagnostics: prints probe states, asserts nothing"]
fn sealed_room_diagnostics() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let outside = scene([10.0, 2.5, 10.0], [6.0, 0.0, 6.0], sealed_room(0.1));
    let inside = scene([0.0, 1.6, -1.5], [0.0, 0.0, 1.5], sealed_room(0.1));
    /// Probe spacing (m), cascades and probes per axis; `None` turns the probes off.
    type Layout = Option<(f32, u32, [u32; 3])>;
    let cases: [(&str, Layout); 4] = [
        ("off", None),
        ("1m 2c 12x8x12", Some((1.0, 2, [12, 8, 12]))),
        ("2m 2c 12x8x12", Some((2.0, 2, [12, 8, 12]))),
        ("2m 4c 12x8x12 (Low)", Some((2.0, 4, [12, 8, 12]))),
    ];
    for (name, case) in cases {
        let make = || {
            let mut renderer = FrameRenderer::new();
            match case {
                None => renderer.set_ddgi_enabled(Some(false)),
                Some((spacing, cascades, dims)) => {
                    renderer.set_ddgi_settings(DdgiSettings {
                        spacing,
                        cascades,
                        dims,
                        ..tier_settings(DynamicGiTier::Low)
                    });
                    renderer.set_ddgi_enabled(Some(true));
                }
            }
            renderer
        };
        // Walk in: the probes first settle with the eye outside, then follow it in.
        let mut renderer = make();
        let out_px = converge(&mut renderer, &device, &outside, 40);
        let exterior = linear_luma(&out_px, 40, 50, 120, 90);
        let mut walked = Vec::new();
        for frames in [60, 60, 120] {
            let px = converge(&mut renderer, &device, &inside, frames);
            walked.push(linear_luma(&px, 40, 50, 120, 90) / exterior.max(1e-9) * 100.0);
        }
        // Steady state: the probes placed with the eye inside.
        let mut fresh = make();
        let px = converge(&mut fresh, &device, &inside, 100);
        let steady = linear_luma(&px, 40, 50, 120, 90) / exterior.max(1e-9) * 100.0;
        eprintln!(
            "[{name}] exterior {exterior:.4}; walked in, interior % after 60/120/240 frames {:.3} / {:.3} / {:.3}; \
             placed inside, after 100 frames {steady:.3} %",
            walked[0], walked[1], walked[2]
        );
    }
}

/// The rooms the sealed box says nothing about: one taller than the finest cascade's span (its
/// ceiling reads coarser cascades, so only the ray rule protects the rest of the room), and one
/// with a door, which must keep its daylight. Every surface, as shares of the exterior ground.
/// `cargo test -p regia_render --test dynamic_gi -- --ignored --nocapture room_matrix`.
#[test]
#[ignore = "diagnostics: prints every surface of the tall and door rooms, asserts nothing"]
fn room_matrix() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for tier in [DynamicGiTier::Low, DynamicGiTier::High] {
        for (name, h, room) in [
            ("sealed 6 m, 20 cm", 6.0, sealed_room_of_height(0.1, 6.0)),
            ("sealed 6 m, 10 cm", 6.0, sealed_room_of_height(0.05, 6.0)),
            ("door 3 m, 20 cm", 3.0, room_with_door(0.1, 3.0)),
            ("door 6 m, 20 cm", 6.0, room_with_door(0.1, 6.0)),
        ] {
            let Some(surfaces) = room_surfaces(tier, h, &|| room.clone()) else {
                return;
            };
            let shares = surfaces.map(|(surface, share)| format!("{surface} {:.3} %", share * 100.0));
            eprintln!("MATRIX {tier:?} [{name}] {}", shares.join(", "));
        }
    }
}

/// ADR-2605 for the owner: what High's layout reads with the visibility blend and seeding switched
/// on (High keeps both off), on the rooms of `room_matrix`.
/// `cargo test -p regia_render --test dynamic_gi -- --ignored --nocapture opt_in_matrix`.
#[test]
#[ignore = "diagnostics: prints High with ADR-2605's options on, asserts nothing"]
fn opt_in_matrix() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let settings = DdgiSettings {
        visibility_blend: true,
        seed_updates: regia_render::dynamic_gi::LOW_SEED_UPDATES,
        ..tier_settings(DynamicGiTier::High)
    };
    for (name, h, room) in [
        ("sealed 3 m, 20 cm", 3.0, sealed_room_of_height(0.1, 3.0)),
        ("sealed 6 m, 20 cm", 6.0, sealed_room_of_height(0.1, 6.0)),
        ("door 3 m, 20 cm", 3.0, room_with_door(0.1, 3.0)),
        ("door 6 m, 20 cm", 6.0, room_with_door(0.1, 6.0)),
    ] {
        let Some(surfaces) = room_surfaces_with("High + ADR-2605", &settings, h, &|| room.clone()) else {
            return;
        };
        let shares = surfaces.map(|(surface, share)| format!("{surface} {:.3} %", share * 100.0));
        eprintln!("OPT-IN High [{name}] {}", shares.join(", "));
    }
}

/// Words of a probe record, and the seed weight's word (`ddgi_trace.wgsl` `DDGI_SEED_WORD`).
const PROBE_WORDS: usize = 12;
const SEED_WORD: usize = 9;

/// `(offset.w, tag.w, seed)` of probe slot `probe`: offset.w 0 is inside geometry, tag.w its updates.
fn probe_record(rb: &regia_render::DdgiReadback, probe: usize) -> (f32, i32, u32) {
    let b = probe * PROBE_WORDS;
    (
        f32::from_bits(rb.data[b + 3]),
        rb.data[b + 7].cast_signed(),
        rb.data[b + SEED_WORD],
    )
}

/// A settled probe that a moved crate uncovers and that takes a seed worth `seed_updates`, as the field's doc
/// says, so it must restart its running average too: with its old age a settled probe keeps the seed at
/// the plain hysteresis (0.97 a frame on a settled Low probe), the seed then counting for about 33
/// updates instead of 4, and coarser light stays in it for seconds (ADR-2605).
#[test]
fn a_probe_out_of_geometry_again_restarts_its_average_with_the_seed() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(DdgiSettings {
        // Inside geometry re-traces every frame here, so the test sees the uncovering at once.
        inactive_slowdown: 0,
        ..tier_settings(DynamicGiTier::Low)
    });
    renderer.set_ddgi_enabled(Some(true));
    let ground = || block([0.0, -0.25, 0.0], [120.0, 0.25, 120.0]);
    let eye = [0.0, 1.6, -6.0];
    let target = [0.0, 1.0, 4.0];
    let crate_ = block([0.0, 2.5, 6.0], [5.0, 2.5, 5.0]);
    let covered = scene(eye, target, vec![ground(), crate_]);
    converge(&mut renderer, &device, &covered, 40);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"));
    let probes = rb.dims.iter().product::<u32>() as usize * rb.cascades as usize;
    let inside: Vec<usize> = (0..probes)
        .filter(|&i| {
            let (state, age, _) = probe_record(&rb, i);
            state < 0.5 && age >= 30
        })
        .collect();
    assert!(!inside.is_empty(), "the crate swallows settled probes");
    // Frame by frame: word 9 stays set from earlier frames inside, so only the exit frame tells a probe
    // that took a seed (the seed's frame is that frame's stamp) from one that replaces its history with
    // this frame's rays because no source could seed it, which keeps its age and holds no seed.
    let uncovered = scene(eye, target, vec![ground()]);
    let mut seeded = Vec::new();
    for _ in 0..12 {
        converge(&mut renderer, &device, &uncovered, 1);
        let rb = renderer
            .ddgi_read_back(&device)
            .unwrap_or_else(|err| panic!("read back: {err}"))
            .unwrap_or_else(|| panic!("the probes ran"));
        for &i in &inside {
            let (state, age, seed) = probe_record(&rb, i);
            let b = i * PROBE_WORDS;
            if state > 1.5 && seed > 0 && rb.data[b + SEED_WORD + 1] == rb.data[b + 8] {
                seeded.push((i, age));
            }
        }
    }
    eprintln!(
        "{} probes inside, seeded on their way out: {seeded:?}",
        inside.len()
    );
    assert!(!seeded.is_empty(), "some probe came out with a seed");
    for (i, age) in seeded {
        assert_eq!(age, 1, "probe {i} took a seed on its way out but kept its age");
    }
}

/// On a frame the light jumps every probe forgets its history: a slot that scrolled on that very frame
/// must not start from a neighbor that still holds the old light (ADR-2605).
#[test]
fn a_probe_scrolled_on_a_light_cut_takes_no_seed() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(tier_settings(DynamicGiTier::Low));
    renderer.set_ddgi_enabled(Some(true));
    let room = || vec![block([0.0, -0.25, 0.0], [120.0, 0.25, 120.0])];
    converge(
        &mut renderer,
        &device,
        &scene([0.0, 1.6, 0.0], [0.0, 1.0, 10.0], room()),
        12,
    );
    // A step past a whole cell of the finest cascade (2 m), so slots scroll, on the frame the sun jumps.
    let mut cut = scene([5.0, 1.6, 0.0], [5.0, 1.0, 10.0], room());
    cut.directional = sun(80.0, 100_000.0);
    converge(&mut renderer, &device, &cut, 1);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"));
    let probes = rb.dims.iter().product::<u32>() as usize * rb.cascades as usize;
    let seeded = (0..probes).filter(|&i| probe_record(&rb, i).2 != 0).count();
    assert_eq!(seeded, 0, "{seeded} probes took a seed of the old light");
}

/// The door room's floor over time with the static-screen cache off, by the options of ADR-2605. `quality()`
/// opts into that cache (`FrameQuality::reuse_static_screen`), so a held view keeps the composite of about
/// its ninth frame while the probes run on: the door room read 0.00706 and 0.0115 at 100, 200, 400 and 800
/// frames with it on, unchanged to five digits.
/// `cargo test -p regia_render --test dynamic_gi -- --ignored --nocapture door_room_settled`.
#[test]
#[ignore = "diagnostics: prints the door room's floor over time with the static-screen cache off, asserts nothing"]
fn door_room_settled() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let mut q = quality();
    q.paged_sun_shadows = false;
    assert!(!q.reuse_static_screen());
    let low = tier_settings(DynamicGiTier::Low);
    let eye = [0.0, 1.6, -1.5];
    let floor = [0.0, 0.0, 1.5];
    for (name, settings) in [
        ("Low", low.clone()),
        (
            "Low, ADR-2605 options off",
            DdgiSettings {
                seed_updates: 0,
                visibility_blend: false,
                recent_weight: 1,
                ..low.clone()
            },
        ),
        (
            "Low, recent_weight 1",
            DdgiSettings {
                recent_weight: 1,
                ..low.clone()
            },
        ),
        (
            "1 m layout",
            DdgiSettings {
                spacing: 1.0,
                cascades: 2,
                dims: [12, 8, 12],
                ..low.clone()
            },
        ),
        ("High", tier_settings(DynamicGiTier::High)),
    ] {
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_settings(settings);
        renderer.set_ddgi_enabled(Some(true));
        let lab = scene(eye, floor, room_with_door(0.1, 3.0));
        let mut done = 0;
        let mut line = Vec::new();
        for at in [9u32, 32, 100, 200, 400] {
            let mut px = Vec::new();
            for _ in done..at {
                let frame = lab.as_frame_scene_with_quality(q);
                px = renderer
                    .render_offscreen(&device, &frame)
                    .unwrap_or_else(|err| panic!("frame: {err}"))
                    .pixels;
            }
            done = at;
            line.push(format!("{at}: {:.5}", linear_luma(&px, 40, 50, 120, 90)));
        }
        eprintln!("DOOR {name}: {}", line.join(", "));
    }
}

/// The sealed rooms' surfaces over time with the static-screen cache off (see `door_room_settled`).
/// `cargo test -p regia_render --test dynamic_gi -- --ignored --nocapture rooms_settled`.
#[test]
#[ignore = "diagnostics: prints the rooms over time with the static-screen cache off, asserts nothing"]
fn rooms_settled() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let mut q = quality();
    q.paged_sun_shadows = false;
    assert!(!q.reuse_static_screen());
    let held = |renderer: &mut FrameRenderer, lab: &LabScene, frames: u32| -> Vec<u8> {
        let mut last = Vec::new();
        for _ in 0..frames {
            let frame = lab.as_frame_scene_with_quality(q);
            last = renderer
                .render_offscreen(&device, &frame)
                .unwrap_or_else(|err| panic!("frame: {err}"))
                .pixels;
        }
        last
    };
    let eye = [0.0, 1.6, -1.5];
    let floor = [0.0, 0.0, 1.5];
    for (name, h, tier) in [
        ("Low 3 m", 3.0f32, DynamicGiTier::Low),
        ("Low 6 m", 6.0, DynamicGiTier::Low),
        ("High 3 m", 3.0, DynamicGiTier::High),
    ] {
        let settings = tier_settings(tier);
        let mut out_renderer = FrameRenderer::new();
        out_renderer.set_ddgi_settings(settings.clone());
        out_renderer.set_ddgi_enabled(Some(true));
        let outside = scene([10.0, 2.5, 10.0], [6.0, 0.0, 6.0], sealed_room_of_height(0.1, h));
        let px = held(&mut out_renderer, &outside, 40);
        let exterior = linear_luma(&px, 40, 50, 120, 90);
        drop(out_renderer);
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_settings(settings);
        renderer.set_ddgi_enabled(Some(true));
        let mut done = 0;
        for at in [9u32, 100, 300] {
            let lab = scene(eye, floor, sealed_room_of_height(0.1, h));
            let px = held(&mut renderer, &lab, at - done);
            done = at;
            eprintln!(
                "SEALED {name} floor at {at}: {:.3} %",
                100.0 * linear_luma(&px, 40, 20, 120, 76) / exterior
            );
        }
    }
}

/// Whether the probes' deficit on the GI validation plates (25 to 41 % low against the path tracer
/// on trench and color_room, `regia_pt` `gi_radiance_cache`, which scores the probes after 32
/// frames) is convergence: the mean HDR luminance of the probe frame after 32, 64, 128 and 256
/// frames, each plate's own probe settings (ADR-2605). It is not: on lavapipe the frame moved
/// +0.05 % (trench) and +0.25 % (color_room) from 32 to 256 frames, so the deficit is the settled
/// probes' own error.
/// `cargo test -p regia_render --test dynamic_gi -- --ignored --nocapture probe_convergence_on_the_gi_plates`.
#[test]
#[ignore = "diagnostics: prints the probe frame's mean luminance over time, asserts nothing"]
fn probe_convergence_on_the_gi_plates() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = regia_render::GiMode::Probes;
    q.gi_probes = true;
    q.screen_space_gi = false;
    q.baked_gi = false;
    for scene in [
        regia_test_utils::gi_scenes::trench(),
        regia_test_utils::gi_scenes::color_room(),
    ] {
        let mut r = FrameRenderer::new();
        r.set_ddgi_settings(scene.ddgi.clone());
        r.set_ddgi_enabled(Some(true));
        let frame = scene.lab.as_frame_scene_with_quality(q);
        let mut done = 0u32;
        let mut line = Vec::new();
        for at in [32u32, 64, 128, 256] {
            while done < at {
                done += 1;
                if done == at {
                    r.request_hdr_capture();
                }
                r.render_offscreen(&device, &frame)
                    .unwrap_or_else(|err| panic!("frame: {err}"));
            }
            let hdr = r
                .take_hdr_capture(&device)
                .unwrap_or_else(|err| panic!("read back: {err}"))
                .unwrap_or_else(|| panic!("the frame recorded its HDR copy"));
            let mean = hdr
                .rgb
                .iter()
                .map(|c| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2])
                .sum::<f32>()
                / hdr.rgb.len().max(1) as f32;
            line.push(format!("{at}: {mean:.2}"));
        }
        eprintln!(
            "CONVERGENCE {} mean HDR luma after {}",
            scene.name,
            line.join(", ")
        );
    }
}

/// Measurement (ADR-5001): the 6 m sealed room's surfaces on High's 1 m layout and its neighbours, to attribute a
/// ceiling leak to the layout, the ADR-2605 options or the normal bias. Prints only.
#[test]
#[ignore = "measurement: the 6 m room across High layout variants, prints only"]
fn six_m_room_on_the_high_layout_variants() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let shipped = tier_settings(DynamicGiTier::High);
    // The variants below were measured on ADR-5001's ten-layer High (ADR-5290 gave High twelve layers and 3.5 m
    // of headroom): they keep that layout so their names and the ADR's numbers still match.
    let high = DdgiSettings {
        dims: [20, 10, 20],
        finest_headroom_m: regia_render::FINEST_HEADROOM_M,
        ..shipped.clone()
    };
    let legacy = DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        update_divisor: 1,
        ..DdgiSettings::default()
    };
    let variants: Vec<(&str, DdgiSettings)> = vec![
        (
            "shipped high (1 m x 3.8, 12 layers, headroom 3.5)",
            shipped.clone(),
        ),
        ("adr-5001 high (1 m x 3.8, options)", high.clone()),
        (
            "candidate 0.75 m x 4.18, 14 layers",
            DdgiSettings {
                spacing: 0.75,
                spacing_ratio: 4.18,
                dims: [20, 14, 20],
                ..shipped.clone()
            },
        ),
        (
            "candidate 0.5 m x 4.79, 18 layers",
            DdgiSettings {
                spacing: 0.5,
                spacing_ratio: 4.79,
                dims: [20, 18, 20],
                ..shipped
            },
        ),
        (
            "2 m x 3 + options",
            DdgiSettings {
                visibility_blend: true,
                seed_updates: high.seed_updates,
                recent_weight: high.recent_weight,
                ..legacy.clone()
            },
        ),
        (
            "2 m x 3 + blend only",
            DdgiSettings {
                visibility_blend: true,
                ..legacy
            },
        ),
        (
            "1 m x 3.8, blend only",
            DdgiSettings {
                seed_updates: 0,
                recent_weight: 1,
                ..high.clone()
            },
        ),
        (
            "1 m x 3.8, 12 layers",
            DdgiSettings {
                dims: [20, 12, 20],
                ..high.clone()
            },
        ),
        (
            "1 m x 3.8, normal bias 0.2",
            DdgiSettings {
                normal_bias: 0.2,
                ..high.clone()
            },
        ),
        (
            "1 m x 3, options",
            DdgiSettings {
                spacing_ratio: 3.0,
                ..high.clone()
            },
        ),
        (
            "1 m x 4.5, options",
            DdgiSettings {
                spacing_ratio: 4.5,
                ..high.clone()
            },
        ),
        (
            "1 m x 5, options",
            DdgiSettings {
                spacing_ratio: 5.0,
                ..high.clone()
            },
        ),
        (
            "1 m x 6, options",
            DdgiSettings {
                spacing_ratio: 6.0,
                ..high.clone()
            },
        ),
        (
            "1 m x 4.5, 12 layers",
            DdgiSettings {
                spacing_ratio: 4.5,
                dims: [20, 12, 20],
                ..high.clone()
            },
        ),
        (
            "1.5 m x 3.3, options",
            DdgiSettings {
                spacing: 1.5,
                spacing_ratio: 3.3,
                ..high.clone()
            },
        ),
        // ADR-5290: the finest cascade's window over the eye (`finest_headroom_m`). At 2 m the visibility
        // blend's fade still covers a ceiling 4.4 m over the eye; at 1 m and 2 m of headroom it ends
        // 0.65 cells under it, so the 3.8 m cascade lights the ceiling alone.
        (
            "1 m x 3.8, headroom 2.7",
            DdgiSettings {
                finest_headroom_m: 2.7,
                ..high.clone()
            },
        ),
        (
            "1 m x 3.8, 12 layers, headroom 3.5",
            DdgiSettings {
                dims: [20, 12, 20],
                finest_headroom_m: 3.5,
                ..high.clone()
            },
        ),
        (
            "1 m x 3.8, 12 layers, headroom 4.0",
            DdgiSettings {
                dims: [20, 12, 20],
                finest_headroom_m: 4.0,
                ..high.clone()
            },
        ),
        (
            "1 m x 3.8, 12 layers, headroom 4.7",
            DdgiSettings {
                dims: [20, 12, 20],
                finest_headroom_m: 4.7,
                ..high
            },
        ),
    ];
    // `REGIA_ROOM_VARIANTS=a,b`: only the variants whose names contain one of the words.
    let only = std::env::var("REGIA_ROOM_VARIANTS").ok();
    let variants: Vec<_> = variants
        .into_iter()
        .filter(|(n, _)| {
            only.as_deref()
                .is_none_or(|o| o.split(',').any(|w| n.contains(w.trim())))
        })
        .collect();
    for (name, settings) in variants {
        let _ = room_surfaces_with(name, &settings, 6.0, &|| sealed_room_of_height(0.1, 6.0));
        let _ = room_surfaces_with(name, &settings, 3.0, &|| sealed_room_of_height(0.1, 3.0));
    }
}
