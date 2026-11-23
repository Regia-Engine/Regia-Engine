// SPDX-License-Identifier: MPL-2.0
//! Distance fog on the runtime forward path. A uniform tint fails: far texels of a
//! rising height strip move toward the fog colour more than near ones. Black and
//! blown-out frames fail. Headless device creation is required (lavapipe).

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    RenderDevice,
};

/// A rising strip along −Z (near at z ≈ −2, far at z ≈ −80). Not a single flat quad:
/// height grows with distance so the surface is a coarse heightfield.
fn height_strip() -> (Vec<LabVertex>, Vec<u32>) {
    let nx = 6u32;
    let nz = 28u32;
    let mut vertices = Vec::with_capacity(((nx + 1) * (nz + 1)) as usize);
    for iz in 0..=nz {
        for ix in 0..=nx {
            let u = ix as f32 / nx as f32;
            let v = iz as f32 / nz as f32;
            let x = (u - 0.5) * 36.0;
            let z = -2.0 - v * 78.0;
            let y = v * 7.0 + (u * std::f32::consts::TAU).sin() * 0.35;
            vertices.push(LabVertex::from_pos_normal_uv([x, y, z], [0.0, 1.0, 0.0], [u, v]));
        }
    }
    let mut indices = Vec::with_capacity((nx * nz * 6) as usize);
    let stride = nx + 1;
    for iz in 0..nz {
        for ix in 0..nx {
            let v00 = iz * stride + ix;
            let v10 = v00 + 1;
            let v01 = v00 + stride;
            let v11 = v01 + 1;
            // CCW from +Y, matching the lit floor in `tests/shadows.rs`.
            // (v00, v10, v01) has a +Y cross product; the opposite winding
            // flips the face so the sun term is zero.
            indices.extend_from_slice(&[v00, v10, v01, v10, v11, v01]);
        }
    }
    (vertices, indices)
}

fn scene() -> LabScene {
    let (vertices, indices) = height_strip();
    LabScene {
        width: 160,
        height: 90,
        eye: [0.0, 14.0, 6.0],
        target: [0.0, 3.0, -36.0],
        fov_y: 0.9,
        near: 0.1,
        far: 220.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.15, -0.72, -0.55], [1.15, 1.05, 0.92], 90_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.32, 0.40, 0.24, 1.0],
            roughness: 0.9,
            metallic: 0.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn luma(px: &[u8]) -> f64 {
    0.2126 * px[0] as f64 + 0.7152 * px[1] as f64 + 0.0722 * px[2] as f64
}

fn mean_luma(pixels: &[u8]) -> f64 {
    let n = pixels.len() / 4;
    let mut sum = 0.0;
    for i in 0..n {
        sum += luma(&pixels[i * 4..i * 4 + 3]);
    }
    sum / n as f64
}

fn near_black_frac(pixels: &[u8]) -> f64 {
    let n = pixels.len() / 4;
    let mut c = 0usize;
    for i in 0..n {
        if pixels[i * 4] < 8 && pixels[i * 4 + 1] < 8 && pixels[i * 4 + 2] < 8 {
            c += 1;
        }
    }
    c as f64 / n as f64
}

fn near_white_frac(pixels: &[u8]) -> f64 {
    let n = pixels.len() / 4;
    let mut c = 0usize;
    for i in 0..n {
        if pixels[i * 4] > 250 && pixels[i * 4 + 1] > 250 && pixels[i * 4 + 2] > 250 {
            c += 1;
        }
    }
    c as f64 / n as f64
}

/// Ground texels in the unfogged frame: green-dominant and brighter than the clear colour.
fn is_ground(px: &[u8]) -> bool {
    let r = px[0] as i32;
    let g = px[1] as i32;
    let b = px[2] as i32;
    g > 18 && g + 6 > r && g >= b && luma(px) > 14.0
}

fn rgb_delta(a: &[u8], b: &[u8]) -> f64 {
    let dr = a[0] as f64 - b[0] as f64;
    let dg = a[1] as f64 - b[1] as f64;
    let db = a[2] as f64 - b[2] as f64;
    (dr * dr + dg * dg + db * db).sqrt()
}

