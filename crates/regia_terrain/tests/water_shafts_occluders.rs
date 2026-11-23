// SPDX-License-Identifier: MPL-2.0
//! Underwater sun shafts cut by occluders, averaged over frames and seen from above (ADR-3251), on
//! a real GPU (lavapipe in CI).
//!
//! The water layer is driven directly (its `simulate` on an encoder of our own) against a depth
//! target cleared to one value, a fronto-parallel wall at a known distance, so a CPU replica of the
//! march's geometry is an exact oracle: a texel whose whole march path lies where a box above the
//! water shadows the surface reads zero, and a texel whose path stays clear of that shadow reads
//! the same bytes as with no shadow bound. The shadow map is rasterized on the CPU from the box,
//! so the test needs no frame renderer. No golden image and no wall clock: every check compares
//! buffers of the same run or a geometric classification.

use std::sync::Arc;

use regia_core::glam::{Mat4, Vec2, Vec3, Vec4};
use regia_render::{LayerCamera, RenderDevice, TranslucentLayer};
use regia_terrain::water_body::SHAFT_HISTORY_FRAMES;
use regia_terrain::{
    WaterBody, WaterCaustics, WaterClock, WaterLook, WaterRenderer, WaterSunShadow, WaterTier,
};

const W: u32 = 192;
const H: u32 = 108;
const LEVEL: f32 = 0.0;
const TICK: u64 = 600;
const NEAR: f32 = 0.1;
const FAR: f32 = 500.0;
const FOV: f32 = 1.1;
/// Toward the sun (unit after normalization): high and to the side.
const TO_SUN: [f32; 3] = [0.35, 0.85, -0.25];
/// A box: its min and max corners.
type Slab = ([f32; 3], [f32; 3]);
/// The pier deck: a long slab 2 m above the water.
const DECK: Slab = ([-3.0, 2.0, 3.0], [3.0, 2.4, 30.0]);
/// A cave roof: the same slab under the water, 0.8 to 1.2 m down.
const ROOF: Slab = ([-3.0, -1.2, 3.0], [3.0, -0.8, 30.0]);
/// Edge of the CPU shadow map (texels) and the square it covers (m).
const MAP: u32 = 512;
const MAP_EXTENT: f32 = 60.0;
/// How far the wall stands in front of the eye (view-space metres).
const WALL: f32 = 9.0;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn to_sun() -> Vec3 {
    Vec3::from_array(TO_SUN).normalize()
}

/// Clear water with stream-sized ripples and shafts on; a flat plane (no swell), so the surface
/// the march uses is exactly `LEVEL`.
fn clear_plane(caustics: WaterCaustics) -> WaterBody {
    let mut look = WaterLook::ocean();
    look.caustics = caustics;
    look.absorption = [0.12, 0.04, 0.03];
    look.scattering = 0.2;
    look.ripple_scale = 1.5;
    WaterBody::plane([0.0, LEVEL, 0.0], [200.0, 200.0], look)
}

fn shafts_on() -> WaterCaustics {
    WaterCaustics {
        enabled: true,
        strength: 2.0,
        shaft_steps: 16,
        ..WaterCaustics::default()
    }
}

/// Under the surface, in front of the deck's shadow, looking along +z toward it.
const UNDER_EYE: [f32; 3] = [0.0, -2.0, -1.0];
const UNDER_TARGET: [f32; 3] = [0.0, -2.6, 8.0];
/// Under the surface inside the deck's shadow, looking along the deck.
const SHADE_EYE: [f32; 3] = [-0.9, -2.0, 6.0];
const SHADE_TARGET: [f32; 3] = [-0.9, -2.6, 15.0];
/// Above the surface, looking down into the water beside the deck.
const ABOVE_EYE: [f32; 3] = [0.0, 3.0, -4.0];
const ABOVE_TARGET: [f32; 3] = [0.0, -2.0, 5.0];

