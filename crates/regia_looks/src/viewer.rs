// SPDX-License-Identifier: MPL-2.0
//! The looks on `regia_viewer` (feature `viewer`): shader hooks for every family, the
//! `LookMaterial -> ViewerMaterial` mapping, meshes and the studio as a viewer environment.
//!
//! ```no_run
//! # fn demo(device: wgpu::Device, queue: wgpu::Queue) -> Result<(), Box<dyn std::error::Error>> {
//! use regia_looks::{viewer, CutType, GemCut, Library};
//! let mut config = regia_viewer::ViewerConfig::new(wgpu::TextureFormat::Rgba8UnormSrgb);
//! config.hooks = viewer::hooks();
//! let mut r = regia_viewer::ViewerRenderer::new(device, queue, config)?;
//! r.set_environment(&viewer::studio_env(&regia_looks::Studio::default())?);
//! let cut = GemCut::new(CutType::RoundBrilliant, [3.0, 3.0]);
//! let mesh = r.upload_mesh(&viewer::mesh_data(&cut.mesh()))?;
//! let lib = Library::builtin()?;
//! let stone = viewer::create_material(&mut r, &lib.look("diamond-white", None)?, cut.stone_size())?;
//! r.add_draw(mesh, stone, &[glam::Affine3A::IDENTITY])?;
//! # Ok(()) }
//! ```

use regia_viewer::{
    AnodizeFilm, AnodizedParams, BakedEnv, EnvError, KeyLight, LutHandle, MaterialBlock, MaterialHandle,
    ShaderFamily, ShaderHooks, ViewerCamera, ViewerConfig, ViewerError, ViewerMaterial, ViewerMeshData,
    ViewerRenderer, ViewerStyle, ViewerVertex,
};

use crate::board::LookScene;

use crate::env::Studio;
use crate::material::{LookKind, LookMaterial, StoneSize};
use crate::mesh::LookMesh;

const COMMON: &str = include_str!("shaders/hooks/common.wgsl");
const SHARED_STEP: &str = include_str!("shaders/shared/step_cut.wgsl");
const SHARED_OPAL: &str = include_str!("shaders/shared/opal_play.wgsl");
const SHARED_GRAIN: &str = include_str!("shaders/shared/metal_grain.wgsl");

/// The metal hook (also anodized metal), without the shared helpers.
pub const HOOK_METAL: &str = include_str!("shaders/hooks/metal.wgsl");
/// The faceted gem hook, without the shared helpers.
pub const HOOK_GEM: &str = include_str!("shaders/hooks/gem.wgsl");
/// The frosted stone hook, without the shared helpers.
pub const HOOK_FROSTED: &str = include_str!("shaders/hooks/frosted.wgsl");
/// The cabochon hook, without the shared helpers.
pub const HOOK_CABOCHON: &str = include_str!("shaders/hooks/cabochon.wgsl");
/// The opal hook, without the shared helpers.
pub const HOOK_OPAL: &str = include_str!("shaders/hooks/opal.wgsl");

/// The complete hook text of a family: the shared helpers, then the family's hook.
pub fn hook_source(family: ShaderFamily) -> String {
    let (shared, hook) = match family {
        ShaderFamily::Metal => (SHARED_GRAIN, HOOK_METAL),
        ShaderFamily::Gem => (SHARED_STEP, HOOK_GEM),
        ShaderFamily::Frosted => (SHARED_STEP, HOOK_FROSTED),
        ShaderFamily::Cabochon => ("", HOOK_CABOCHON),
        ShaderFamily::Opal => (SHARED_OPAL, HOOK_OPAL),
    };
    format!("{COMMON}\n{shared}\n{hook}")
}

/// Every family's looks hook, for `ViewerConfig::hooks`.
pub fn hooks() -> ShaderHooks {
    ShaderFamily::ALL
        .iter()
        .fold(ShaderHooks::default(), |h, f| h.with_hook(*f, hook_source(*f)))
}

/// The viewer family of a look kind.
pub fn family(kind: LookKind) -> ShaderFamily {
    match kind {
        LookKind::Metal => ShaderFamily::Metal,
        LookKind::Gem => ShaderFamily::Gem,
        LookKind::Cabochon => ShaderFamily::Cabochon,
        LookKind::Opal => ShaderFamily::Opal,
        LookKind::Frosted => ShaderFamily::Frosted,
    }
}

/// The row a lookup handle names (read through the viewer's own block packing).
fn lut_row(lut: LutHandle) -> u32 {
    let block = ViewerMaterial::Anodized(AnodizedParams {
        roughness: 0.0,
        film: AnodizeFilm::Lut(lut),
    })
    .to_block();
    block.p0[1].max(0.0) as u32
}

