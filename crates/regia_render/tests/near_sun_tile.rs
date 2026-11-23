// SPDX-License-Identifier: MPL-2.0
//! The near sun tile (ADR-164): mesh receivers within `NEAR_SUN_RANGE_M` of the eye take a
//! finer sun shadow than cascade 0, the tile is skipped when nothing is that close, and a caster
//! far up-sun still shadows them.

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, RenderDevice, ShadowCast,
};

fn vertex(position: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex {
        position,
        normal,
        uv: [0.0, 0.0],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// Axis-aligned box as 12 triangles (flat normals per face).
fn cuboid(min: [f32; 3], max: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    let [x0, y0, z0] = min;
    let [x1, y1, z1] = max;
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        (
            [0.0, 1.0, 0.0],
            [[x0, y1, z0], [x0, y1, z1], [x1, y1, z1], [x1, y1, z0]],
        ),
        (
            [0.0, -1.0, 0.0],
            [[x0, y0, z0], [x1, y0, z0], [x1, y0, z1], [x0, y0, z1]],
        ),
        (
            [1.0, 0.0, 0.0],
            [[x1, y0, z0], [x1, y1, z0], [x1, y1, z1], [x1, y0, z1]],
        ),
        (
            [-1.0, 0.0, 0.0],
            [[x0, y0, z0], [x0, y0, z1], [x0, y1, z1], [x0, y1, z0]],
        ),
        (
            [0.0, 0.0, 1.0],
            [[x0, y0, z1], [x1, y0, z1], [x1, y1, z1], [x0, y1, z1]],
        ),
        (
            [0.0, 0.0, -1.0],
            [[x0, y0, z0], [x0, y1, z0], [x1, y1, z0], [x1, y0, z0]],
        ),
    ];
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (normal, corners) in faces {
        let base = vertices.len() as u32;
        vertices.extend(corners.iter().map(|&p| vertex(p, normal)));
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

struct Shot {
    pixels: Vec<u8>,
    live: bool,
}

/// A 6 m ground slab and a thin bar 0.6 m over it, seen from `eye_dist` metres, the sun high
/// and slanted so the bar's shadow edge crosses the shadow texel grid diagonally. `tile` is the
/// near tile edge (0 = off); `wall` adds a wall far up-sun under a low sun; `bar_casts` = the
/// bar casts.
fn shot(device: &RenderDevice, eye_dist: f32, tile: u32, wall: bool, bar_casts: bool) -> Shot {
    let (ground_v, ground_i) = cuboid([-3.0, -0.2, -3.0], [3.0, 0.0, 3.0]);
    let (bar_v, bar_i) = cuboid([-1.2, 0.6, -0.03], [1.2, 0.66, 0.03]);
    let (wall_v, wall_i) = cuboid([-100.0, 0.0, 150.0], [100.0, 80.0, 160.0]);
    let material: &'static LabMaterial = Box::leak(Box::new(LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.8,
        ..LabMaterial::EMPTY
    }));
    let quality = FrameQuality {
        csm_cascade_count: 4,
        csm_resolution: 2048,
        near_sun_tile: tile,
        ..FrameQuality::LAB
    };
    let identity = regia_core::glam::Mat4::IDENTITY.to_cols_array_2d();
    let mesh =
        |key: u64, vertices: &'static [LabVertex], indices: &'static [u32], shadow: ShadowCast| FrameMesh {
            is_static: false,
            mesh_key: key,
            material_key: 1,
            object_key: key,
            vertices,
            indices,
            material,
            model: identity,
            joints: &[],
            receiver_layer: 0,
            shadow,
            uv1: &[],
            lod_dither: 0.0,
        };
    let leak_v = |v: Vec<LabVertex>| -> &'static [LabVertex] { Box::leak(v.into_boxed_slice()) };
    let leak_i = |v: Vec<u32>| -> &'static [u32] { Box::leak(v.into_boxed_slice()) };
    let mut meshes = vec![
        mesh(1, leak_v(ground_v), leak_i(ground_i), ShadowCast::All),
        mesh(
            2,
            leak_v(bar_v),
            leak_i(bar_i),
            if bar_casts {
                ShadowCast::All
            } else {
                ShadowCast::Off
            },
        ),
    ];
    if wall {
        meshes.push(mesh(3, leak_v(wall_v), leak_i(wall_i), ShadowCast::All));
    }
    let scene = FrameScene {
        width: 256,
        height: 192,
        eye: [0.0, eye_dist * 0.8, eye_dist * 0.6],
        target: [0.0, 0.0, -0.4],
        fov_y: 0.9,
        near: 0.05,
        far: 2000.0,
        depth_prepass: false,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.1,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: if wall {
            // Low sun from +z: the wall 150 m up-sun shadows everything in view.
            DirectionalLightUniform::new([0.05, -0.4, -1.0], [1.0, 1.0, 1.0], 1.0)
        } else {
            DirectionalLightUniform::new([0.35, -1.0, -0.25], [1.0, 1.0, 1.0], 1.0)
        },
        local_lights: &[],
        quality,
        meshes,
    };
    let mut renderer = FrameRenderer::new();
    let pixels = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    let live = renderer.shadow_cache_report().near_sun_tile;
    Shot { pixels, live }
}