fn camera(eye: [f32; 3], target: [f32; 3]) -> LayerCamera {
    LayerCamera {
        eye,
        target,
        fov_y: FOV,
        near: NEAR,
        far: FAR,
        width: W,
        height: H,
        exposure: 1.0,
        display_exposure: 1.0,
        hdr: true,
        sun_direction: to_sun().to_array(),
        sun_color: [1.0, 0.97, 0.92],
        sun_illuminance: 90_000.0,
        sky_ambient: None,
    }
}

/// The view-projection the water layer builds for `camera` (directx depth, 0 near, 1 far).
fn view_proj(camera: &LayerCamera) -> Mat4 {
    let aspect = camera.width as f32 / camera.height as f32;
    let view = regia_render::view_math::look_at_view(
        Vec3::from_array(camera.eye),
        Vec3::from_array(camera.target),
        Vec3::Y,
    );
    let (fov, near, far) = regia_render::view_math::usable_projection(camera.fov_y, camera.near, camera.far);
    regia_core::glam::camera::rh::proj::directx::perspective(fov, aspect, near, far) * view
}

/// NDC depth of a fronto-parallel wall `dist` metres in front of the eye.
fn wall_depth(camera: &LayerCamera, dist: f32) -> f32 {
    let (fov, near, far) = regia_render::view_math::usable_projection(camera.fov_y, camera.near, camera.far);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(fov, 1.0, near, far);
    let c = proj * Vec4::new(0.0, 0.0, -dist, 1.0);
    c.z / c.w
}

/// A `Depth32Float` target of `size` cleared to `depth`.
fn depth_target(device: &RenderDevice, size: [u32; 2], depth: f32) -> wgpu::TextureView {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shaft test depth"),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("shaft test depth clear"),
        color_attachments: &[],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(depth),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    }));
    device.queue.submit([encoder.finish()]);
    view
}

/// The ray from `origin` along `dir` against `slab`: the entry distance, if it hits.
fn ray_box(origin: Vec3, dir: Vec3, slab: Slab) -> Option<f32> {
    let (lo, hi) = (Vec3::from_array(slab.0), Vec3::from_array(slab.1));
    let mut t0 = 0.0f32;
    let mut t1 = f32::INFINITY;
    for a in 0..3 {
        if dir[a].abs() < 1.0e-9 {
            if origin[a] < lo[a] || origin[a] > hi[a] {
                return None;
            }
            continue;
        }
        let (mut near, mut far) = ((lo[a] - origin[a]) / dir[a], (hi[a] - origin[a]) / dir[a]);
        if near > far {
            std::mem::swap(&mut near, &mut far);
        }
        t0 = t0.max(near);
        t1 = t1.min(far);
        if t0 > t1 {
            return None;
        }
    }
    Some(t0)
}

/// The sun's orthographic view over the scene and its shadow map, rasterized from the box on the
/// CPU (texel centers, depth of the box's face toward the sun; 1 where nothing casts).
fn light_view_proj() -> Mat4 {
    let center = Vec3::new(0.0, 0.0, 10.0);
    let eye = center + to_sun() * 50.0;
    let view = regia_render::view_math::look_at_view(eye, center, Vec3::Z);
    let h = 0.5 * MAP_EXTENT;
    regia_core::glam::camera::rh::proj::directx::orthographic(-h, h, -h, h, 1.0, 100.0) * view
}

fn shadow_texels(caster: Option<Slab>) -> Vec<f32> {
    let vp = light_view_proj();
    let inv = vp.inverse();
    let mut out = vec![1.0f32; (MAP * MAP) as usize];
    let Some(caster) = caster else {
        return out;
    };
    for j in 0..MAP {
        for i in 0..MAP {
            let suv = Vec2::new((i as f32 + 0.5) / MAP as f32, (j as f32 + 0.5) / MAP as f32);
            let ndc = Vec2::new(suv.x * 2.0 - 1.0, 1.0 - suv.y * 2.0);
            let origin = inv.project_point3(Vec3::new(ndc.x, ndc.y, 0.0));
            if let Some(t) = ray_box(origin, -to_sun(), caster) {
                let hit = origin - to_sun() * t;
                out[(j * MAP + i) as usize] = vp.project_point3(hit).z;
            }
        }
    }
    out
}