/// The viewer material of a look: a `Custom` block for the looks hook of its family. `lut` is the
/// resident row of an anodized metal's color table ([`create_material`] uploads it).
pub fn viewer_material(look: &LookMaterial, stone: StoneSize, lut: Option<LutHandle>) -> ViewerMaterial {
    let p = look.params(stone, lut.map(lut_row));
    ViewerMaterial::Custom {
        family: family(look.kind()),
        block: MaterialBlock {
            base: p.p0,
            p0: p.p1,
            p1: p.p2,
            p2: p.p3,
        },
    }
}

/// Creates a material slot for a look, uploading its anodize table first when it has one.
///
/// # Errors
/// [`ViewerError::LutFull`] or [`ViewerError::BadLut`] from the table upload.
pub fn create_material(
    renderer: &mut ViewerRenderer,
    look: &LookMaterial,
    stone: StoneSize,
) -> Result<MaterialHandle, ViewerError> {
    let lut = look.anodize_lut().map(|t| renderer.add_lut(t)).transpose()?;
    Ok(renderer.create_material(&viewer_material(look, stone, lut)))
}

/// A look mesh as viewer mesh data (no texture coordinates).
pub fn mesh_data(mesh: &LookMesh) -> ViewerMeshData {
    let vertices = (0..mesh.positions.len())
        .map(|i| ViewerVertex {
            position: mesh.positions[i],
            normal: mesh.normals[i],
            uv: [0.0, 0.0],
            facet: mesh.facets[i] as f32,
        })
        .collect();
    ViewerMeshData {
        vertices,
        indices: mesh.indices.clone(),
    }
}

/// Real spherical harmonics up to order 2 in the viewer's coefficient order.
fn sh_basis(d: glam::Vec3) -> [f32; 9] {
    let (x, y, z) = (d.x, d.y, d.z);
    [
        0.282_095,
        0.488_603 * y,
        0.488_603 * z,
        0.488_603 * x,
        1.092_548 * x * y,
        1.092_548 * y * z,
        0.315_392 * (3.0 * z * z - 1.0),
        1.092_548 * x * z,
        0.546_274 * (x * x - y * y),
    ]
}

/// The studio as a viewer environment: the same procedural radiance, prefiltered for the viewer's
/// roughness-to-mip mapping (`lod = sqrt(roughness) * last mip`), plus its diffuse light as
/// cosine-convolved second-order spherical harmonics divided by pi.
///
/// # Errors
/// [`EnvError`] when the studio's size is not one the viewer accepts.
pub fn studio_env(studio: &Studio) -> Result<BakedEnv, EnvError> {
    let _span = tracing::debug_span!("looks_studio_env", size = studio.base_size).entered();
    let last = studio.mips.saturating_sub(1).max(1) as f32;
    // Roughness r lands on mip sqrt(r) * last; the lobe width is ~1.1 r^2 radians.
    let levels = studio.build_with(|level| {
        let t = level as f32 / last;
        1.1 * t.powi(4)
    });
    let mut texels = Vec::new();
    for faces in &levels {
        for face in faces {
            texels.extend(
                face.iter()
                    .map(|t| regia_viewer::env::encode_rgb9e5([t[0], t[1], t[2]])),
            );
        }
    }
    // Diffuse: project the radiance on the basis with solid-angle weights over a 32 x 32 grid per face.
    let n = 32u32;
    let mut sh = [[0.0f32; 3]; 9];
    for face in 0..6 {
        for y in 0..n {
            for x in 0..n {
                let u = (x as f32 + 0.5) / n as f32 * 2.0 - 1.0;
                let v = (y as f32 + 0.5) / n as f32 * 2.0 - 1.0;
                let d = crate::env::face_dir(face, u, v);
                let weight = 4.0 / (n * n) as f32 / (1.0 + u * u + v * v).powf(1.5);
                let l = studio.radiance(d);
                for (k, b) in sh_basis(d).iter().enumerate() {
                    sh[k][0] += l.x * b * weight;
                    sh[k][1] += l.y * b * weight;
                    sh[k][2] += l.z * b * weight;
                }
            }
        }
    }
    // Cosine lobe (pi, 2 pi / 3, pi / 4 per band), divided by pi.
    let band = [1.0, 2.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0, 0.25, 0.25, 0.25, 0.25, 0.25];
    for (k, c) in sh.iter_mut().enumerate() {
        for ch in c.iter_mut() {
            *ch *= band[k];
        }
    }
    BakedEnv::from_parts(studio.base_size, studio.mips, sh, texels)
}

/// The background of looks stills (linear): a dark neutral gray, so bright stones read.
pub const BOARD_CLEAR: [f32; 4] = [0.11, 0.112, 0.118, 1.0];

