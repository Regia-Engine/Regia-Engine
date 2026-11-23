// SPDX-License-Identifier: MIT-0
//! `vfx_lab --decals <dir>`: a quick offscreen preview of scorch, pad marking and tyre ruts.
//!
//! Writes `preview_scorch.png`, `preview_pad.png` and `preview_ruts.png`. The blessed
//! default-look frames are `docs/evidence/decals/snow_setpiece.png` and `mars_setpiece.png`
//! (the `regia_vfx` decal test). Discrete-GPU timing is `docs/evidence/decals/perf.md`.

use std::path::Path;

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::{
    DecalFrame, DecalScene, DecalStrip, VfxCamera, VfxError, VfxProfile, builtin_prefab, measure_decal_pass,
    sample_quadratic,
};

use crate::save_png;

/// Renders the three evidence frames into `dir` and prints the 1080p pass time.
pub fn write_decals(device: &RenderDevice, dir: &Path) -> Result<f32, VfxError> {
    std::fs::create_dir_all(dir).map_err(|err| VfxError::Shader(err.to_string()))?;
    let profile = VfxProfile {
        particle_capacity: 64,
        decal_capacity: 256,
        sort: false,
        light_budget: 0,
        screen_floor_px: 0.0,
        smoke_budget: 0,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        event_capacity: 16,
        child_capacity: 16,
    };
    let mut scene = DecalScene::new(device, profile)?;

    let width = 320u32;
    let height = 180u32;
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 8.0, 9.0), Vec3::ZERO);
    let mut frame = DecalFrame {
        width,
        height,
        camera,
        ground: true,
        checker: true,
        ..DecalFrame::default()
    };
    let pad = builtin_prefab("pad_marking").ok_or_else(|| VfxError::Shader("pad prefab missing".into()))?;
    let scorch = builtin_prefab("scorch").ok_or_else(|| VfxError::Shader("scorch prefab missing".into()))?;
    let (pad_d, pad_s) = pad
        .instantiate(Vec3::new(-1.6, 0.02, 0.2), Vec3::Y, 0.0, None)
        .map_err(|err| VfxError::Shader(err.to_string()))?;
    let (scorch_d, scorch_s) = scorch
        .instantiate(Vec3::new(1.8, 0.02, 0.2), Vec3::Y, 0.0, None)
        .map_err(|err| VfxError::Shader(err.to_string()))?;
    frame.decals = vec![(pad_d, pad_s), (scorch_d, scorch_s)];
    let painted = scene.render(device, &frame)?;
    save_png(&dir.join("preview_pad.png"), &painted, width, height)
        .map_err(|err| VfxError::Shader(err.to_string()))?;

    let camera = VfxCamera::looking_at(Vec3::new(0.0, 9.0, 11.0), Vec3::ZERO);
    let rut = builtin_prefab("rut").ok_or_else(|| VfxError::Shader("rut prefab missing".into()))?;
    let (_, surface) = rut
        .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
        .map_err(|err| VfxError::Shader(err.to_string()))?;
    let mut frame = DecalFrame {
        width,
        height,
        camera,
        ground: true,
        ..DecalFrame::default()
    };
    frame.decals = DecalStrip {
        points: sample_quadratic(
            Vec3::new(-4.0, 0.02, 2.0),
            Vec3::new(0.0, 0.02, -2.0),
            Vec3::new(4.0, 0.02, 2.0),
            16,
        ),
        normal: Vec3::Y,
        half_width: 0.22,
        half_depth: 0.15,
        spacing: 0.4,
        surface,
        lifetime: None,
    }
    .stamps();
    let ruts = scene.render(device, &frame)?;
    save_png(&dir.join("preview_ruts.png"), &ruts, width, height)
        .map_err(|err| VfxError::Shader(err.to_string()))?;

    let camera = VfxCamera::looking_at(Vec3::new(8.0, 6.5, 10.0), Vec3::new(1.0, 0.4, 0.4));
    let mut piece = DecalFrame {
        width: 640,
        height: 360,
        camera,
        ground: true,
        mesh: true,
        mesh_ripple: 0.75,
        checker: true,
        ..DecalFrame::default()
    };
    let (ground_scorch, gs) = scorch
        .instantiate(Vec3::new(0.2, 0.02, 0.4), Vec3::Y, 0.4, None)
        .map_err(|err| VfxError::Shader(err.to_string()))?;
    let (mesh_scorch, ms) = scorch
        .instantiate(Vec3::new(2.2, 1.02, 0.0), Vec3::Y, 0.0, None)
        .map_err(|err| VfxError::Shader(err.to_string()))?;
    let (pad_d, pad_s) = pad
        .instantiate(Vec3::new(0.4, 0.02, 2.4), Vec3::Y, 0.2, None)
        .map_err(|err| VfxError::Shader(err.to_string()))?;
    let (_, rut_s) = rut
        .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
        .map_err(|err| VfxError::Shader(err.to_string()))?;
    piece.decals = vec![(ground_scorch, gs), (mesh_scorch, ms), (pad_d, pad_s)];
    piece.decals.extend(
        DecalStrip {
            points: sample_quadratic(
                Vec3::new(-3.0, 0.02, 3.2),
                Vec3::new(0.2, 0.02, 1.2),
                Vec3::new(3.4, 0.02, 3.0),
                14,
            ),
            normal: Vec3::Y,
            half_width: 0.2,
            half_depth: 0.14,
            spacing: 0.45,
            surface: rut_s,
            lifetime: None,
        }
        .stamps(),
    );
    let shot = scene.render(device, &piece)?;
    save_png(&dir.join("preview_scorch.png"), &shot, 640, 360)
        .map_err(|err| VfxError::Shader(err.to_string()))?;

    measure_decal_pass(device, 256, 1920, 1080)
}