fn shadow(device: &RenderDevice, caster: Option<Slab>) -> WaterSunShadow {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shaft test sun shadow"),
        size: wgpu::Extent3d {
            width: MAP,
            height: MAP,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let texels = shadow_texels(caster);
    device.queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(MAP * 4),
            rows_per_image: Some(MAP),
        },
        wgpu::Extent3d {
            width: MAP,
            height: MAP,
            depth_or_array_layers: 1,
        },
    );
    WaterSunShadow {
        view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
        light_view_proj: light_view_proj().to_cols_array_2d(),
        bias: 0.0,
        edge_fade: false,
    }
}

struct Rig {
    device: RenderDevice,
    water: WaterRenderer,
    layer: Arc<dyn TranslucentLayer>,
    depth: wgpu::TextureView,
    camera: LayerCamera,
}

fn rig(device: &RenderDevice, body: &WaterBody, camera: LayerCamera, underwater: bool) -> Rig {
    let water = WaterRenderer::new(device);
    water.set_tier(WaterTier::High);
    water.set_underwater(underwater);
    water.set_clock(WaterClock {
        tick: TICK,
        ..WaterClock::default()
    });
    water.set_bodies(&[body]);
    water.set_ambient_lux(18_000.0);
    let depth = depth_target(device, [camera.width, camera.height], wall_depth(&camera, WALL));
    Rig {
        device: device.clone(),
        layer: water.layer(),
        water,
        depth,
        camera,
    }
}

impl Rig {
    /// One frame's simulate (the caustic tile, the shafts and the resolve); returns the shafts the
    /// draw would sample.
    fn frame(&self) -> (Vec<[f32; 4]>, [u32; 2]) {
        let mut encoder = self
            .device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        self.layer.simulate(&self.depth, &self.camera, &mut encoder);
        self.device.queue.submit([encoder.finish()]);
        self.water.read_shafts().expect("a shaft pass ran")
    }
}

/// The march path of shaft texel `(i, j)` under the surface, as the shader forms it: the eye, the
/// view direction and the path length (the wall, the surface or 60 m).
fn under_path(camera: &LayerCamera, size: [u32; 2], i: u32, j: u32) -> (Vec3, Vec3, f32) {
    let inv = view_proj(camera).inverse();
    let uv = Vec2::new(
        (i as f32 + 0.5) / size[0] as f32,
        (j as f32 + 0.5) / size[1] as f32,
    );
    let p = inv.project_point3(Vec3::new(
        uv.x * 2.0 - 1.0,
        1.0 - uv.y * 2.0,
        wall_depth(camera, WALL),
    ));
    let eye = Vec3::from_array(camera.eye);
    let dir = (p - eye).normalize();
    let mut path = (p - eye).length();
    if dir.y > 1.0e-4 {
        path = path.min((LEVEL - eye.y) / dir.y);
    }
    (eye, dir, path.clamp(0.0, 60.0))
}

/// Where the refracted sun beam that reaches `q` crossed the surface.
fn entry_of(q: Vec3) -> Vec2 {
    let beam = refract(-to_sun(), Vec3::Y, 1.0 / 1.333);
    let s = q + beam * ((LEVEL - q.y) / beam.y.min(-1.0e-3));
    Vec2::new(s.x, s.z)
}

/// WGSL's `refract`.
fn refract(i: Vec3, n: Vec3, eta: f32) -> Vec3 {
    let d = n.dot(i);
    let k = 1.0 - eta * eta * (1.0 - d * d);
    if k < 0.0 {
        Vec3::ZERO
    } else {
        eta * i - (eta * d + k.sqrt()) * n
    }
}