fn luma(p: &[u8]) -> f32 {
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

/// Pixels in the shadow's soft edge (between a quarter and three quarters of the way from
/// shadowed to lit), against the frame without the bar's shadow.
fn edge_pixels(lit: &[u8], shot: &[u8]) -> (usize, usize) {
    let mut edge = 0;
    let mut dark = 0;
    for (a, b) in lit.as_chunks::<4>().0.iter().zip(shot.as_chunks::<4>().0) {
        let (la, lb) = (luma(a), luma(b));
        if la < 20.0 {
            continue;
        }
        let r = lb / la;
        if r < 0.35 {
            dark += 1;
        } else if r < 0.85 {
            edge += 1;
        }
    }
    (edge, dark)
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_close_receiver_takes_the_near_tile_and_a_sharper_edge() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lit = shot(&device, 2.5, 0, false, false);
    let off = shot(&device, 2.5, 0, false, true);
    let on = shot(&device, 2.5, 2048, false, true);
    assert!(!off.live, "tile 0 is off");
    assert!(on.live, "the slab 2.5 m away is within range");
    let (edge_c0, dark_c0) = edge_pixels(&lit.pixels, &off.pixels);
    let (edge_tile, dark_tile) = edge_pixels(&lit.pixels, &on.pixels);
    eprintln!("cascade 0: edge {edge_c0} dark {dark_c0}; near tile: edge {edge_tile} dark {dark_tile}");
    assert!(
        dark_c0 > 200 && dark_tile > 200,
        "the bar must shadow the slab: {dark_c0} {dark_tile}"
    );
    assert!(
        (edge_tile as f32) < edge_c0 as f32 * 0.75,
        "the near tile's edge must be sharper than cascade 0's: {edge_tile} vs {edge_c0}"
    );
    // The same shadowed area (umbra plus edge) within a tenth: the tile sharpens the shadow, it
    // does not move or shrink it (lavapipe: 936 vs 937 pixels).
    let (a, b) = ((dark_c0 + edge_c0) as f32, (dark_tile + edge_tile) as f32);
    assert!((a - b).abs() < 0.1 * a.max(b), "shadowed area {a} vs {b}");
}

#[test]
fn the_near_tile_is_skipped_with_nothing_in_range() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let off = shot(&device, 12.0, 0, false, true);
    let on = shot(&device, 12.0, 2048, false, true);
    assert!(!on.live, "nothing within 4 m: the tile must not draw");
    assert_eq!(
        off.pixels, on.pixels,
        "a skipped tile leaves the frame bit-identical"
    );
}

#[test]
fn a_far_up_sun_caster_still_shadows_near_receivers() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let on = shot(&device, 2.5, 2048, true, true);
    assert!(on.live);
    let lit = shot(&device, 2.5, 2048, false, true);
    let mean = |p: &[u8]| p.as_chunks::<4>().0.iter().map(|c| luma(c)).sum::<f32>() / (p.len() / 4) as f32;
    let (walled, open) = (mean(&on.pixels), mean(&lit.pixels));
    eprintln!("mean luma: wall {walled:.1}, open {open:.1}");
    assert!(
        walled < open * 0.5,
        "the wall 150 m up-sun must shadow the slab: {walled} vs {open}"
    );
}