/// A viewer renderer set up for the looks: every family's looks hook, the looks studio as the
/// environment and its three key lights, the board background.
///
/// # Errors
/// [`ViewerError`] when the renderer or the studio cannot be built.
pub fn looks_renderer(
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    sample_count: u32,
    studio: &Studio,
) -> Result<ViewerRenderer, ViewerError> {
    let mut config = ViewerConfig::new(format);
    config.sample_count = sample_count;
    config.hooks = hooks();
    let mut r = ViewerRenderer::new(device, queue, config)?;
    r.set_environment(&studio_env(studio)?);
    r.set_style(&ViewerStyle {
        clear_color: BOARD_CLEAR,
        key_lights: key_lights(studio),
        ..ViewerStyle::default()
    });
    Ok(r)
}

/// The studio's three key lights as the viewer's style takes them: the looks' glints come from these.
pub fn key_lights(studio: &Studio) -> [KeyLight; 3] {
    studio.keys.map(|k| KeyLight {
        dir: k.dir.to_array(),
        radiance: k.color.to_array(),
    })
}

/// Adds every item of a scene as a draw, and aims the camera: the scene's own camera, or a close-up
/// `(x_mm, y_mm, fov_deg)` straight on at that board point.
///
/// # Errors
/// [`ViewerError`] for a mesh the viewer refuses or a full lookup atlas.
pub fn draw_scene(
    r: &mut ViewerRenderer,
    scene: &LookScene,
    close_up: Option<(f32, f32, f32)>,
) -> Result<(), ViewerError> {
    let _span = tracing::debug_span!("looks_draw_scene", items = scene.items.len()).entered();
    for item in &scene.items {
        let mesh = r.upload_mesh(&mesh_data(&item.mesh))?;
        let material = create_material(r, &item.material, item.stone)?;
        r.add_draw(mesh, material, &[glam::Affine3A::from_mat4(item.model)])?;
    }
    let c = scene.camera;
    // The viewer's field of view spans the shorter side; stills here are landscape, so the scene's
    // vertical field of view carries over.
    let (eye, target, fov_deg) = match close_up {
        Some((x, y, fov)) => ([x, y, c.eye.z], [x, y, 0.0], fov),
        None => (c.eye.to_array(), c.target.to_array(), c.fov_y_deg),
    };
    let distance = (c.eye - c.target).length().max(1.0);
    r.set_camera(&ViewerCamera {
        eye,
        target,
        up: [0.0, 1.0, 0.0],
        fov_deg,
        near_mm: distance * 0.05,
        far_mm: distance * 8.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_maps_to_its_family_with_the_looks_block() {
        let lib = crate::Library::builtin().expect("library");
        let stone = StoneSize::round(3.0, 1.8);
        for (id, fam) in [
            ("gold-yellow-18k", ShaderFamily::Metal),
            ("diamond-white", ShaderFamily::Gem),
            ("cz-frosted", ShaderFamily::Frosted),
            ("opal-white", ShaderFamily::Opal),
        ] {
            let m = viewer_material(&lib.look(id, None).expect("look"), stone, None);
            assert_eq!(m.family(), fam, "{id}");
        }
        let gem = lib.look("diamond-white", None).expect("diamond");
        let block = viewer_material(&gem, stone, None).to_block();
        assert_eq!(block.p0[0], 2.417, "n_d in p0.x");
        assert_eq!(block.p1[1], 1.5, "half length in p1.y");
        let metal = viewer_material(&lib.look("platinum", None).expect("pt"), stone, None).to_block();
        assert_eq!(metal.p0[3], -1.0, "no anodize row");
    }

    #[test]
    fn the_studio_converts_to_a_viewer_environment() {
        let studio = Studio {
            base_size: 16,
            mips: 5,
            ..Studio::default()
        };
        let env = studio_env(&studio).expect("env");
        assert_eq!(env.face_size(), 16);
        assert_eq!(env.mip_count(), 5);
        // The studio is brighter above than below: the y band of the diffuse light is positive.
        assert!(env.sh()[1][1] > 0.0, "{:?}", env.sh());
        assert!(env.sh()[0][1] > 0.0);
    }

    #[test]
    fn hooks_replace_every_family() {
        let h = hooks();
        for f in ShaderFamily::ALL {
            assert!(h.hook(f).contains("looks_hash01"), "{f:?}");
            assert!(h.family_source(f).contains("fn shade_surface"));
        }
    }

    #[test]
    fn mesh_data_keeps_facets() {
        let cut = crate::GemCut::new(crate::CutType::Pear, [3.0, 2.0]);
        let data = mesh_data(&cut.mesh());
        assert!(data.validate().is_ok());
        assert!(data.vertices.iter().any(|v| v.facet > 10.0));
    }
}