/// Whether the box shadows the surface point `s` (the sun's ray from `s` hits it), and the same
/// for every point within `margin` around it: `Some(true)` all shadowed, `Some(false)` none,
/// `None` mixed.
fn shadowed_around(s: Vec2, margin: f32) -> Option<bool> {
    let mut lit = 0;
    let mut dark = 0;
    for k in 0..9 {
        let off = if k == 0 {
            Vec2::ZERO
        } else {
            let a = (k - 1) as f32 * std::f32::consts::FRAC_PI_4;
            Vec2::new(a.cos(), a.sin()) * margin
        };
        let p = Vec3::new(s.x + off.x, LEVEL, s.y + off.y);
        if ray_box(p, to_sun(), DECK).is_some() {
            dark += 1;
        } else {
            lit += 1;
        }
    }
    match (dark, lit) {
        (_, 0) => Some(true),
        (0, _) => Some(false),
        _ => None,
    }
}

/// Classifies a march path: `Some(true)` when every point on it under the water is in the box's
/// shadow (by a margin), `Some(false)` when none is, `None` otherwise. The path is walked in 2 cm
/// steps, finer than the margin.
fn classify(eye: Vec3, dir: Vec3, path: f32, margin: f32) -> Option<bool> {
    let steps = ((path / 0.02).ceil() as usize).max(1);
    let mut verdict: Option<bool> = None;
    for k in 0..=steps {
        let q = eye + dir * (path * k as f32 / steps as f32);
        if LEVEL - q.y <= 0.0 {
            continue;
        }
        let here = shadowed_around(entry_of(q), margin)?;
        if verdict.is_some_and(|v| v != here) {
            return None;
        }
        verdict = Some(here);
    }
    verdict
}

fn energy(texel: &[f32; 4]) -> f32 {
    texel[0] + texel[1] + texel[2]
}

/// Shafts of one camera with no map, an empty map and the box's map; per texel the class of its
/// march path. Asserts the empty map changes no byte, every texel wholly in the shadow reads zero
/// and every texel wholly clear of it keeps its bytes. Returns `(inside, outside, energy lost)`.
fn carve(device: &RenderDevice, eye: [f32; 3], target: [f32; 3]) -> (usize, usize, f32) {
    let body = clear_plane(shafts_on());
    let rig = rig(device, &body, camera(eye, target), true);
    let (unbound, size) = rig.frame();
    let stats = rig.water.caustic_stats();
    assert!(stats.shafts_drawn && !stats.shadow_bound, "{stats:?}");
    rig.water.set_sun_shadow(Some(shadow(device, None)));
    let (empty, _) = rig.frame();
    assert!(rig.water.caustic_stats().shadow_bound);
    assert_eq!(unbound, empty, "a shadow map with no caster changed the shafts");
    rig.water.set_sun_shadow(Some(shadow(device, Some(DECK))));
    let (cut, _) = rig.frame();
    let (mut inside, mut outside) = (0usize, 0usize);
    for j in 0..size[1] {
        for i in 0..size[0] {
            let k = (j * size[0] + i) as usize;
            let (eye, dir, path) = under_path(&rig.camera, size, i, j);
            match classify(eye, dir, path, 0.25) {
                Some(true) => {
                    inside += 1;
                    assert_eq!(energy(&cut[k]), 0.0, "texel {i},{j} lies in the box's shadow");
                }
                Some(false) => {
                    outside += 1;
                    assert_eq!(cut[k], unbound[k], "texel {i},{j} is clear of the shadow");
                }
                None => {}
            }
        }
    }
    let total = |b: &[[f32; 4]]| b.iter().map(energy).sum::<f32>();
    (inside, outside, total(&unbound) - total(&cut))
}