/// How much bluer (relative to red) `fogged` is than `clear`. Outdoor haze is blue-biased.
fn blue_shift(clear: &[u8], fogged: &[u8]) -> f64 {
    let clear_br = clear[2] as f64 - clear[0] as f64;
    let fog_br = fogged[2] as f64 - fogged[0] as f64;
    fog_br - clear_br
}

#[test]
fn distance_fog_shifts_far_texels_more_than_near_ones() {
    let device = RenderDevice::headless().expect("lavapipe headless device");
    let scene = scene();
    let mut clear_scene = scene.as_frame_scene();
    clear_scene.fog = FogSettings::default();
    let clear = FrameRenderer::new()
        .render_offscreen(&device, &clear_scene)
        .expect("unfogged frame");

    let mut fogged_scene = scene.as_frame_scene();
    fogged_scene.fog = FogSettings::outdoor_haze();
    let fogged = FrameRenderer::new()
        .render_offscreen(&device, &fogged_scene)
        .expect("fogged frame");

    let diff = clear
        .pixels
        .iter()
        .zip(fogged.pixels.iter())
        .filter(|(a, b)| a != b)
        .count();
    assert!(diff > 0, "fog on/off must differ (changed bytes {diff})");

    let width = clear.width;
    let height = clear.height;
    let mut min_y = height;
    let mut max_y = 0u32;
    for y in 0..height {
        for x in width / 5..width * 4 / 5 {
            let i = ((y * width + x) * 4) as usize;
            if is_ground(&clear.pixels[i..i + 3]) {
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    assert!(
        max_y > min_y + 8,
        "height strip must cover a vertical span of ground (y {min_y}..{max_y})"
    );

    let span = (max_y - min_y).max(1);
    let mut near_delta = 0.0;
    let mut near_blue = 0.0;
    let mut near_n = 0u32;
    let mut far_delta = 0.0;
    let mut far_blue = 0.0;
    let mut far_n = 0u32;
    for y in min_y..=max_y {
        let t = (y - min_y) as f32 / span as f32;
        // Image y grows downward: the top of the ground band is farther from the camera.
        let bucket = if t < 0.28 {
            Some(true) // far
        } else if t > 0.72 {
            Some(false) // near
        } else {
            None
        };
        let Some(far) = bucket else { continue };
        for x in width / 3..width * 2 / 3 {
            let i = ((y * width + x) * 4) as usize;
            if !is_ground(&clear.pixels[i..i + 3]) {
                continue;
            }
            let delta = rgb_delta(&clear.pixels[i..i + 3], &fogged.pixels[i..i + 3]);
            let blue = blue_shift(&clear.pixels[i..i + 3], &fogged.pixels[i..i + 3]);
            if far {
                far_delta += delta;
                far_blue += blue;
                far_n += 1;
            } else {
                near_delta += delta;
                near_blue += blue;
                near_n += 1;
            }
        }
    }
    assert!(
        near_n > 8 && far_n > 8,
        "need near ({near_n}) and far ({far_n}) ground samples"
    );
    let near_delta = near_delta / f64::from(near_n);
    let far_delta = far_delta / f64::from(far_n);
    let near_blue = near_blue / f64::from(near_n);
    let far_blue = far_blue / f64::from(far_n);
    assert!(
        far_delta > near_delta * 1.35,
        "far ground must move more than near ground (far {far_delta:.2} vs near {near_delta:.2}); a uniform tint fails this"
    );
    assert!(
        far_blue > near_blue + 1.5,
        "far ground must shift toward the blue haze more than near ground (far {far_blue:.2} vs near {near_blue:.2})"
    );

    for (label, pixels) in [("clear", &clear.pixels), ("fog", &fogged.pixels)] {
        let black = near_black_frac(pixels);
        let white = near_white_frac(pixels);
        assert!(black < 0.85, "{label}: too many near-black pixels ({black:.2})");
        assert!(white < 0.25, "{label}: too many near-white pixels ({white:.2})");
        let mean = mean_luma(pixels);
        assert!(
            mean > 8.0 && mean < 220.0,
            "{label}: mean luma {mean:.1} is black or blown out"
        );
    }
}