/// Item 1 (ADR-3251): a box above the water carves a dark volume into the shafts, exactly where
/// the geometry says, and leaves every other texel's bytes alone; with the map bound but nothing
/// casting, the shafts are the same bytes as with no map.
#[test]
fn a_box_above_the_water_carves_its_shadow_out_of_the_shafts() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    // From inside the shadow, along the deck: most paths stay in it.
    let (inside, _, lost) = carve(&device, SHADE_EYE, SHADE_TARGET);
    println!("in the shade: {inside} texels wholly shadowed, {lost} energy removed");
    assert!(inside >= 200, "only {inside} texels wholly in the shadow");
    assert!(lost > 0.0, "the shadowed texels had no shaft light to lose");
    // From in front of it: paths that turn away stay lit, paths toward it lose light.
    let (_, outside, lost) = carve(&device, UNDER_EYE, UNDER_TARGET);
    println!("in front: {outside} texels wholly clear, {lost} energy removed");
    assert!(outside >= 200, "only {outside} texels clear of the shadow");
    assert!(lost > 0.0, "the paths into the shadow lost no light");
}

/// Whether the straight line from `q` to the sun meets the cave roof, for `q` and the points
/// within `margin` around it and 15 cm nearer the sun (beyond the march's depth bias): `Some(true)`
/// all shadowed, `Some(false)` none, `None` mixed.
fn under_roof(q: Vec3, margin: f32) -> Option<bool> {
    let (mut lit, mut dark) = (0, 0);
    for k in 0..10 {
        let p = match k {
            0 => q,
            9 => q + to_sun() * 0.15,
            _ => {
                let a = (k - 1) as f32 * std::f32::consts::FRAC_PI_4;
                q + Vec3::new(a.cos(), 0.0, a.sin()) * margin
            }
        };
        if ray_box(p, to_sun(), ROOF).is_some() {
            dark += 1;
        } else {
            lit += 1;
        }
    }
    match (dark, lit) {
        (_, 0) => Some(true),
        (0, _) => Some(false),
        _ => None,
    }
}

/// Item 1, casters under the water (ADR-3251): a cave roof shades the water below it. The march
/// reads the map along the straight line to the sun there, so the oracle is that line: every
/// texel whose whole path lies under the roof reads zero, and the roof takes light away.
#[test]
fn a_cave_roof_under_the_water_shades_the_shafts_below_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let rig = rig(
        &device,
        &clear_plane(shafts_on()),
        camera(SHADE_EYE, SHADE_TARGET),
        true,
    );
    let (open, size) = rig.frame();
    rig.water.set_sun_shadow(Some(shadow(&device, Some(ROOF))));
    let (roofed, _) = rig.frame();
    let mut inside = 0usize;
    for j in 0..size[1] {
        for i in 0..size[0] {
            let k = (j * size[0] + i) as usize;
            let (eye, dir, path) = under_path(&rig.camera, size, i, j);
            let steps = ((path / 0.02).ceil() as usize).max(1);
            let all_dark = (0..=steps).all(|n| {
                let q = eye + dir * (path * n as f32 / steps as f32);
                LEVEL - q.y <= 0.0 || under_roof(q, 0.25) == Some(true)
            });
            if all_dark {
                inside += 1;
                assert_eq!(energy(&roofed[k]), 0.0, "texel {i},{j} lies under the roof");
            }
        }
    }
    let total = |b: &[[f32; 4]]| b.iter().map(energy).sum::<f32>();
    let lost = total(&open) - total(&roofed);
    println!("under the roof: {inside} texels wholly shaded, {lost} energy removed");
    assert!(inside >= 200, "only {inside} texels wholly under the roof");
    assert!(lost > 0.0, "the roof took no light away");
}

/// The grain of a buffer: the mean squared difference of each texel's energy from its 3 x 3
/// neighbourhood mean.
fn grain(buf: &[[f32; 4]], size: [u32; 2]) -> f64 {
    let (w, h) = (size[0] as i64, size[1] as i64);
    let e = |x: i64, y: i64| f64::from(energy(&buf[(y.clamp(0, h - 1) * w + x.clamp(0, w - 1)) as usize]));
    let mut sum = 0.0;
    for y in 0..h {
        for x in 0..w {
            let mut mean = 0.0;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    mean += e(x + dx, y + dy);
                }
            }
            sum += (e(x, y) - mean / 9.0).powi(2);
        }
    }
    sum / (w * h) as f64
}

/// The mean absolute difference of two buffers' energies.
fn flicker(a: &[[f32; 4]], b: &[[f32; 4]]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from((energy(x) - energy(y)).abs()))
        .sum::<f64>()
        / a.len() as f64
}

/// Item 2 (ADR-3251): with a fixed camera and clock the accumulated shafts hold the history's
/// frame count in alpha, carry less grain than the raw march and flicker far less from frame to
/// frame than the same moving jitter without a history; a camera cut restarts every texel.
#[test]
fn accumulation_cuts_the_grain_and_restarts_on_a_camera_cut() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let cam = camera(UNDER_EYE, UNDER_TARGET);
    let raw_rig = rig(&device, &clear_plane(shafts_on()), cam, true);
    let (raw, size) = raw_rig.frame();
    assert_eq!(
        raw_rig.water.caustic_stats().resolves,
        0,
        "no resolve without the switch"
    );

    let acc = WaterCaustics {
        shaft_accumulate: true,
        ..shafts_on()
    };
    // The moving jitter alone: the history restarted before every frame.
    let jitter_rig = rig(&device, &clear_plane(acc), cam, true);
    let mut jittered = Vec::new();
    for _ in 0..3 {
        jitter_rig.water.reset_shaft_history();
        jittered.push(jitter_rig.frame().0);
    }
    assert!(
        jittered[2].iter().all(|t| t[3] == 1.0),
        "a restarted history holds one frame"
    );

    let acc_rig = rig(&device, &clear_plane(acc), cam, true);
    acc_rig.water.set_camera_cuts(7);
    let mut frames = Vec::new();
    for _ in 0..24 {
        frames.push(acc_rig.frame().0);
    }
    let stats = acc_rig.water.caustic_stats();
    assert_eq!(stats.resolves, 24, "{stats:?}");
    assert_eq!(stats.history_frames, SHAFT_HISTORY_FRAMES, "{stats:?}");
    let last = &frames[23];
    // A fixed camera reprojects every texel onto itself: all hold the capped count.
    assert!(
        last.iter().all(|t| t[3] == SHAFT_HISTORY_FRAMES as f32),
        "every texel should average {SHAFT_HISTORY_FRAMES} frames"
    );
    let (g_raw, g_acc) = (grain(&raw, size), grain(last, size));
    let (f_jit, f_acc) = (flicker(&jittered[2], &jittered[1]), flicker(last, &frames[22]));
    println!(
        "grain raw {g_raw:.6e} accumulated {g_acc:.6e}; flicker jitter-only {f_jit:.6e} accumulated {f_acc:.6e}"
    );
    assert!(
        g_acc < 0.7 * g_raw,
        "grain {g_acc} not below 0.7 x the raw march's {g_raw}"
    );
    assert!(
        f_acc < 0.5 * f_jit,
        "flicker {f_acc} not below half the jitter-only {f_jit}"
    );

    // A camera cut: the next frame starts over in every texel.
    let resets = stats.history_resets;
    acc_rig.water.set_camera_cuts(8);
    let (after, _) = acc_rig.frame();
    let stats = acc_rig.water.caustic_stats();
    assert_eq!(stats.history_resets, resets + 1, "{stats:?}");
    assert_eq!(stats.history_frames, 1, "{stats:?}");
    assert!(after.iter().all(|t| t[3] == 1.0), "a cut keeps no history");
}

/// Item 3 (ADR-3251): off by default nothing runs with the eye above the water; on, the march runs
/// and only texels whose view ray enters the water before it meets the wall can carry light.
#[test]
fn shafts_from_above_are_opt_in_and_only_behind_the_surface() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let cam = camera(ABOVE_EYE, ABOVE_TARGET);
    let off = rig(&device, &clear_plane(shafts_on()), cam, true);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    off.layer.simulate(&off.depth, &off.camera, &mut encoder);
    device.queue.submit([encoder.finish()]);
    let stats = off.water.caustic_stats();
    assert_eq!(stats.shaft_passes, 0, "{stats:?}");
    assert!(off.water.read_shafts().is_none());

    let above = WaterCaustics {
        shafts_from_above: true,
        ..shafts_on()
    };
    let on = rig(&device, &clear_plane(above), cam, true);
    let (buf, size) = on.frame();
    let stats = on.water.caustic_stats();
    assert!(stats.shafts_drawn && stats.from_above, "{stats:?}");
    let inv = view_proj(&cam).inverse();
    let eye = Vec3::from_array(cam.eye);
    let (mut dry, mut lit) = (0usize, 0usize);
    for j in 0..size[1] {
        for i in 0..size[0] {
            let uv = Vec2::new(
                (i as f32 + 0.5) / size[0] as f32,
                (j as f32 + 0.5) / size[1] as f32,
            );
            let p = inv.project_point3(Vec3::new(
                uv.x * 2.0 - 1.0,
                1.0 - uv.y * 2.0,
                wall_depth(&cam, WALL),
            ));
            let dir = (p - eye).normalize();
            let entry = if dir.y < -1.0e-4 {
                (eye.y - LEVEL) / -dir.y
            } else {
                f32::INFINITY
            };
            let e = energy(&buf[(j * size[0] + i) as usize]);
            // A margin of 2 % keeps texels at the waterline out of either class.
            if (p - eye).length() < entry * 0.98 {
                dry += 1;
                assert_eq!(e, 0.0, "texel {i},{j} meets the wall before the water");
            } else if e > 0.0 {
                lit += 1;
            }
        }
    }
    assert!(dry >= 100, "only {dry} texels see the wall above the water");
    assert!(
        lit >= 100,
        "only {lit} texels behind the surface carry shaft light"
    );
    // Back under the surface the view from above stops at once.
    let under = rig(
        &device,
        &clear_plane(above),
        camera(UNDER_EYE, UNDER_TARGET),
        true,
    );
    under.frame();
    assert!(!under.water.caustic_stats().from_above);
}

/// Writes a shaft buffer as a PNG (energy tone-mapped by `1 - exp(-x / scale)`, scaled up 2x)
/// when `REGIA_WATER_FRAME_DUMP` names a directory. Evidence for the owner; never compared.
fn dump(label: &str, buf: &[[f32; 4]], size: [u32; 2], scale: f32) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_WATER_FRAME_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = std::path::Path::new(&dir).join(format!("shafts_{label}.png"));
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    let (w, h) = (size[0] * 2, size[1] * 2);
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let t = buf[((y / 2) * size[0] + x / 2) as usize];
            for channel in &t[..3] {
                let v = 1.0 - (-channel.max(0.0) / scale).exp();
                pixels.push((v.powf(1.0 / 2.2) * 255.0).round() as u8);
            }
            pixels.push(255);
        }
    }
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(&pixels);
    }
}

/// Plates for the owner (ADR-3251; `REGIA_WATER_FRAME_DUMP=<dir> cargo test -p regia_terrain --test
/// water_shafts_occluders -- --ignored shaft_plates`): the shaft buffer at 960 x 540 (shafts at half
/// that) under the pier without and with its shadow, and the raw march against 24 accumulated
/// frames. The buffers are what the water adds to the underwater fog, before the scene and the
/// water column; the frame-level plates need the frame hookup patch and a scene with a pier.
#[test]
#[ignore = "plate generator; run by hand with REGIA_WATER_FRAME_DUMP"]
fn shaft_plates() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let big = |eye, target| LayerCamera {
        width: 960,
        height: 540,
        ..camera(eye, target)
    };
    let pier = rig(
        &device,
        &clear_plane(shafts_on()),
        big(UNDER_EYE, UNDER_TARGET),
        true,
    );
    let (before, size) = pier.frame();
    let peak = before.iter().map(energy).fold(0.0f32, f32::max).max(1.0e-6);
    let scale = 0.25 * peak;
    dump("pier_before", &before, size, scale);
    pier.water.set_sun_shadow(Some(shadow(&device, Some(DECK))));
    let (after, _) = pier.frame();
    dump("pier_after", &after, size, scale);
    let shade = rig(
        &device,
        &clear_plane(shafts_on()),
        big(SHADE_EYE, SHADE_TARGET),
        true,
    );
    let (before, _) = shade.frame();
    dump("shade_before", &before, size, scale);
    shade.water.set_sun_shadow(Some(shadow(&device, Some(DECK))));
    let (after, _) = shade.frame();
    dump("shade_after", &after, size, scale);
    dump("raw", &pier.frame().0, size, scale);
    let acc = rig(
        &device,
        &clear_plane(WaterCaustics {
            shaft_accumulate: true,
            ..shafts_on()
        }),
        big(UNDER_EYE, UNDER_TARGET),
        true,
    );
    acc.water.set_sun_shadow(Some(shadow(&device, Some(DECK))));
    let mut last = Vec::new();
    for _ in 0..24 {
        last = acc.frame().0;
    }
    dump("accumulated", &last, size, scale);
    let above = rig(
        &device,
        &clear_plane(WaterCaustics {
            shafts_from_above: true,
            ..shafts_on()
        }),
        big(ABOVE_EYE, ABOVE_TARGET),
        true,
    );
    above.water.set_sun_shadow(Some(shadow(&device, Some(DECK))));
    dump("from_above", &above.frame().0, size, scale);
}

/// Item 2, reprojection (ADR-3251): after 8 frames at one camera, the eye turns and one more frame
/// is resolved. A texel whose march path reprojects onto the previous screen averages 9 frames, one
/// whose path was off the previous screen restarts at 1. A pure turn moves every point of a view
/// ray alike, so the previous view-projection alone decides it (2 % margins at the edges).
#[test]
fn a_turning_camera_reprojects_the_history() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let acc = WaterCaustics {
        shaft_accumulate: true,
        ..shafts_on()
    };
    let first = camera(UNDER_EYE, UNDER_TARGET);
    let mut rig = rig(&device, &clear_plane(acc), first, true);
    for _ in 0..8 {
        rig.frame();
    }
    // Turn by about a quarter of the view to the side, eye fixed.
    let turned = camera(UNDER_EYE, [4.0, -2.6, 8.0]);
    rig.camera = turned;
    let (buf, size) = rig.frame();
    let prev = view_proj(&first);
    let (mut kept, mut restarted) = (0usize, 0usize);
    for j in 0..size[1] {
        for i in 0..size[0] {
            let (eye, dir, _) = under_path(&turned, size, i, j);
            let c = prev * (eye + dir * 5.0).extend(1.0);
            let uv = Vec2::new(c.x / c.w * 0.5 + 0.5, 0.5 - c.y / c.w * 0.5);
            let n = buf[(j * size[0] + i) as usize][3];
            let inner = |v: f32| (0.02..0.98).contains(&v);
            let outer = |v: f32| !(-0.02..1.02).contains(&v);
            if c.w > 0.0 && inner(uv.x) && inner(uv.y) {
                kept += 1;
                assert_eq!(n, 9.0, "texel {i},{j} reprojects to {uv} and keeps its history");
            } else if c.w <= 0.0 || outer(uv.x) || outer(uv.y) {
                restarted += 1;
                assert_eq!(n, 1.0, "texel {i},{j} was off the last screen ({uv})");
            }
        }
    }
    assert!(
        kept >= 500 && restarted >= 500,
        "kept {kept}, restarted {restarted}"
    );
}
