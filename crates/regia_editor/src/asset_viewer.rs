// SPDX-License-Identifier: MPL-2.0
//! Asset viewers (`docs/specs/19-editor.md` § 2): mesh (LOD chain, parts and materials, an
//! orbit render through the frame renderer), texture (mip chain, channel isolation) and sound
//! (waveform, playback through `regia_audio`). The mesh viewer draws overlays over its render
//! (ADR-7469): the UV0 and UV1 layouts, vertex normals, and the cooked collider.
//!
//! The pure parts — loading, LOD simplification, mip generation, channel isolation, WAV
//! decoding, waveform drawing — are functions over data, used by the [`asset_viewer_panel`],
//! the agent protocol (`editor.view_asset`, `editor.play_sound`) and the tests alike.
//!
//! LODs: the cook builds a mesh's LOD chain with meshoptimizer's simplifier when the asset's
//! `.meta` enables `generate_lods` (`regia_asset_pipeline`, `mesh_cook`). The viewer previews
//! the same simplifier with the cook's default chain (triangle ratios 1, ½, ¼, ⅛; 1 % target
//! error, locked borders) per part, so each LOD keeps its materials.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::{Resource, World, bevy_ecs};
use regia_render::{CpuMesh, FrameMesh, FrameQuality, FrameScene, LabTexture, RenderDevice};

use crate::mesh_source::{LoadedModel, MeshLibrary, ModelPart, resolve_asset_path};

/// What a viewer shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    /// glTF / GLB mesh (or a `builtin:` primitive).
    Mesh,
    /// Image file.
    Texture,
    /// Sound file.
    Sound,
    /// Animation clip(s): a `.bvh` motion (spec addendum § 8.8, ADR-7788).
    Animation,
}

impl AssetKind {
    /// Stable name (`mesh`, `texture`, `sound`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Mesh => "mesh",
            Self::Texture => "texture",
            Self::Sound => "sound",
            Self::Animation => "animation",
        }
    }
}

/// The viewer kind for a path, by extension.
#[must_use]
pub fn kind_of(path: &str) -> Option<AssetKind> {
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("builtin:") || lower.ends_with(".gltf") || lower.ends_with(".glb") {
        Some(AssetKind::Mesh)
    } else if [".png", ".jpg", ".jpeg", ".tga", ".bmp", ".hdr", ".exr"]
        .iter()
        .any(|ext| lower.ends_with(ext))
    {
        Some(AssetKind::Texture)
    } else if sound_extensions().iter().any(|ext| lower.ends_with(ext)) {
        Some(AssetKind::Sound)
    } else if is_animation_path(&lower) {
        Some(AssetKind::Animation)
    } else {
        None
    }
}

/// Every sound extension the preview decodes: what `regia_audio`'s bounded whole-file decoder
/// reads (WAV, Ogg Vorbis, FLAC, MP3 — the kira/symphonia codecs the runtime plays).
pub fn sound_extensions() -> [&'static str; 4] {
    [".wav", ".ogg", ".flac", ".mp3"]
}

// ---------------------------------------------------------------------------------------------
// Mesh
// ---------------------------------------------------------------------------------------------

/// The default LOD chain the cook uses (`MeshLodSettings { count: 4, ratios: [] }`).
pub const LOD_RATIOS: [f32; 4] = [1.0, 0.5, 0.25, 0.125];

/// One LOD of a model: its parts (same materials as LOD 0) and its triangle count.
#[derive(Debug, Clone)]
pub struct ModelLod {
    /// The target triangle ratio.
    pub ratio: f32,
    /// Parts with simplified indices.
    pub parts: Vec<ModelPart>,
    /// Triangles over all parts.
    pub triangles: usize,
}

/// The LOD chain of a model: LOD 0 is the model; each further level simplifies the previous
/// one (meshoptimizer `simplify`, locked borders, 1 % error) towards its ratio. A level that
/// could not get smaller ends the chain (as in the cook).
#[must_use]
pub fn build_lods(model: &LoadedModel) -> Vec<ModelLod> {
    let mut lods = vec![ModelLod {
        ratio: 1.0,
        parts: model.parts.clone(),
        triangles: model.triangles(),
    }];
    let mut previous: Vec<Vec<u32>> = model.parts.iter().map(|p| p.mesh.indices.clone()).collect();
    for ratio in LOD_RATIOS.iter().skip(1) {
        let mut parts = Vec::new();
        let mut shrank = false;
        let mut next_indices = Vec::new();
        for (part, prev) in model.parts.iter().zip(&previous) {
            let positions: Vec<[f32; 3]> = part.mesh.vertices.iter().map(|v| v.position).collect();
            let target = ((part.mesh.indices.len() as f32 * ratio) as usize / 3 * 3).max(3);
            let simplified = meshopt::simplify_decoder(
                prev,
                &positions,
                target,
                0.01,
                meshopt::SimplifyOptions::LockBorder,
                None,
            );
            let indices = if simplified.len() < prev.len() && !simplified.is_empty() {
                shrank = true;
                simplified
            } else {
                prev.clone()
            };
            parts.push(ModelPart {
                mesh: Arc::new(CpuMesh {
                    vertices: part.mesh.vertices.clone(),
                    indices: indices.clone(),
                }),
                material: Arc::clone(&part.material),
                material_name: part.material_name.clone(),
            });
            next_indices.push(indices);
        }
        if !shrank {
            break;
        }
        let triangles = parts.iter().map(|p| p.mesh.indices.len() / 3).sum();
        lods.push(ModelLod {
            ratio: *ratio,
            parts,
            triangles,
        });
        previous = next_indices;
    }
    lods
}

/// The viewer stage's orbit camera around a bounding box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrbitCamera {
    /// Eye position.
    pub eye: Vec3,
    /// Look-at point (the bounds' center).
    pub target: Vec3,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
}

impl OrbitCamera {
    /// The camera framing `bounds` from `yaw`/`pitch` (radians), as [`render_model`] uses it.
    #[must_use]
    pub fn framing(bounds: (Vec3, Vec3), yaw: f32, pitch: f32) -> Self {
        let (min, max) = bounds;
        let center = (min + max) * 0.5;
        let radius = ((max - min).length() * 0.5).max(0.05);
        let fov_y = 40f32.to_radians();
        let distance = radius / (fov_y * 0.5).sin() * 1.15;
        let dir = Vec3::new(pitch.cos() * yaw.sin(), pitch.sin(), pitch.cos() * yaw.cos());
        Self {
            eye: center + dir * distance,
            target: center,
            fov_y,
            near: (distance - radius * 2.0).max(distance * 0.01),
            far: distance * 40.0,
        }
    }

    /// Where a world point lands in a `size` image (pixels from the top left), or `None`
    /// behind the camera.
    #[must_use]
    pub fn project(&self, point: Vec3, size: [f32; 2]) -> Option<[f32; 2]> {
        // A right-handed view basis (Y up) and a symmetric perspective: depth does not matter here.
        let forward = (self.target - self.eye).normalize_or_zero();
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(forward);
        let d = point - self.eye;
        let depth = d.dot(forward);
        if depth <= 1e-6 {
            return None;
        }
        let aspect = size[0].max(1.0) / size[1].max(1.0);
        let half = (self.fov_y * 0.5).tan();
        let ndc_x = d.dot(right) / (depth * half * aspect);
        let ndc_y = d.dot(up) / (depth * half);
        Some([(ndc_x * 0.5 + 0.5) * size[0], (0.5 - ndc_y * 0.5) * size[1]])
    }
}

/// Renders a model's parts on the viewer stage: centred, framed by its bounds, yaw/pitch orbit,
/// key light, backdrop sky, editor quality.
#[must_use]
pub fn render_model(
    device: &RenderDevice,
    parts: &[ModelPart],
    bounds: (Vec3, Vec3),
    yaw: f32,
    pitch: f32,
    size: [u32; 2],
) -> Option<egui::ColorImage> {
    let placed: Vec<(&ModelPart, Mat4)> = parts.iter().map(|part| (part, Mat4::IDENTITY)).collect();
    render_placed_parts(device, &placed, bounds, yaw, pitch, size)
}

/// [`render_model`] with a model matrix per part (a kit's cockpit at its socket, for one).
#[must_use]
pub fn render_placed_parts(
    device: &RenderDevice,
    parts: &[(&ModelPart, Mat4)],
    bounds: (Vec3, Vec3),
    yaw: f32,
    pitch: f32,
    size: [u32; 2],
) -> Option<egui::ColorImage> {
    render_placed_parts_scene(
        device,
        parts,
        bounds,
        yaw,
        pitch,
        size,
        &crate::preview_scene::PreviewContext::default(),
    )
}

/// [`render_placed_parts`] in a shared preview scene (`crate::preview_scene`, ADR-7771): the
/// lighting preset and environment intensity, an optional floor with a grid at the bounds' lowest
/// point, and the turntable turning every part about the bounds' vertical axis.
#[must_use]
pub fn render_placed_parts_scene(
    device: &RenderDevice,
    parts: &[(&ModelPart, Mat4)],
    bounds: (Vec3, Vec3),
    yaw: f32,
    pitch: f32,
    size: [u32; 2],
    scene: &crate::preview_scene::PreviewContext,
) -> Option<egui::ColorImage> {
    use crate::preview_scene::{Backdrop, PreviewFloor, SunBase};
    use std::sync::{Mutex, OnceLock};
    static RENDERER: OnceLock<Mutex<regia_render::DeviceFrameRenderer>> = OnceLock::new();
    let camera = OrbitCamera::framing(bounds, yaw, pitch);
    let (eye, center) = (camera.eye, camera.target);
    let distance = (eye - center).length();
    let radius = ((bounds.1 - bounds.0).length() * 0.5).max(0.05);
    let sky_mat = crate::render::sky_material();
    let sky = crate::render::sky_sphere();
    let rig = scene.rig(
        SunBase {
            direction: [-0.45, -0.75, -0.5],
            color: [1.0, 0.97, 0.92],
        },
        radius,
    );
    let flat = crate::preview_scene::flat_backdrop_material(match rig.backdrop {
        Backdrop::Flat(color) => color,
        Backdrop::Sky => [0.0; 3],
    });
    let floor_materials = PreviewFloor::default();
    let mut meshes = Vec::new();
    crate::preview_scene::push_backdrop(
        &mut meshes,
        rig.backdrop,
        (&sky_mat, sky),
        &flat,
        eye,
        distance * 20.0,
    );
    if scene.settings.floor {
        let (y, half) = crate::preview_scene::floor_placement(bounds.0.y, radius);
        // The floor is centered under the bounds, not the origin.
        let before = meshes.len();
        crate::preview_scene::push_floor(&mut meshes, &floor_materials, y, half);
        let slide = Mat4::from_translation(Vec3::new(
            (bounds.0.x + bounds.1.x) * 0.5,
            0.0,
            (bounds.0.z + bounds.1.z) * 0.5,
        ));
        for mesh in &mut meshes[before..] {
            mesh.model = (slide * Mat4::from_cols_array_2d(&mesh.model)).to_cols_array_2d();
        }
    }
    let turn = scene.spin.model(&scene.settings, center);
    for (index, (part, model)) in parts.iter().enumerate() {
        meshes.push(FrameMesh {
            is_static: false,
            mesh_key: Arc::as_ptr(&part.mesh) as u64,
            material_key: Arc::as_ptr(&part.material) as u64,
            object_key: 0x00ed_17b0 + index as u64,
            vertices: &part.mesh.vertices,
            indices: &part.mesh.indices,
            material: &part.material,
            model: (turn * *model).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
    }
    let mut frame = FrameScene {
        width: size[0].max(16),
        height: size[1].max(16),
        eye: eye.to_array(),
        target: center.to_array(),
        fov_y: camera.fov_y,
        near: camera.near,
        far: camera.far,
        depth_prepass: false,
        directional_shadows: false,
        ibl: rig.ibl,
        exposure_ev: crate::render::VIEWPORT_EXPOSURE_EV,
        ambient_lux: rig.ambient_lux,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: rig.directional,
        local_lights: &rig.local_lights,
        quality: FrameQuality::EDITOR,
        meshes,
    };
    let renderer = RENDERER.get_or_init(|| Mutex::new(regia_render::DeviceFrameRenderer::new()));
    let mut renderer = renderer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let frame_renderer = renderer.get(device);
    frame.ibl = rig.ibl && crate::preview_scene::apply_environment(frame_renderer, device, &rig.environment);
    let result = frame_renderer.render_offscreen(device, &frame).ok()?;
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [result.width as usize, result.height as usize],
        &result.pixels,
    ))
}

/// Renders a texture mip as the albedo of a lit sphere or plane in the shared preview scene
/// (`surface` `Flat` draws the plane): the texture viewer's lit preview. `None` when the render fails.
#[must_use]
pub fn render_texture_preview(
    device: &RenderDevice,
    mip: &LabTexture,
    surface: crate::preview_scene::TextureSurface,
    scene: &crate::preview_scene::PreviewContext,
    side: u32,
) -> Option<egui::ColorImage> {
    use crate::material_editor::{PreviewShape, PreviewView, preview_geometry, render_preview_scene};
    let shape = match surface {
        crate::preview_scene::TextureSurface::Sphere => PreviewShape::Sphere,
        crate::preview_scene::TextureSurface::Plane | crate::preview_scene::TextureSurface::Flat => {
            PreviewShape::Plane
        }
    };
    let geometry = preview_geometry(&shape, None).ok()?;
    let material = regia_render::LabMaterial {
        base_color: [1.0; 4],
        metallic: 0.0,
        roughness: 0.6,
        albedo: Some(mip.clone()),
        ..regia_render::LabMaterial::default()
    };
    let side = side.max(64) as f32;
    // Key 0: uploaded for this render only (the viewer renders on change, not per frame).
    render_preview_scene(
        device,
        &material,
        &geometry,
        &PreviewView::default(),
        [side, side],
        0,
        scene,
    )
}

/// A material's summary line for the viewer and the protocol.
#[must_use]
pub fn material_summary(part: &ModelPart) -> serde_json::Value {
    let m = &part.material;
    serde_json::json!({
        "name": part.material_name,
        "base_color": m.base_color,
        "metallic": m.metallic,
        "roughness": m.roughness,
        "emissive": m.emissive,
        "alpha_mode": m.alpha_mode,
        "albedo_map": m.albedo.as_ref().map(|t| [t.width, t.height]),
        "normal_map": m.normal_map.as_ref().map(|t| [t.width, t.height]),
        "orm_map": m.occlusion_map.as_ref().map(|t| [t.width, t.height]),
        "emissive_map": m.emissive_map.as_ref().map(|t| [t.width, t.height]),
        "thin_film": m.thin_film.is_on().then_some(m.thin_film.factor),
        "subsurface": m.subsurface.is_on().then_some(m.subsurface.profile.name()),
        "graph": m.surface_graph.is_some(),
        "triangles": part.mesh.indices.len() / 3,
        "vertices": part.mesh.vertices.len(),
    })
}

// ---------------------------------------------------------------------------------------------
// Texture
// ---------------------------------------------------------------------------------------------

/// Channel isolation for the texture viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Channel {
    /// Colour as authored (alpha over a checkerboard).
    #[default]
    Rgba,
    /// Red as grey.
    R,
    /// Green as grey.
    G,
    /// Blue as grey.
    B,
    /// Alpha as grey.
    A,
}

impl Channel {
    /// Every channel, in menu order.
    pub const ALL: [Self; 5] = [Self::Rgba, Self::R, Self::G, Self::B, Self::A];

    /// Stable name (`rgba`, `r`, `g`, `b`, `a`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Rgba => "rgba",
            Self::R => "r",
            Self::G => "g",
            Self::B => "b",
            Self::A => "a",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// Source range `[start, end)` along one axis feeding output texel `i` of `out` texels from
/// `src` source texels: two texels, or three for the last output texel of an odd axis.
fn mip_footprint(i: u32, out: u32, src: u32) -> (u32, u32) {
    let start = (i * 2).min(src - 1);
    let end = if i + 1 == out { src } else { (i * 2 + 2).min(src) };
    (start, end.max(start + 1))
}

/// The full mip chain of an image (2×2 box filter per level, down to 1×1).
#[must_use]
pub fn mip_chain(image: &LabTexture) -> Vec<LabTexture> {
    let mut chain = vec![image.clone()];
    while let Some(last) = chain.last() {
        if last.width <= 1 && last.height <= 1 {
            break;
        }
        let (w, h) = ((last.width / 2).max(1), (last.height / 2).max(1));
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                // The last output texel of an odd-sized axis also covers the leftover source line.
                let (x0, x1) = mip_footprint(x, w, last.width);
                let (y0, y1) = mip_footprint(y, h, last.height);
                let count = (x1 - x0) * (y1 - y0);
                for c in 0..4 {
                    let mut sum = 0u32;
                    for sy in y0..y1 {
                        for sx in x0..x1 {
                            sum += u32::from(last.rgba[((sy * last.width + sx) * 4 + c) as usize]);
                        }
                    }
                    rgba[((y * w + x) * 4 + c) as usize] = ((sum + count / 2) / count) as u8;
                }
            }
        }
        chain.push(LabTexture {
            width: w,
            height: h,
            rgba,
        });
    }
    chain
}

/// One mip as a displayable image with `channel` isolated (alpha composited over a checker).
#[must_use]
pub fn texture_view(mip: &LabTexture, channel: Channel) -> egui::ColorImage {
    let mut pixels = Vec::with_capacity((mip.width * mip.height) as usize);
    for y in 0..mip.height {
        for x in 0..mip.width {
            let i = ((y * mip.width + x) * 4) as usize;
            let p = &mip.rgba[i..i + 4];
            let color = match channel {
                Channel::R => egui::Color32::from_gray(p[0]),
                Channel::G => egui::Color32::from_gray(p[1]),
                Channel::B => egui::Color32::from_gray(p[2]),
                Channel::A => egui::Color32::from_gray(p[3]),
                Channel::Rgba => {
                    let checker = if ((x / 8) + (y / 8)) % 2 == 0 { 200u32 } else { 140 };
                    let a = u32::from(p[3]);
                    let mix = |c: u8| ((u32::from(c) * a + checker * (255 - a)) / 255) as u8;
                    egui::Color32::from_rgb(mix(p[0]), mix(p[1]), mix(p[2]))
                }
            };
            pixels.push(color);
        }
    }
    egui::ColorImage::new([mip.width as usize, mip.height as usize], pixels)
}

// ---------------------------------------------------------------------------------------------
// Sound
// ---------------------------------------------------------------------------------------------

/// A decoded sound: interleaved samples in −1..1.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedSound {
    /// Channels.
    pub channels: u16,
    /// Sample rate, Hz.
    pub sample_rate: u32,
    /// Interleaved samples.
    pub samples: Vec<f32>,
}

impl DecodedSound {
    /// Frames (samples per channel).
    #[must_use]
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }

    /// Duration, seconds.
    #[must_use]
    pub fn seconds(&self) -> f32 {
        self.frames() as f32 / self.sample_rate.max(1) as f32
    }

    /// Mono mixdown (channel average).
    #[must_use]
    pub fn mono(&self) -> Vec<f32> {
        let c = usize::from(self.channels.max(1));
        self.samples
            .chunks_exact(c)
            .map(|frame| frame.iter().sum::<f32>() / c as f32)
            .collect()
    }
}

/// Decodes a RIFF/WAVE file: PCM 8/16/24/32-bit integer and 32-bit float, including
/// `WAVE_FORMAT_EXTENSIBLE` (the format fields per the Microsoft RIFF/WAVE specification).
///
/// # Errors
///
/// Not a WAVE file, an unsupported encoding, or a truncated chunk.
pub fn decode_wav(bytes: &[u8]) -> Result<DecodedSound, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let mut format: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let len = u32_at(bytes, offset + 4) as usize;
        let body_start = offset + 8;
        let body_end = body_start.saturating_add(len).min(bytes.len());
        let body = &bytes[body_start..body_end];
        if id == b"fmt " {
            if body.len() < 16 {
                return Err("truncated fmt chunk".into());
            }
            let mut tag = u16_at(body, 0);
            if tag == 0xFFFE && body.len() >= 26 {
                tag = u16_at(body, 24); // the sub-format GUID's first two bytes
            }
            format = Some((tag, u16_at(body, 2), u32_at(body, 4), u16_at(body, 14)));
        } else if id == b"data" {
            data = Some(body);
        }
        offset = body_start + len + (len & 1);
    }
    let (tag, channels, sample_rate, bits) = format.ok_or("no fmt chunk")?;
    let data = data.ok_or("no data chunk")?;
    if channels == 0 {
        return Err("zero channels".into());
    }
    let samples: Vec<f32> = match (tag, bits) {
        (1, 8) => data.iter().map(|&b| (f32::from(b) - 128.0) / 128.0).collect(),
        (1, 16) => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| f32::from(i16::from_le_bytes(*b)) / 32_768.0)
            .collect(),
        (1, 24) => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0)
            .collect(),
        (1, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes(*b) as f32 / 2_147_483_648.0)
            .collect(),
        (3, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect(),
        (tag, bits) => return Err(format!("unsupported WAVE encoding (format {tag}, {bits} bits)")),
    };
    Ok(DecodedSound {
        channels,
        sample_rate,
        samples,
    })
}

/// Decodes a sound for the preview: a RIFF/WAVE file through the pure WAV reader above (whatever
/// the file is named), every other format `regia_audio` decodes (Ogg Vorbis, FLAC, MP3) through
/// its bounded whole-file decoder — the same call, caps and codecs the runtime plays through.
/// The frames are read back through `regia_audio`'s public channel reader; a fully silent file is
/// a legal preview (a flat waveform), so its zero samples are filled in from the decoded duration.
///
/// # Errors
///
/// The file does not decode (the message names `name`).
pub fn decode_sound(bytes: &[u8], name: &str) -> Result<DecodedSound, String> {
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return decode_wav(bytes);
    }
    let asset = regia_audio::SoundAsset::from_bytes_static_with_limits(
        bytes.to_vec(),
        name,
        &regia_audio::SoundDecodeLimits::default(),
    )
    .map_err(|e| format!("{name}: {e}"))?;
    let rate = asset.sample_rate();
    let channels_of = |ir: &regia_audio::ImpulseResponse| ir.channel_count();
    match regia_audio::ImpulseResponse::from_sound(&asset) {
        Ok(ir) => {
            let channels = channels_of(&ir);
            let mut samples = Vec::with_capacity(ir.frames() * channels);
            for frame in 0..ir.frames() {
                for channel in 0..channels {
                    samples.push(
                        ir.channel(channel)
                            .and_then(|c| c.get(frame).copied())
                            .unwrap_or(0.0),
                    );
                }
            }
            Ok(DecodedSound {
                channels: u16::try_from(channels).unwrap_or(2),
                sample_rate: rate,
                samples,
            })
        }
        // A silent or empty sound is still a legal preview: a flat line of the decoded length.
        Err(e) if matches!(e.code(), "ir_silent" | "ir_empty") => {
            let frames = (asset.duration_secs() * f64::from(rate)).round() as usize;
            Ok(DecodedSound {
                channels: 1,
                sample_rate: rate,
                samples: vec![0.0; frames],
            })
        }
        Err(e) => Err(format!("{name}: {e}")),
    }
}

/// Draws the waveform (per-column min/max of the mono mixdown) into an image.
#[must_use]
pub fn waveform_image(sound: &DecodedSound, width: usize, height: usize) -> egui::ColorImage {
    let (width, height) = (width.max(8), height.max(8));
    let mut image = egui::ColorImage::filled([width, height], egui::Color32::from_rgb(22, 24, 30));
    let mid = height / 2;
    for x in 0..width {
        image.pixels[mid * width + x] = egui::Color32::from_rgb(60, 64, 76);
    }
    let mono = sound.mono();
    if mono.is_empty() {
        return image;
    }
    for x in 0..width {
        let a = x * mono.len() / width;
        let b = ((x + 1) * mono.len() / width).max(a + 1).min(mono.len());
        let (lo, hi) = mono[a..b]
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        let to_y = |v: f32| ((1.0 - v.clamp(-1.0, 1.0)) * 0.5 * (height - 1) as f32).round() as usize;
        for y in to_y(hi)..=to_y(lo) {
            image.pixels[y * width + x] = egui::Color32::from_rgb(110, 250, 200);
        }
    }
    image
}

thread_local! {
    /// The editor's audio output for previews (opened on first play; offline when no device).
    static PREVIEW_AUDIO: std::cell::RefCell<Option<regia_audio::Audio>> = const { std::cell::RefCell::new(None) };
}

/// Plays a decoded sound through `regia_audio` on the default device (the offline backend
/// when there is none — `AudioSettings::headless` false asks for the device).
///
/// # Errors
///
/// The clip was rejected (empty, bad rate) or the mixer refused it.
pub fn play_sound(sound: &DecodedSound) -> Result<bool, String> {
    let clip = regia_audio::SoundClip::new(sound.mono(), sound.sample_rate).map_err(|e| e.to_string())?;
    with_preview_audio(|audio| {
        audio.play(&clip).map_err(|e| e.to_string())?;
        audio.drain();
        Ok(true)
    })
}

/// Runs `f` on the editor's preview output (the one [`play_sound`] plays on), opening it on first
/// use: the sequencer's edit-world preview plays its sound sections there when the edit world has
/// no `Audio` of its own (ADR-8940). `f` must not call [`play_sound`].
pub fn with_preview_audio<R>(f: impl FnOnce(&mut regia_audio::Audio) -> R) -> R {
    PREVIEW_AUDIO.with(|cell| {
        let mut slot = cell.borrow_mut();
        let audio =
            slot.get_or_insert_with(|| regia_audio::Audio::new(&regia_audio::AudioSettings::default()));
        f(audio)
    })
}

// ---------------------------------------------------------------------------------------------
// The viewer state + panel
// ---------------------------------------------------------------------------------------------

/// What the viewer has open.
#[derive(Default)]
pub enum Loaded {
    /// Nothing.
    #[default]
    None,
    /// A mesh, its LOD chain and bounds.
    Mesh {
        /// The model.
        model: Arc<LoadedModel>,
        /// LODs (0 = the model).
        lods: Vec<ModelLod>,
    },
    /// A texture's mip chain.
    Texture(Vec<LabTexture>),
    /// A sound.
    Sound(DecodedSound),
    /// An animation: skeleton and clips (ADR-7788).
    Animation(Arc<AnimPreview>),
}

/// Gathered overlay geometry, by (overlay, LOD).
type OverlayCache = Vec<(MeshOverlay, usize, Result<OverlayGeometry, OverlayRefusal>)>;

/// Clears `cache` when the import-settings write count moved since `seen` (ADR-7495).
fn drop_stale_overlays(cache: &mut OverlayCache, seen: &mut u64) {
    let writes = crate::asset_ops_agent::IMPORT_SETTINGS_WRITES.load(std::sync::atomic::Ordering::Relaxed);
    if writes != *seen {
        cache.clear();
        *seen = writes;
    }
}

/// What a texture's lit preview was rendered for: the mip, the scene and the image size.
pub type TextureViewKey = (usize, crate::preview_scene::PreviewContextKey, u32);

/// The asset viewer's state.
#[derive(Default)]
pub struct AssetViewerState {
    /// Project-relative path of the open asset.
    pub path: Option<String>,
    /// The open asset.
    pub loaded: Loaded,
    /// The last load error.
    pub error: Option<String>,
    /// Mesh LOD shown.
    pub lod: usize,
    /// Mesh orbit yaw/pitch, radians.
    pub orbit: (f32, f32),
    /// Texture mip shown.
    pub mip: usize,
    /// Texture channel shown.
    pub channel: Channel,
    /// A request from the content browser (double-click) to open.
    pub open_request: Option<String>,
    /// Mesh overlays drawn over the render (ADR-7469; none by default).
    pub overlays: Vec<MeshOverlay>,
    /// Gathered overlay geometry for the open mesh: (overlay, LOD, result). Cleared on open and
    /// after any import-settings write recooked (`overlay_writes`).
    pub overlay_cache: Vec<(MeshOverlay, usize, Result<OverlayGeometry, OverlayRefusal>)>,
    /// The import-settings write count `overlay_cache` was gathered at (ADR-7495).
    pub overlay_writes: u64,
    /// The mesh viewer's preview scene: lighting preset, environment intensity, floor and
    /// turntable (`crate::preview_scene`, ADR-7771).
    pub scene: crate::preview_scene::PreviewContext,
    /// The texture viewer's preview scene (the textured preview shape, ADR-7771).
    pub texture_scene: crate::preview_scene::PreviewContext,
    /// The texture viewer's lit preview image and what it was rendered for.
    pub texture_view: Option<(TextureViewKey, egui::TextureHandle)>,
    /// What the Add simple collision button asks for.
    pub collision_shape: CollisionShape,
    /// A helper button the shell has not run yet (it needs the world: the collision write and
    /// the nav preview job, or the open mesh for the lightmap UV).
    pub helper_request: Option<MeshHelperRequest>,
    /// What the last helper did (or why it did not).
    pub helper_toast: Option<String>,
    /// The last Generate lightmap UV result for the open mesh, and its drawn atlas.
    pub lightmap_atlas: Option<(LightmapUvPreview, Option<egui::TextureHandle>)>,
    /// Animation playback (clip, time, paused; ADR-7788).
    pub anim: AnimViewerState,
}

impl AssetViewerState {
    /// Drops the gathered overlay geometry when an import-settings write recooked since it was
    /// gathered (an Apply in the Import settings panel or `editor.set_import_settings`), so the
    /// collision overlay reads the new hulls and a cached `collision_not_cooked` is retried.
    pub fn drop_stale_overlays(&mut self) {
        drop_stale_overlays(&mut self.overlay_cache, &mut self.overlay_writes);
    }

    /// Opens `path` (project-relative or `builtin:`), replacing what was open.
    pub fn open(&mut self, root: Option<&Path>, library: &MeshLibrary, path: &str) {
        self.path = Some(path.to_string());
        self.error = None;
        self.lod = 0;
        self.mip = 0;
        self.orbit = (0.6, 0.35);
        self.overlay_cache.clear();
        self.lightmap_atlas = None;
        self.helper_toast = None;
        self.texture_view = None;
        self.anim = AnimViewerState::default();
        match load_asset(root, library, path) {
            Ok(loaded) => self.loaded = loaded,
            Err(err) => {
                self.loaded = Loaded::None;
                self.error = Some(err);
            }
        }
    }
}

/// Loads an asset for viewing.
///
/// # Errors
///
/// Unknown kind, or the loader failed.
pub fn load_asset(root: Option<&Path>, library: &MeshLibrary, path: &str) -> Result<Loaded, String> {
    match kind_of(path) {
        Some(AssetKind::Mesh) => {
            let model = library.model(root, path)?;
            let lods = build_lods(&model);
            Ok(Loaded::Mesh { model, lods })
        }
        Some(AssetKind::Texture) => {
            let image = crate::mesh_source::load_texture_file(&resolve_asset_path(root, path))?;
            Ok(Loaded::Texture(mip_chain(&image)))
        }
        Some(AssetKind::Sound) => {
            let full: PathBuf = resolve_asset_path(root, path);
            let bytes = std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
            Ok(Loaded::Sound(decode_sound(&bytes, path)?))
        }
        Some(AssetKind::Animation) => Ok(Loaded::Animation(Arc::new(load_anim_preview(root, path)?))),
        None => Err(format!(
            "{path}: no viewer for this file type (meshes, textures, sounds: {})",
            sound_extensions().join(" ")
        )),
    }
}

/// The asset viewer panel.
pub fn asset_viewer_panel(
    ui: &mut egui::Ui,
    state: &mut AssetViewerState,
    root: Option<&Path>,
    library: &MeshLibrary,
    device: Option<&RenderDevice>,
) {
    if let Some(path) = state.open_request.take() {
        state.open(root, library, &path);
    }
    let Some(path) = state.path.clone() else {
        // Wraps in a narrow dock (bug 19, ADR-8586).
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "Double-click a mesh, texture or sound in the Content Browser to view it.",
                )
                .weak(),
            )
            .wrap(),
        );
        return;
    };
    ui.heading(&path);
    if let Some(err) = &state.error {
        ui.colored_label(egui::Color32::from_rgb(255, 120, 100), err);
        return;
    }
    let available = ui.available_size();
    let side = available.x.min(available.y - 80.0).max(64.0);
    match &state.loaded {
        Loaded::None => {}
        Loaded::Mesh { model, lods } => {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} parts · {} triangles · {} vertices · bounds {:.2}×{:.2}×{:.2} m",
                    model.parts.len(),
                    model.triangles(),
                    model.vertices(),
                    model.aabb_max.x - model.aabb_min.x,
                    model.aabb_max.y - model.aabb_min.y,
                    model.aabb_max.z - model.aabb_min.z,
                ));
            });
            ui.horizontal(|ui| {
                for (index, lod) in lods.iter().enumerate() {
                    ui.selectable_value(
                        &mut state.lod,
                        index,
                        format!("LOD{index} ({} tris, {:.0}%)", lod.triangles, lod.ratio * 100.0),
                    );
                }
            });
            for part in &model.parts {
                let m = &part.material;
                ui.weak(format!(
                    "material {} · base {:.2},{:.2},{:.2} · metal {:.2} rough {:.2}{}{}{}",
                    part.material_name,
                    m.base_color[0],
                    m.base_color[1],
                    m.base_color[2],
                    m.metallic,
                    m.roughness,
                    if m.albedo.is_some() { " · albedo" } else { "" },
                    if m.normal_map.is_some() { " · normal" } else { "" },
                    if m.surface_graph.is_some() {
                        " · graph"
                    } else {
                        ""
                    },
                ));
            }
            ui.horizontal(|ui| {
                ui.weak("Overlays:");
                for (overlay, action) in [
                    (MeshOverlay::Uv0, &crate::agent_parity::VIEWER_OVERLAY_UV),
                    (MeshOverlay::Uv1, &crate::agent_parity::VIEWER_OVERLAY_UV1),
                    (MeshOverlay::Normals, &crate::agent_parity::VIEWER_OVERLAY_NORMALS),
                    (
                        MeshOverlay::Collision,
                        &crate::agent_parity::VIEWER_OVERLAY_COLLISION,
                    ),
                ] {
                    let mut on = state.overlays.contains(&overlay);
                    if crate::agent_parity::action_toggle(ui, action, &mut on).changed() {
                        state.overlays.retain(|o| *o != overlay);
                        if on {
                            state.overlays.push(overlay);
                        }
                    }
                }
            });
            // The LOD the render below shows (`lods.first()` when `state.lod` is out of range).
            let lod_index = if state.lod < lods.len() { state.lod } else { 0 };
            drop_stale_overlays(&mut state.overlay_cache, &mut state.overlay_writes);
            for overlay in state.overlays.clone() {
                if !state
                    .overlay_cache
                    .iter()
                    .any(|(o, l, _)| *o == overlay && *l == lod_index)
                    && let Some(lod) = lods.get(lod_index)
                {
                    let geometry = overlay_geometry(root, &path, model, lod, overlay);
                    state.overlay_cache.push((overlay, lod_index, geometry));
                }
            }
            for (overlay, _, result) in state
                .overlay_cache
                .iter()
                .filter(|(o, l, _)| *l == lod_index && state.overlays.contains(o))
            {
                if let Err(refusal) = result {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("{}: {}", overlay.name(), refusal.message),
                    )
                    .on_hover_text(format!("Agent error code: {}", refusal.code));
                }
            }
            ui.horizontal_wrapped(|ui| {
                ui.weak("Helpers:");
                let combo = egui::ComboBox::from_id_salt("asset-viewer-collision-shape")
                    .selected_text(state.collision_shape.label())
                    .show_ui(ui, |ui| {
                        for shape in CollisionShape::ALL {
                            ui.selectable_value(&mut state.collision_shape, shape, shape.label());
                        }
                    });
                crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Collision shape");
                if crate::agent_parity::action_button(ui, &crate::agent_parity::VIEWER_ADD_COLLISION)
                    .clicked()
                {
                    state.helper_request = Some(MeshHelperRequest::AddCollision(state.collision_shape));
                }
                if crate::agent_parity::action_button(ui, &crate::agent_parity::VIEWER_LIGHTMAP_UV).clicked()
                {
                    state.helper_request = Some(MeshHelperRequest::LightmapUv);
                }
                if crate::agent_parity::action_button(ui, &crate::agent_parity::VIEWER_NAV_PREVIEW).clicked()
                {
                    state.helper_request = Some(MeshHelperRequest::NavPreview);
                }
            });
            if let Some(toast) = &state.helper_toast {
                ui.weak(toast);
            }
            crate::preview_scene::preview_scene_controls(ui, &mut state.scene.settings);
            state.scene.tick(ui.ctx());
            let lod = lods.get(state.lod).or_else(|| lods.first());
            if let (Some(device), Some(lod)) = (device, lod) {
                let placed: Vec<(&ModelPart, Mat4)> =
                    lod.parts.iter().map(|part| (part, Mat4::IDENTITY)).collect();
                let image = render_placed_parts_scene(
                    device,
                    &placed,
                    (model.aabb_min, model.aabb_max),
                    state.orbit.0,
                    state.orbit.1,
                    [side as u32, side as u32],
                    &state.scene,
                );
                if let Some(mut image) = image {
                    let camera =
                        OrbitCamera::framing((model.aabb_min, model.aabb_max), state.orbit.0, state.orbit.1);
                    for overlay in &state.overlays {
                        if let Some((_, _, Ok(geometry))) = state
                            .overlay_cache
                            .iter()
                            .find(|(o, l, _)| o == overlay && *l == lod_index)
                        {
                            paint_overlay(&mut image, &camera, geometry);
                        }
                    }
                    let texture =
                        ui.ctx()
                            .load_texture("asset-viewer-mesh", image, egui::TextureOptions::LINEAR);
                    let response = ui.add(egui::Image::new(&texture).sense(egui::Sense::drag()));
                    let delta = response.drag_delta();
                    state.orbit.0 -= delta.x * 0.01;
                    state.orbit.1 = (state.orbit.1 + delta.y * 0.01).clamp(-1.4, 1.4);
                }
            }
            if let Some((preview, texture)) = &state.lightmap_atlas {
                ui.label(preview.summary_line());
                if let Some(texture) = texture {
                    ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(256.0, 256.0)));
                }
            }
        }
        Loaded::Texture(chain) => {
            let base = &chain[0];
            ui.label(format!("{}×{} · {} mips", base.width, base.height, chain.len()));
            ui.horizontal(|ui| {
                let mip_menu = egui::ComboBox::from_id_salt("asset-viewer-mip")
                    .selected_text(format!("mip {}", state.mip))
                    .show_ui(ui, |ui| {
                        for (i, mip) in chain.iter().enumerate() {
                            ui.selectable_value(
                                &mut state.mip,
                                i,
                                format!("mip {i} ({}×{})", mip.width, mip.height),
                            );
                        }
                    });
                crate::a11y_audit::name(mip_menu.response, egui::WidgetType::ComboBox, "Texture mip");
                for channel in Channel::ALL {
                    ui.selectable_value(&mut state.channel, channel, channel.name());
                }
            });
            ui.horizontal_wrapped(|ui| {
                let combo = egui::ComboBox::from_id_salt("asset-viewer-texture-surface")
                    .selected_text(state.texture_scene.settings.surface.label())
                    .show_ui(ui, |ui| {
                        for surface in crate::preview_scene::TextureSurface::ALL {
                            ui.selectable_value(
                                &mut state.texture_scene.settings.surface,
                                surface,
                                surface.label(),
                            );
                        }
                    });
                crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Preview surface");
            });
            let mip_index = state.mip.min(chain.len() - 1);
            let mip = &chain[mip_index];
            if state.texture_scene.settings.surface == crate::preview_scene::TextureSurface::Flat {
                let texture = ui.ctx().load_texture(
                    "asset-viewer-texture",
                    texture_view(mip, state.channel),
                    egui::TextureOptions::NEAREST,
                );
                ui.add(egui::Image::new(&texture).fit_to_exact_size(egui::vec2(side, side)));
            } else {
                crate::preview_scene::preview_scene_controls(ui, &mut state.texture_scene.settings);
                state.texture_scene.tick(ui.ctx());
                match device {
                    Some(device) => {
                        // Renders once per change of the mip, the scene or the size (ADR-9970).
                        let key = (mip_index, state.texture_scene.key(), side as u32);
                        if state.texture_view.as_ref().is_none_or(|(k, _)| *k != key)
                            && let Some(image) = render_texture_preview(
                                device,
                                mip,
                                state.texture_scene.settings.surface,
                                &state.texture_scene,
                                side as u32,
                            )
                        {
                            let handle = ui.ctx().load_texture(
                                "asset-viewer-texture-3d",
                                image,
                                egui::TextureOptions::LINEAR,
                            );
                            state.texture_view = Some((key, handle));
                        }
                        if let Some((_, handle)) = &state.texture_view {
                            ui.add(egui::Image::new(handle).fit_to_exact_size(egui::vec2(side, side)));
                        }
                    }
                    None => {
                        ui.weak("(the lit preview needs a GPU device)");
                    }
                }
            }
        }
        Loaded::Sound(sound) => {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} Hz · {} ch · {:.2} s",
                    sound.sample_rate,
                    sound.channels,
                    sound.seconds()
                ));
                if ui.button("▶ Play").clicked()
                    && let Err(err) = play_sound(sound)
                {
                    state.error = Some(err);
                }
            });
            let image = waveform_image(sound, available.x.max(64.0) as usize, 160);
            let texture = ui
                .ctx()
                .load_texture("asset-viewer-sound", image, egui::TextureOptions::LINEAR);
            ui.add(egui::Image::new(&texture));
        }
        Loaded::Animation(preview) => {
            let preview = Arc::clone(preview);
            anim_viewer_ui(ui, &preview, &mut state.anim, side);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Mesh overlays (ADR-7469): UV layout, vertex normals, the cooked collider
// ---------------------------------------------------------------------------------------------

/// A mesh viewer overlay, drawn over the orbit render (`editor.view_asset { overlay }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeshOverlay {
    /// The UV0 layout (`TEXCOORD_0`) in a centered square.
    Uv0,
    /// The UV1 layout (`TEXCOORD_1`, the lightmap set) in a centered square.
    Uv1,
    /// Vertex normals as short lines from each vertex.
    Normals,
    /// The cooked collider: hull silhouettes, or the `collision_*` node triangles.
    Collision,
}

impl MeshOverlay {
    /// Every overlay, in toolbar order.
    pub const ALL: [Self; 4] = [Self::Uv0, Self::Uv1, Self::Normals, Self::Collision];

    /// Stable name (`uv`, `uv1`, `normals`, `collision`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Uv0 => "uv",
            Self::Uv1 => "uv1",
            Self::Normals => "normals",
            Self::Collision => "collision",
        }
    }

    /// The overlay called `name` (`uv0` is accepted for `uv`).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "uv" | "uv0" => Some(Self::Uv0),
            "uv1" => Some(Self::Uv1),
            "normals" => Some(Self::Normals),
            "collision" => Some(Self::Collision),
            _ => None,
        }
    }
}

/// Why an overlay has nothing to draw; `code` is the agent error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayRefusal {
    /// `uv_set_missing`, `no_collision` or `collision_not_cooked`.
    pub code: &'static str,
    /// What is missing and what to do.
    pub message: String,
}

/// What an overlay draws, gathered from the model and, for collision, the cook's cache.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayGeometry {
    /// Triangles in UV space (glTF convention: `(0, 0)` is the image's top left).
    Uv {
        /// The texture coordinate set (0 or 1).
        set: u32,
        /// The triangles.
        triangles: Vec<[[f32; 2]; 3]>,
    },
    /// Normal segments in model space: start, end, and the line's color (`n * 0.5 + 0.5`).
    Normals {
        /// The segments.
        segments: Vec<(Vec3, Vec3, [u8; 3])>,
    },
    /// The collider in model space.
    Collision {
        /// `cooked_hulls` (the `collision: ConvexDecomposition` hull sets in the dev cache) or
        /// `collision_nodes` (the file's `collision_*` meshes).
        source: &'static str,
        /// Convex hulls as vertex lists.
        hulls: Vec<Vec<Vec3>>,
        /// Collision-node triangles.
        triangles: Vec<[Vec3; 3]>,
    },
}

impl OverlayGeometry {
    /// A short JSON summary for the protocol (`info.overlays[]`).
    #[must_use]
    pub fn summary(&self) -> serde_json::Value {
        match self {
            Self::Uv { set, triangles } => serde_json::json!({ "set": set, "triangles": triangles.len() }),
            Self::Normals { segments } => serde_json::json!({ "normals": segments.len() }),
            Self::Collision {
                source,
                hulls,
                triangles,
            } => serde_json::json!({ "source": source, "hulls": hulls.len(), "triangles": triangles.len() }),
        }
    }
}

/// Most normal lines drawn: larger meshes draw every n-th vertex.
pub const MAX_OVERLAY_NORMALS: usize = 20_000;

/// The normal line length as a fraction of the bounds' diagonal.
pub const NORMAL_LENGTH: f32 = 0.06;

/// Gathers what `overlay` draws for `lod` of `model` (`path` names the source: a project
/// path or `builtin:`).
///
/// # Errors
///
/// [`OverlayRefusal`]: `uv_set_missing` (no `TEXCOORD_<set>` in the file; a `builtin:` mesh has
/// UV0 only), `no_collision` (no cooked hulls and no `collision_*` nodes), or
/// `collision_not_cooked` (`collision: ConvexDecomposition` is set but the dev cache holds no
/// hulls: cook first).
pub fn overlay_geometry(
    root: Option<&Path>,
    path: &str,
    model: &LoadedModel,
    lod: &ModelLod,
    overlay: MeshOverlay,
) -> Result<OverlayGeometry, OverlayRefusal> {
    let _span =
        tracing::debug_span!(target: "regia_editor", "mesh_overlay_geometry", overlay = overlay.name())
            .entered();
    let builtin = path.starts_with("builtin:");
    match overlay {
        MeshOverlay::Uv0 | MeshOverlay::Uv1 => {
            let set = u32::from(overlay == MeshOverlay::Uv1);
            let triangles = if builtin {
                if set == 0 {
                    part_uv_triangles(&lod.parts)
                } else {
                    Vec::new()
                }
            } else {
                gltf_uv_triangles(&resolve_asset_path(root, path), set).unwrap_or_default()
            };
            if triangles.is_empty() {
                return Err(OverlayRefusal {
                    code: "uv_set_missing",
                    message: format!("{path} has no TEXCOORD_{set} (UV{set}) on any drawn primitive"),
                });
            }
            Ok(OverlayGeometry::Uv { set, triangles })
        }
        MeshOverlay::Normals => {
            let length = (model.aabb_max - model.aabb_min).length().max(1e-3) * NORMAL_LENGTH;
            let total: usize = lod.parts.iter().map(|p| p.mesh.vertices.len()).sum();
            let stride = total.div_ceil(MAX_OVERLAY_NORMALS).max(1);
            let segments = lod
                .parts
                .iter()
                .flat_map(|p| p.mesh.vertices.iter())
                .step_by(stride)
                .filter_map(|v| {
                    let n = Vec3::from_array(v.normal).normalize_or_zero();
                    (n != Vec3::ZERO).then(|| {
                        let p = Vec3::from_array(v.position);
                        let color = n
                            .to_array()
                            .map(|c| ((c * 0.5 + 0.5).clamp(0.0, 1.0) * 255.0).round() as u8);
                        (p, p + n * length, color)
                    })
                })
                .collect();
            Ok(OverlayGeometry::Normals { segments })
        }
        MeshOverlay::Collision => {
            let no_collision = || OverlayRefusal {
                code: "no_collision",
                message: format!(
                    "{path} has no cooked collider: set `collision: ConvexDecomposition` in its import \
                     settings (or add collision_* nodes) and cook"
                ),
            };
            if builtin {
                return Err(no_collision());
            }
            let full = resolve_asset_path(root, path);
            let (hulls, decomposition) = match root {
                Some(root) => cooked_hulls(root, &full),
                None => (Vec::new(), false),
            };
            if !hulls.is_empty() {
                return Ok(OverlayGeometry::Collision {
                    source: "cooked_hulls",
                    hulls,
                    triangles: Vec::new(),
                });
            }
            let options = crate::mesh_source::GltfLoadOptions {
                keep_collision_nodes: true,
            };
            let material = Arc::new(regia_render::LabMaterial::default());
            let triangles: Vec<[Vec3; 3]> =
                crate::mesh_source::load_gltf_model_with_options(&full, &material, &|_| true, options)
                    .map(|m| {
                        m.collision_parts
                            .iter()
                            .flat_map(|part| {
                                let v = &part.mesh.vertices;
                                part.mesh.indices.as_chunks::<3>().0.iter().filter_map(move |t| {
                                    let p = |i: u32| v.get(i as usize).map(|x| Vec3::from_array(x.position));
                                    Some([p(t[0])?, p(t[1])?, p(t[2])?])
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            if !triangles.is_empty() {
                return Ok(OverlayGeometry::Collision {
                    source: "collision_nodes",
                    hulls: Vec::new(),
                    triangles,
                });
            }
            if decomposition {
                return Err(OverlayRefusal {
                    code: "collision_not_cooked",
                    message: format!(
                        "{path} asks for convex collision but the dev cache holds no hulls: cook it \
                         (Reimport, or editor.reimport)"
                    ),
                });
            }
            Err(no_collision())
        }
    }
}

/// UV0 triangles of the loaded parts (a `builtin:` mesh).
fn part_uv_triangles(parts: &[ModelPart]) -> Vec<[[f32; 2]; 3]> {
    parts
        .iter()
        .flat_map(|part| {
            let v = &part.mesh.vertices;
            part.mesh.indices.as_chunks::<3>().0.iter().filter_map(move |t| {
                let uv = |i: u32| v.get(i as usize).map(|x| x.uv);
                Some([uv(t[0])?, uv(t[1])?, uv(t[2])?])
            })
        })
        .collect()
}

/// The triangles of every drawn mesh (collision nodes left out, each mesh once) in UV set
/// `set`; empty when no triangle primitive has that set.
///
/// # Errors
///
/// The file does not open as glTF.
pub fn gltf_uv_triangles(path: &Path, set: u32) -> Result<Vec<[[f32; 2]; 3]>, String> {
    let (document, buffers) = regia_asset_pipeline::open_gltf_checked(path)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for node in document.nodes() {
        if node.name().is_some_and(regia_asset::is_collision_node_name) {
            continue;
        }
        let Some(mesh) = node.mesh() else { continue };
        if !seen.insert(mesh.index()) {
            continue;
        }
        for primitive in mesh.primitives() {
            if primitive.mode() != gltf::mesh::Mode::Triangles {
                continue;
            }
            let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data.0[..]));
            let Some(uvs) = reader.read_tex_coords(set) else {
                continue;
            };
            let uvs: Vec<[f32; 2]> = uvs.into_f32().collect();
            let indices: Vec<u32> = reader
                .read_indices()
                .map(|i| i.into_u32().collect())
                .unwrap_or_else(|| (0..uvs.len() as u32).collect());
            for t in indices.as_chunks::<3>().0 {
                let uv = |i: u32| uvs.get(i as usize).copied();
                if let (Some(a), Some(b), Some(c)) = (uv(t[0]), uv(t[1]), uv(t[2])) {
                    out.push([a, b, c]);
                }
            }
        }
    }
    Ok(out)
}

/// The hull sets the cook wrote for `full` (`collision/...` sub-assets of its `.meta`, read from
/// the host platform's dev cache), mapped back into the viewer's space: the cooked meshes bake
/// the importer's root matrix (`forward: Convert` yaw and `scale`), the viewer's model does not.
/// The flag is whether the `.meta` asks for `collision: ConvexDecomposition`.
fn cooked_hulls(root: &Path, full: &Path) -> (Vec<Vec<Vec3>>, bool) {
    let Ok(bytes) = std::fs::read(regia_asset::meta_path_for(full)) else {
        return (Vec::new(), false);
    };
    let Ok(meta) = regia_asset::MetaSidecar::from_ron(&bytes) else {
        return (Vec::new(), false);
    };
    let settings = regia_asset_pipeline::GltfImporterSettings::from_meta_string(meta.settings.as_deref());
    let decomposition = settings.collision == regia_asset_pipeline::CollisionImport::ConvexDecomposition;
    let s = if settings.scale.is_finite() && settings.scale > 0.0 {
        settings.scale
    } else {
        1.0
    };
    let undo = match settings.forward {
        regia_asset_pipeline::GltfForward::Convert => Vec3::new(-1.0 / s, 1.0 / s, -1.0 / s),
        regia_asset_pipeline::GltfForward::Keep => Vec3::splat(1.0 / s),
    };
    let cache = regia_asset::cooked_cache_dir(root, regia_asset_pipeline::host_platform());
    let mut hulls = Vec::new();
    for sub in &meta.sub_assets {
        if !sub.path.starts_with("collision/") || sub.type_path != regia_physics::hull_set::HULL_SET_TYPE_PATH
        {
            continue;
        }
        let Ok(file) = std::fs::read(cache.join(regia_asset::cooked_file_name(sub.id))) else {
            continue;
        };
        let Ok((_, payload)) = regia_asset::decode_cooked_file(&file) else {
            continue;
        };
        match regia_physics::hull_set::HullSet::from_bytes(payload) {
            Ok(set) => hulls.extend(
                set.hulls
                    .into_iter()
                    .map(|hull| hull.into_iter().map(|p| p * undo).collect::<Vec<_>>()),
            ),
            Err(e) => tracing::warn!(target: "regia_editor", path = %sub.path, "cooked hull set: {e}"),
        }
    }
    (hulls, decomposition)
}

/// The UV overlay's square inside a `width` × `height` image: `(x0, y0, side)` in pixels.
#[must_use]
pub fn uv_square(width: usize, height: usize) -> (usize, usize, usize) {
    let margin = 8.min(width.min(height) / 8);
    let side = width.min(height).saturating_sub(margin * 2).max(1);
    (
        (width - side.min(width)) / 2,
        (height - side.min(height)) / 2,
        side,
    )
}

const UV_LINE: egui::Color32 = egui::Color32::from_rgb(255, 196, 64);
const UV_FRAME: egui::Color32 = egui::Color32::from_rgb(140, 140, 140);
const COLLISION_LINE: egui::Color32 = egui::Color32::from_rgb(64, 255, 96);

/// Draws `geometry` over `image` (a render made with `camera`). UV layouts fill a centered
/// square with a darkened backdrop; normals and collision are projected through `camera`, so
/// they only touch pixels around the mesh.
pub fn paint_overlay(image: &mut egui::ColorImage, camera: &OrbitCamera, geometry: &OverlayGeometry) {
    let _span = tracing::debug_span!(target: "regia_editor", "mesh_overlay_paint").entered();
    let [width, height] = image.size;
    let size = [width as f32, height as f32];
    match geometry {
        OverlayGeometry::Uv { triangles, .. } => {
            let (x0, y0, side) = uv_square(width, height);
            let clip = (x0 as i64, y0 as i64, (x0 + side) as i64, (y0 + side) as i64);
            for y in y0..(y0 + side).min(height) {
                for x in x0..(x0 + side).min(width) {
                    let px = &mut image.pixels[y * width + x];
                    *px = egui::Color32::from_rgb(px.r() / 4, px.g() / 4, px.b() / 4);
                }
            }
            let span = (side.max(2) - 1) as f32;
            let to_px = |uv: [f32; 2]| [x0 as f32 + uv[0] * span, y0 as f32 + uv[1] * span];
            let corners = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]].map(to_px);
            for i in 0..4 {
                draw_line(image, corners[i], corners[(i + 1) % 4], UV_FRAME, clip);
            }
            for tri in triangles {
                let p = tri.map(to_px);
                draw_line(image, p[0], p[1], UV_LINE, clip);
                draw_line(image, p[1], p[2], UV_LINE, clip);
                draw_line(image, p[2], p[0], UV_LINE, clip);
            }
        }
        OverlayGeometry::Normals { segments } => {
            let clip = (0, 0, width as i64, height as i64);
            for (a, b, color) in segments {
                if let (Some(a), Some(b)) = (camera.project(*a, size), camera.project(*b, size)) {
                    draw_line(
                        image,
                        a,
                        b,
                        egui::Color32::from_rgb(color[0], color[1], color[2]),
                        clip,
                    );
                }
            }
        }
        OverlayGeometry::Collision { hulls, triangles, .. } => {
            let clip = (0, 0, width as i64, height as i64);
            for hull in hulls {
                let points: Vec<[f32; 2]> = hull.iter().filter_map(|p| camera.project(*p, size)).collect();
                let outline = convex_hull_2d(&points);
                fill_convex(image, &outline, COLLISION_LINE, 0.35);
                for (i, a) in outline.iter().enumerate() {
                    draw_line(image, *a, outline[(i + 1) % outline.len()], COLLISION_LINE, clip);
                }
            }
            for tri in triangles {
                let p: Vec<[f32; 2]> = tri.iter().filter_map(|p| camera.project(*p, size)).collect();
                if let [a, b, c] = p[..] {
                    draw_line(image, a, b, COLLISION_LINE, clip);
                    draw_line(image, b, c, COLLISION_LINE, clip);
                    draw_line(image, c, a, COLLISION_LINE, clip);
                }
            }
        }
    }
}

/// A line from `a` to `b` (pixel centers), one pixel wide, inside `clip` (x0, y0, x1, y1;
/// exclusive ends). Bresenham's integer line (public domain, 1965).
fn draw_line(
    image: &mut egui::ColorImage,
    a: [f32; 2],
    b: [f32; 2],
    color: egui::Color32,
    clip: (i64, i64, i64, i64),
) {
    let [width, height] = image.size;
    let round = |v: f32| {
        if v.is_finite() {
            v.round().clamp(-1.0e6, 1.0e6) as i64
        } else {
            -1
        }
    };
    let (mut x, mut y, x1, y1) = (round(a[0]), round(a[1]), round(b[0]), round(b[1]));
    let (dx, dy) = ((x1 - x).abs(), -(y1 - y).abs());
    let (sx, sy) = (if x < x1 { 1 } else { -1 }, if y < y1 { 1 } else { -1 });
    let mut err = dx + dy;
    // A line far outside the image is skipped rather than walked.
    if dx.max(-dy) > 4 * (width + height) as i64 {
        return;
    }
    loop {
        if x >= clip.0.max(0)
            && y >= clip.1.max(0)
            && x < clip.2.min(width as i64)
            && y < clip.3.min(height as i64)
        {
            image.pixels[y as usize * width + x as usize] = color;
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// The convex hull of 2D points, counter-clockwise in image space (Andrew's monotone chain,
/// A. M. Andrew 1979).
#[must_use]
pub fn convex_hull_2d(points: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let mut pts: Vec<[f32; 2]> = points
        .iter()
        .copied()
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .collect();
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross =
        |o: [f32; 2], a: [f32; 2], b: [f32; 2]| (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
    let mut hull: Vec<[f32; 2]> = Vec::with_capacity(pts.len() * 2);
    for pass in 0..2 {
        let start = hull.len();
        let iter: Box<dyn Iterator<Item = &[f32; 2]>> = if pass == 0 {
            Box::new(pts.iter())
        } else {
            Box::new(pts.iter().rev())
        };
        for &p in iter {
            while hull.len() >= start + 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    hull
}

/// Blends `color` at `alpha` into every pixel whose center lies inside the convex polygon
/// `outline` (either winding).
fn fill_convex(image: &mut egui::ColorImage, outline: &[[f32; 2]], color: egui::Color32, alpha: f32) {
    if outline.len() < 3 {
        return;
    }
    let [width, height] = image.size;
    let (mut min, mut max) = ([f32::MAX; 2], [f32::MIN; 2]);
    for p in outline {
        min = [min[0].min(p[0]), min[1].min(p[1])];
        max = [max[0].max(p[0]), max[1].max(p[1])];
    }
    let x0 = min[0].floor().max(0.0) as usize;
    let y0 = min[1].floor().max(0.0) as usize;
    let x1 = (max[0].ceil().max(0.0) as usize).min(width);
    let y1 = (max[1].ceil().max(0.0) as usize).min(height);
    let blend = |a: u8, b: u8| (f32::from(a) * (1.0 - alpha) + f32::from(b) * alpha).round() as u8;
    for y in y0..y1 {
        for x in x0..x1 {
            let c = [x as f32, y as f32];
            let mut sign = 0.0f32;
            let inside = outline.iter().enumerate().all(|(i, a)| {
                let b = outline[(i + 1) % outline.len()];
                let side = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
                if side == 0.0 {
                    return true;
                }
                if sign == 0.0 {
                    sign = side.signum();
                }
                side.signum() == sign
            });
            if inside {
                let px = &mut image.pixels[y * width + x];
                *px = egui::Color32::from_rgb(
                    blend(px.r(), color.r()),
                    blend(px.g(), color.g()),
                    blend(px.b(), color.b()),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Mesh helper actions (v0.6 UI parity P-088, ADR-7771): collision, lightmap UV, nav preview
// ---------------------------------------------------------------------------------------------

/// The collision a mesh helper asks the importer for: the `.meta` `collision` request that
/// `editor.set_import_settings` writes (the importer cooks hull sets from it, ADR-2301).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CollisionShape {
    /// One convex hull of the whole mesh (`collision: ConvexDecomposition` with `max_hulls: 1`).
    #[default]
    Convex,
    /// Several convex hulls from approximate convex decomposition (the importer's own hull
    /// settings stay as they are).
    Decomposition,
    /// No cooked collision (`collision: None`): takes the request back.
    None,
}

impl CollisionShape {
    /// Every shape, in menu order.
    pub const ALL: [Self; 3] = [Self::Convex, Self::Decomposition, Self::None];

    /// The protocol name (`convex`, `decomposition`, `none`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Convex => "convex",
            Self::Decomposition => "decomposition",
            Self::None => "none",
        }
    }

    /// The menu label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Convex => "Convex hull",
            Self::Decomposition => "Convex decomposition",
            Self::None => "No collision",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// A mesh helper button the shell runs (it needs the world or the open mesh).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshHelperRequest {
    /// Write the collision request into the mesh's `.meta` and recook it.
    AddCollision(CollisionShape),
    /// Unwrap the open mesh's lightmap UV and show the atlas.
    LightmapUv,
    /// Build the viewport's navigation preview for the open level.
    NavPreview,
}

/// The `collision_hulls` value Convex hull writes: one hull.
const CONVEX_HULL_PIN: &str = "(max_hulls: 1)";

/// The `.meta` settings object with the collision `shape` asked for: `base` (the source's own
/// settings, else what the project's import rules give it) keeps every other field.
///
/// # Errors
///
/// `base` is not a RON settings object (the parse message).
pub fn collision_settings(base: Option<&str>, shape: CollisionShape) -> Result<String, String> {
    use regia_asset::import_rules::SettingsObject;
    let mut object = SettingsObject::parse(base.unwrap_or("()")).map_err(|e| e.to_string())?;
    let mut patch = SettingsObject::default();
    let mut set = |name: &str, value: &str| patch.fields.push((name.to_string(), value.to_string()));
    match shape {
        CollisionShape::Convex => {
            set("collision", "ConvexDecomposition");
            set("collision_hulls", CONVEX_HULL_PIN);
        }
        CollisionShape::Decomposition => set("collision", "ConvexDecomposition"),
        CollisionShape::None => set("collision", "None"),
    }
    // Leaving Convex hull drops its one-hull pin, so a decomposition gets the importer's default
    // hull count again (review sprint-w12-rev-g2 #7); hull settings someone wrote are kept.
    if shape != CollisionShape::Convex {
        object
            .fields
            .retain(|(name, value)| !(name == "collision_hulls" && value.trim() == CONVEX_HULL_PIN));
    }
    object.merge(&patch);
    Ok(object.to_ron())
}

/// The settings a glTF source cooks with now: its own `.meta` settings, else what the project's
/// import rules (`config/import_rules.ron`) give its path, else none.
///
/// # Errors
///
/// `asset_not_imported` (no `.meta`), or a `.meta` or rules file that does not parse.
fn current_settings(root: &Path, rel: &str) -> Result<Option<String>, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    let full = crate::asset_ops_agent::assets_request_path(root, "path", rel)?;
    if !full.is_file() {
        return Err(RpcError::not_found(
            format!("{rel} does not exist or is not a file"),
            "editor.list_assets shows the project's files",
        ));
    }
    let meta_path = regia_asset::meta_path_for(&full);
    let bytes = std::fs::read(&meta_path).map_err(|_| {
        crate::asset_ops_agent::reasoned(
            "asset_not_imported",
            format!("{rel} has no .meta: cook it once first (import it, or run editor.reimport)"),
        )
    })?;
    let meta = regia_asset::MetaSidecar::from_ron(&bytes)
        .map_err(|e| RpcError::invalid_params(format!("{} does not parse: {e}", meta_path.display())))?;
    if meta.settings.is_some() {
        return Ok(meta.settings);
    }
    let rules = regia_asset::import_rules::ImportRulesFile::for_project(root)
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let rel_slash = crate::content::slash_path(full.strip_prefix(root).unwrap_or(&full));
    Ok(rules.and_then(|rules| {
        let matched = rules.matched_rules(&rel_slash, "gltf");
        regia_asset::import_rules::resolve_settings(None, &matched)
    }))
}

/// The helper actions of `editor.mesh_helper`.
pub const MESH_HELPER_ACTIONS: [&str; 3] = ["add_collision", "lightmap_uv", "nav_preview"];

/// The error a helper refused with: a stable lowercase `code` and what to do.
fn helper_error(code: &str, message: impl Into<String>) -> regia_agent::protocol::RpcError {
    crate::asset_ops_agent::reasoned(code, message)
}

/// Whether `path` is a glTF source the importer can write a collision request for.
pub(crate) fn is_gltf_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    !lower.starts_with("builtin:") && (lower.ends_with(".glb") || lower.ends_with(".gltf"))
}

/// Writes the collision request for `path` into its `.meta` through the import-settings API
/// (`editor.set_import_settings`: one write-and-cook job) and answers that call's result plus
/// `shape` and the `settings` written. With `asynchronous` the answer is `{ job }` at once.
///
/// # Errors
///
/// `mesh_helper_not_importable` (a built-in primitive or a file that is not `.glb`/`.gltf`),
/// `mesh_helper_no_project`, `asset_not_imported`, or what the import-settings write refuses
/// (`tools_busy`, `import_settings_apply_running`, `asset_settings_invalid`).
pub fn add_collision(
    world: &mut World,
    path: &str,
    shape: CollisionShape,
    asynchronous: bool,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    if !is_gltf_source(path) {
        return Err(helper_error(
            "mesh_helper_not_importable",
            format!(
                "{path}: collision is cooked from a project .glb or .gltf (a built-in shape has no import settings)"
            ),
        ));
    }
    let root = crate::mesh_source::project_root(world).ok_or_else(|| {
        helper_error(
            "mesh_helper_no_project",
            "no project is open: open one before changing import settings",
        )
    })?;
    let base = current_settings(&root, path)?;
    let settings = collision_settings(base.as_deref(), shape).map_err(|e| {
        helper_error(
            "asset_settings_invalid",
            format!("{path}: the current settings do not parse: {e}"),
        )
    })?;
    crate::import_settings_panel::tick_apply_job(world);
    let mut out = crate::import_settings_panel::set_import_settings_job(
        world,
        &root,
        path,
        Some(&settings),
        asynchronous,
    )?;
    if let Some(object) = out.as_object_mut() {
        object.insert("action".into(), serde_json::json!("add_collision"));
        object.insert("shape".into(), serde_json::json!(shape.name()));
        object.insert("settings".into(), serde_json::json!(settings));
    }
    Ok(out)
}

/// Builds the viewport's navigation preview for the open level (`editor.nav_preview`'s `build`);
/// the bake runs as the job the answer names.
///
/// # Errors
///
/// What `editor.nav_preview` refuses (`nav_preview_busy`, `nav_preview_too_large`, ...).
pub fn build_nav_preview(
    world: &mut World,
    asynchronous: bool,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let (_, handler) = crate::nav_preview::methods()
        .into_iter()
        .next()
        .ok_or_else(|| regia_agent::protocol::RpcError::internal("editor.nav_preview is not registered"))?;
    let mut out = handler(
        world,
        &serde_json::json!({ "action": "build", "async": asynchronous }),
    )?;
    if let Some(object) = out.as_object_mut() {
        object.insert("action".into(), serde_json::json!("nav_preview"));
    }
    Ok(out)
}

/// The lightmap UV unwrap of one mesh, as `regia bake-lightmaps` does it (the same generator and
/// settings, this mesh alone), ready to show.
#[derive(Clone, Debug, PartialEq)]
pub struct LightmapUvPreview {
    /// What the layout did.
    pub report: regia_asset_pipeline::lightmap_uv::LightmapUvReport,
    /// The mesh's rectangle in the atlas, texels `[x, y, width, height]`.
    pub rect: [u32; 4],
    /// Texels per meter the mesh was laid out at.
    pub texels_per_metre: f32,
    /// Triangles of the source mesh.
    pub source_triangles: usize,
    /// Vertices of the source mesh.
    pub source_vertices: usize,
    /// Vertices after the unwrap split them at chart seams.
    pub unwrapped_vertices: usize,
    /// The unwrapped triangles in local UV space (0..1, glTF convention: `(0, 0)` top left).
    pub triangles: Vec<[[f32; 2]; 3]>,
}

impl LightmapUvPreview {
    /// One line for the viewer's status.
    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "Lightmap UV: {} charts, {}×{} texels at {:.1} texels/m, {:.0}% of the atlas in charts, {} → {} vertices",
            self.report.charts,
            self.rect[2],
            self.rect[3],
            self.texels_per_metre,
            self.report.chart_fill * 100.0,
            self.source_vertices,
            self.unwrapped_vertices,
        )
    }

    /// The protocol summary.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "charts": self.report.charts,
            "width": self.rect[2],
            "height": self.rect[3],
            "texels_per_metre": self.texels_per_metre,
            "chart_fill": self.report.chart_fill,
            "degenerate_triangles": self.report.degenerate_triangles,
            "source_triangles": self.source_triangles,
            "source_vertices": self.source_vertices,
            "unwrapped_vertices": self.unwrapped_vertices,
            "triangles": self.triangles.len(),
            "generator": self.report.generator,
        })
    }

    /// The atlas drawn as `side` × `side` pixels: the frame of the unit square and every triangle
    /// of the unwrap (the UV overlay's drawing) on `backdrop` (a theme token: the viewer passes
    /// the theme's recessed fill).
    #[must_use]
    pub fn image(&self, side: usize, backdrop: egui::Color32) -> egui::ColorImage {
        let side = side.clamp(32, 2048);
        let mut image = egui::ColorImage::filled([side, side], backdrop);
        let camera = OrbitCamera::framing((Vec3::splat(-1.0), Vec3::splat(1.0)), 0.0, 0.0);
        paint_overlay(
            &mut image,
            &camera,
            &OverlayGeometry::Uv {
                set: 1,
                triangles: self.triangles.clone(),
            },
        );
        image
    }
}

/// Unwraps the parts of one mesh (merged, model space) with the generator and settings
/// `regia bake-lightmaps` uses by default, this mesh alone.
///
/// # Errors
///
/// `mesh_helper_no_triangles` (nothing to unwrap) or `mesh_helper_unwrap_failed` (the generator
/// refused; its code is in the message).
pub fn lightmap_uv_preview(
    parts: &[ModelPart],
) -> Result<LightmapUvPreview, regia_agent::protocol::RpcError> {
    use regia_asset_pipeline::lightmap_uv::{LightmapUvSettings, UnwrapInput, build_atlas_layout};
    let _span =
        tracing::info_span!(target: "regia_editor", "mesh_lightmap_uv", parts = parts.len()).entered();
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    for part in parts {
        let base = positions.len() as u32;
        positions.extend(part.mesh.vertices.iter().map(|v| v.position));
        indices.extend(part.mesh.indices.iter().map(|i| base + i));
    }
    if indices.len() < 3 {
        return Err(helper_error(
            "mesh_helper_no_triangles",
            "the mesh has no triangles to unwrap",
        ));
    }
    let input = UnwrapInput {
        name: "mesh",
        positions: &positions,
        indices: &indices,
        density_scale: 1.0,
    };
    let (layout, report) = build_atlas_layout(&[input], &LightmapUvSettings::default(), 0).map_err(|e| {
        helper_error(
            "mesh_helper_unwrap_failed",
            format!("the lightmap UV unwrap failed: {e}"),
        )
    })?;
    let (Some(entry), Some(unwrap)) = (layout.entries.first(), layout.unwraps.first()) else {
        return Err(helper_error(
            "mesh_helper_unwrap_failed",
            "the unwrap produced no entry",
        ));
    };
    let triangles = unwrap
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .filter_map(|t| {
            let uv = |i: u32| unwrap.uv.get(i as usize).copied();
            Some([uv(t[0])?, uv(t[1])?, uv(t[2])?])
        })
        .collect();
    Ok(LightmapUvPreview {
        report,
        rect: entry.rect,
        texels_per_metre: entry.texels_per_metre,
        source_triangles: indices.len() / 3,
        source_vertices: positions.len(),
        unwrapped_vertices: unwrap.uv.len(),
        triangles,
    })
}

/// Content Browser requests for the viewer: a lightmap UV for a mesh the viewer is about to open
/// (the menu item cannot reach the viewer's state; the shell hands these over).
#[derive(Resource, Debug, Clone, Default)]
pub struct MeshHelperQueue {
    /// Project-relative mesh paths waiting for the viewer to open them.
    pub lightmap_uv: Vec<String>,
}

/// Runs what the viewer's buttons and the Content Browser asked for this frame. The shell calls it
/// right after drawing the viewer panel, with the panel's `state`; a request that needs the world
/// (the collision write, the nav preview) runs here, one that needs the open mesh (the lightmap
/// UV) runs here too, and the answer lands in `state.helper_toast`.
pub fn run_helper_requests(ctx: &egui::Context, state: &mut AssetViewerState, world: &mut World) {
    let mut requests: Vec<MeshHelperRequest> = state.helper_request.take().into_iter().collect();
    if let Some(open) = state.path.clone()
        && let Some(mut queue) = world.get_resource_mut::<MeshHelperQueue>()
        && queue.lightmap_uv.contains(&open)
        && matches!(state.loaded, Loaded::Mesh { .. })
    {
        queue.lightmap_uv.retain(|p| *p != open);
        requests.push(MeshHelperRequest::LightmapUv);
    }
    for request in requests {
        let toast = match request {
            MeshHelperRequest::AddCollision(shape) => match state.path.clone() {
                Some(path) => match add_collision(world, &path, shape, true) {
                    Ok(out) => format!(
                        "collision {} for {path}: import-settings job {}",
                        shape.name(),
                        out["job"].as_str().unwrap_or("started")
                    ),
                    Err(e) => format!("collision: {}", e.message),
                },
                None => "no mesh is open".to_string(),
            },
            MeshHelperRequest::NavPreview => match build_nav_preview(world, true) {
                Ok(out) => format!("nav preview: job {}", out["job"].as_str().unwrap_or("started")),
                Err(e) => format!("nav preview: {}", e.message),
            },
            MeshHelperRequest::LightmapUv => match &state.loaded {
                Loaded::Mesh { model, .. } => match lightmap_uv_preview(&model.parts) {
                    Ok(preview) => {
                        let texture = ctx.load_texture(
                            "asset-viewer-lightmap-uv",
                            preview.image(256, crate::theme::ui_tokens(ctx).well),
                            egui::TextureOptions::LINEAR,
                        );
                        let line = preview.summary_line();
                        state.lightmap_atlas = Some((preview, Some(texture)));
                        line
                    }
                    Err(e) => format!("lightmap UV: {}", e.message),
                },
                _ => "open a mesh first".to_string(),
            },
        };
        state.helper_toast = Some(toast);
    }
}

/// `editor.mesh_helper`.
fn mesh_helper(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    use serde_json::{Value, json};
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let action = params.get("action").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`action` is required (add_collision, lightmap_uv, nav_preview)")
    })?;
    if !MESH_HELPER_ACTIONS.contains(&action) {
        return Err(helper_error(
            "mesh_helper_unknown_action",
            format!("unknown action `{action}` (add_collision, lightmap_uv, nav_preview)"),
        ));
    }
    if !path.to_ascii_lowercase().starts_with("builtin:") {
        crate::mesh_source::agent_asset_path(world, "path", path)?;
    }
    let asynchronous = params.get("async").and_then(Value::as_bool) == Some(true);
    match action {
        "add_collision" => {
            let shape = match params.get("shape").and_then(Value::as_str) {
                None => CollisionShape::Convex,
                Some(name) => CollisionShape::from_name(name).ok_or_else(|| {
                    helper_error(
                        "mesh_helper_unknown_shape",
                        format!("unknown shape `{name}` (convex, decomposition, none)"),
                    )
                })?,
            };
            add_collision(world, path, shape, asynchronous)
        }
        "nav_preview" => build_nav_preview(world, asynchronous),
        _ => {
            let root = crate::mesh_source::project_root(world);
            let library = world
                .get_resource_or_insert_with(crate::mesh_source::MeshLibrary::default)
                .clone();
            let model = library
                .model(root.as_deref(), path)
                .map_err(|err| RpcError::not_found(err, "check the path"))?;
            let preview = lightmap_uv_preview(&model.parts)?;
            let side = params
                .get("size")
                .and_then(Value::as_u64)
                .map_or(256, |v| v.clamp(32, 1024) as usize);
            let image = preview.image(side, crate::theme::ThemeTokens::regia_default().well);
            let png = crate::render::viewport_png_base64(&image).map_err(RpcError::internal)?;
            Ok(json!({
                "action": "lightmap_uv",
                "path": path,
                "atlas": preview.to_json(),
                "width": image.width(),
                "height": image.height(),
                "mime": "image/png",
                "png_base64": png,
            }))
        }
    }
}

/// The agent method of the mesh helpers.
#[must_use]
pub fn mesh_helper_methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    vec![(
        MethodSchema {
            name: "editor.mesh_helper",
            tool: "mesh_helper",
            description: "Mesh helper actions of the mesh viewer and the Content Browser mesh menu. \
                `add_collision` writes the `.meta` collision request through the import-settings API \
                (one write-and-cook job, as editor.set_import_settings) keeping every other setting: \
                `shape` convex (one hull of the mesh, the default), decomposition (several hulls) or \
                none (takes the request back). `lightmap_uv` unwraps the mesh's lightmap UV with the \
                generator and settings the lightmap bake uses, this mesh alone, and answers the atlas \
                figures and a PNG of the layout; nothing is written. `nav_preview` builds the viewport's \
                navigation preview for the open level (as editor.nav_preview build). Refusals: \
                mesh_helper_unknown_action, mesh_helper_unknown_shape, mesh_helper_not_importable, \
                mesh_helper_no_project, mesh_helper_no_triangles, mesh_helper_unwrap_failed, plus those \
                of the calls named above.",
            params: &[
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative mesh path (a .glb or .gltf under assets/; lightmap_uv also takes builtin:cube/plane/sphere).",
                },
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: true,
                    description: "add_collision, lightmap_uv or nav_preview.",
                },
                ParamSchema {
                    name: "shape",
                    kind: "string",
                    required: false,
                    description: "With add_collision: convex (default), decomposition or none.",
                },
                ParamSchema {
                    name: "async",
                    kind: "boolean",
                    required: false,
                    description: "With add_collision or nav_preview: true answers { job } at once (poll agent.job_status); absent or false waits for the job.",
                },
                ParamSchema {
                    name: "size",
                    kind: "number",
                    required: false,
                    description: "With lightmap_uv: the atlas image's side in pixels (32 to 1024, default 256).",
                },
            ],
            result: "add_collision: { action, shape, settings, path, cooked, skipped, errors, job } (async: { job, action, shape, settings }); lightmap_uv: { action, path, atlas, width, height, mime, png_base64 }; nav_preview: as editor.nav_preview plus action",
            mutating: true,
            permission: Permission::Assets,
        },
        mesh_helper as crate::viewport_agent::Handler,
    )]
}

#[cfg(test)]
mod tests {

    /// rev-claude-57 asset finding 2 (ADR-7495): an import-settings write that recooked drops the
    /// viewer's gathered overlays, so the collision overlay is gathered again from the new cook.
    #[test]
    fn an_import_settings_write_drops_the_gathered_overlays() {
        let mut state = AssetViewerState::default();
        state.drop_stale_overlays();
        let seen = state.overlay_writes;
        let refusal = || OverlayRefusal {
            code: "collision_not_cooked",
            message: "not cooked yet".to_string(),
        };
        state
            .overlay_cache
            .push((MeshOverlay::Collision, 0, Err(refusal())));
        state.drop_stale_overlays();
        // Other tests of this binary may write import settings meanwhile: only without a write
        // in between must the cache stay.
        if state.overlay_writes == seen {
            assert_eq!(state.overlay_cache.len(), 1, "no write since: kept");
        }
        crate::asset_ops_agent::IMPORT_SETTINGS_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        state.drop_stale_overlays();
        assert!(state.overlay_cache.is_empty(), "a write recooked: gathered again");
    }
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_follow_extensions() {
        assert_eq!(kind_of("assets/a.glb"), Some(AssetKind::Mesh));
        assert_eq!(kind_of("builtin:cube"), Some(AssetKind::Mesh));
        assert_eq!(kind_of("assets/t/Albedo.PNG"), Some(AssetKind::Texture));
        assert_eq!(kind_of("assets/s/boom.wav"), Some(AssetKind::Sound));
        assert_eq!(kind_of("assets/x.level.ron"), None);
    }

    #[test]
    fn sphere_lods_shrink_and_keep_materials() {
        let library = MeshLibrary::default();
        let model = library.model(None, "builtin:sphere").expect("sphere");
        let lods = build_lods(&model);
        assert!(lods.len() >= 3, "sphere simplifies: {}", lods.len());
        for pair in lods.windows(2) {
            assert!(pair[1].triangles < pair[0].triangles);
            assert!(Arc::ptr_eq(
                &pair[1].parts[0].material,
                &pair[0].parts[0].material
            ));
        }
    }

    #[test]
    fn odd_sized_mips_read_the_last_column_and_row() {
        // 5x1: columns 0..3 black, column 4 white; output (2x1) texel 1 covers columns 2..5.
        let mut rgba = [0u8, 0, 0, 255].repeat(5);
        rgba[16..19].copy_from_slice(&[255, 255, 255]);
        let chain = mip_chain(&LabTexture {
            width: 5,
            height: 1,
            rgba,
        });
        assert_eq!((chain[1].width, chain[1].height), (2, 1));
        assert_eq!(chain[1].rgba[0], 0);
        assert_eq!(chain[1].rgba[4], 85, "(0 + 0 + 255) / 3: the last column counts");
    }

    #[test]
    fn mips_halve_down_to_one_pixel_and_average() {
        let image = LabTexture {
            width: 4,
            height: 2,
            rgba: [[255u8, 0, 0, 255], [0, 0, 255, 255]]
                .iter()
                .cycle()
                .take(8)
                .flatten()
                .copied()
                .collect(),
        };
        let chain = mip_chain(&image);
        let sizes: Vec<(u32, u32)> = chain.iter().map(|m| (m.width, m.height)).collect();
        assert_eq!(sizes, vec![(4, 2), (2, 1), (1, 1)]);
        assert_eq!(&chain[1].rgba[0..4], &[128, 0, 128, 255]);
        let red = texture_view(&chain[0], Channel::R);
        assert_eq!(red.pixels[0], egui::Color32::from_gray(255));
        assert_eq!(red.pixels[1], egui::Color32::from_gray(0));
    }

    fn wav16(samples: &[i16], channels: u16, rate: u32) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn wav_decodes_and_draws_a_waveform() {
        let samples: Vec<i16> = (0..4800)
            .map(|i| ((i as f32 * 0.05).sin() * 16_000.0) as i16)
            .flat_map(|s| [s, s])
            .collect();
        let sound = decode_wav(&wav16(&samples, 2, 48_000)).expect("wav");
        assert_eq!(
            (sound.channels, sound.sample_rate, sound.frames()),
            (2, 48_000, 4800)
        );
        assert!((sound.seconds() - 0.1).abs() < 1e-4);
        let image = waveform_image(&sound, 200, 64);
        let lit = image
            .pixels
            .iter()
            .filter(|p| **p == egui::Color32::from_rgb(110, 250, 200))
            .count();
        assert!(lit > 200 * 10, "the wave covers the image: {lit}");
        assert!(decode_wav(b"RIFF....WAVEjunk").is_err());
        assert!(decode_wav(b"not a wav").is_err());
    }

    #[test]
    fn wav_24_bit_samples_keep_their_sign() {
        let data: [u8; 9] = [0x00, 0x00, 0x80, 0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x00];
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&48_000u32.to_le_bytes());
        out.extend_from_slice(&(48_000u32 * 3).to_le_bytes());
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        let sound = decode_wav(&out).expect("wav");
        assert_eq!(sound.samples[0], -1.0, "0x800000 is full-scale negative");
        assert!(sound.samples[1] > 0.99, "0x7FFFFF is full-scale positive");
        assert_eq!(sound.samples[2], 0.0);
    }

    // -----------------------------------------------------------------------------------------
    // Mesh overlays (ADR-7469)
    // -----------------------------------------------------------------------------------------

    /// A unit cube centered at `center` (24 vertices, outward winding) as a GLB: UV0 in a 3×2
    /// atlas, UV1 when asked, and a `collision_box` node reusing the mesh when asked.
    fn cube_glb(center: [f32; 3], uv1: bool, collision_node: bool) -> Vec<u8> {
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut uv0 = Vec::new();
        let mut uv1s = Vec::new();
        let mut indices: Vec<u16> = Vec::new();
        let axis = |i: usize| {
            let mut v = [0.0f32; 3];
            v[i % 3] = 1.0;
            Vec3::from_array(v)
        };
        for face in 0..6usize {
            let (i, s) = (face / 2, if face % 2 == 0 { 1.0 } else { -1.0 });
            let (n, a, b) = (axis(i) * s, axis(i + 1), axis(i + 2));
            let base = positions.len() as u16;
            for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
                let p = Vec3::from_array(center) + n * 0.5 + a * (u - 0.5) + b * (v - 0.5);
                positions.push(p.to_array());
                normals.push(n.to_array());
                let (cx, cy) = ((face % 3) as f32, (face / 3) as f32);
                uv0.push([(cx + u) / 3.0, (cy + v) / 2.0]);
                uv1s.push([0.1 + 0.3 * u, 0.6 + 0.3 * v]);
            }
            let quad = if s > 0.0 {
                [0, 1, 2, 0, 2, 3]
            } else {
                [0, 2, 1, 0, 3, 2]
            };
            indices.extend(quad.iter().map(|q| base + q));
        }
        let mut bin = Vec::new();
        let mut views = Vec::new();
        let mut push = |bytes: Vec<u8>, bin: &mut Vec<u8>| {
            let offset = bin.len();
            bin.extend_from_slice(&bytes);
            while !bin.len().is_multiple_of(4) {
                bin.push(0);
            }
            views.push(json!({ "buffer": 0, "byteOffset": offset, "byteLength": bytes.len() }));
            views.len() - 1
        };
        let f32s = |v: &[[f32; 3]]| {
            v.iter()
                .flatten()
                .flat_map(|f| f.to_le_bytes())
                .collect::<Vec<u8>>()
        };
        let f32s2 = |v: &[[f32; 2]]| {
            v.iter()
                .flatten()
                .flat_map(|f| f.to_le_bytes())
                .collect::<Vec<u8>>()
        };
        let p_view = push(f32s(&positions), &mut bin);
        let n_view = push(f32s(&normals), &mut bin);
        let t0_view = push(f32s2(&uv0), &mut bin);
        let t1_view = push(f32s2(&uv1s), &mut bin);
        let i_view = push(indices.iter().flat_map(|i| i.to_le_bytes()).collect(), &mut bin);
        let lo = center.map(|c| c - 0.5);
        let hi = center.map(|c| c + 0.5);
        let mut accessors = vec![
            json!({ "bufferView": p_view, "componentType": 5126, "count": 24, "type": "VEC3", "min": lo, "max": hi }),
            json!({ "bufferView": n_view, "componentType": 5126, "count": 24, "type": "VEC3" }),
            json!({ "bufferView": t0_view, "componentType": 5126, "count": 24, "type": "VEC2" }),
            json!({ "bufferView": i_view, "componentType": 5123, "count": 36, "type": "SCALAR" }),
        ];
        let mut attributes = json!({ "POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2 });
        if uv1 {
            accessors
                .push(json!({ "bufferView": t1_view, "componentType": 5126, "count": 24, "type": "VEC2" }));
            attributes["TEXCOORD_1"] = json!(4);
        }
        let mut nodes = vec![json!({ "name": "box", "mesh": 0 })];
        if collision_node {
            nodes.push(json!({ "name": "collision_box", "mesh": 0, "scale": [1.2, 1.2, 1.2] }));
        }
        let scene_nodes: Vec<usize> = (0..nodes.len()).collect();
        let doc = json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": scene_nodes }],
            "nodes": nodes,
            "meshes": [{ "primitives": [{ "attributes": attributes, "indices": 3 }] }],
            "buffers": [{ "byteLength": bin.len() }],
            "bufferViews": views,
            "accessors": accessors,
        });
        let mut text = serde_json::to_vec(&doc).expect("json");
        while !text.len().is_multiple_of(4) {
            text.push(b' ');
        }
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&((12 + 8 + text.len() + 8 + bin.len()) as u32).to_le_bytes());
        glb.extend_from_slice(&(text.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&text);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    /// A scratch project holding `assets/<name>` = `bytes`.
    fn overlay_project(label: &str, name: &str, bytes: &[u8]) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        std::fs::write(root.join("assets").join(name), bytes).expect("glb");
        root
    }

    fn mesh_of(root: &Path, rel: &str) -> (Arc<LoadedModel>, Vec<ModelLod>) {
        match load_asset(Some(root), &MeshLibrary::default(), rel).expect("load") {
            Loaded::Mesh { model, lods } => (model, lods),
            _ => panic!("{rel} is not a mesh"),
        }
    }

    #[test]
    fn overlay_names_round_trip() {
        for overlay in MeshOverlay::ALL {
            assert_eq!(MeshOverlay::from_name(overlay.name()), Some(overlay));
        }
        assert_eq!(MeshOverlay::from_name("uv0"), Some(MeshOverlay::Uv0));
        assert_eq!(MeshOverlay::from_name("wireframe"), None);
    }

    #[test]
    fn uv_layouts_read_each_set_and_a_missing_set_is_refused() {
        let root = overlay_project("overlay-uv", "plain.glb", &cube_glb([0.0; 3], false, false));
        std::fs::write(
            root.join("assets/lightmapped.glb"),
            cube_glb([0.0; 3], true, true),
        )
        .expect("glb");
        let (model, lods) = mesh_of(&root, "assets/plain.glb");
        match overlay_geometry(
            Some(&root),
            "assets/plain.glb",
            &model,
            &lods[0],
            MeshOverlay::Uv0,
        ) {
            Ok(OverlayGeometry::Uv { set: 0, triangles }) => assert_eq!(triangles.len(), 12),
            other => panic!("uv0: {other:?}"),
        }
        let refused = overlay_geometry(
            Some(&root),
            "assets/plain.glb",
            &model,
            &lods[0],
            MeshOverlay::Uv1,
        )
        .expect_err("a mesh with no UV1");
        assert_eq!(refused.code, "uv_set_missing");
        let (model, lods) = mesh_of(&root, "assets/lightmapped.glb");
        match overlay_geometry(
            Some(&root),
            "assets/lightmapped.glb",
            &model,
            &lods[0],
            MeshOverlay::Uv1,
        ) {
            // The collision node shares the mesh: each mesh is laid out once.
            Ok(OverlayGeometry::Uv { set: 1, triangles }) => assert_eq!(triangles.len(), 12),
            other => panic!("uv1: {other:?}"),
        }
        // A builtin has UV0 only.
        let library = MeshLibrary::default();
        let cube = library.model(None, "builtin:cube").expect("builtin");
        let lods = build_lods(&cube);
        assert!(overlay_geometry(None, "builtin:cube", &cube, &lods[0], MeshOverlay::Uv0).is_ok());
        assert_eq!(
            overlay_geometry(None, "builtin:cube", &cube, &lods[0], MeshOverlay::Uv1)
                .expect_err("builtin uv1")
                .code,
            "uv_set_missing"
        );
    }

    #[test]
    fn collision_overlay_reads_collision_nodes_and_refuses_a_mesh_without_a_collider() {
        let root = overlay_project(
            "overlay-collision",
            "plain.glb",
            &cube_glb([0.0; 3], false, false),
        );
        std::fs::write(root.join("assets/hull.glb"), cube_glb([0.0; 3], false, true)).expect("glb");
        let (model, lods) = mesh_of(&root, "assets/plain.glb");
        let refused = overlay_geometry(
            Some(&root),
            "assets/plain.glb",
            &model,
            &lods[0],
            MeshOverlay::Collision,
        )
        .expect_err("no collider");
        assert_eq!(refused.code, "no_collision");
        let (model, lods) = mesh_of(&root, "assets/hull.glb");
        match overlay_geometry(
            Some(&root),
            "assets/hull.glb",
            &model,
            &lods[0],
            MeshOverlay::Collision,
        ) {
            Ok(OverlayGeometry::Collision {
                source: "collision_nodes",
                triangles,
                ..
            }) => {
                assert_eq!(triangles.len(), 12);
                // The node's 1.2 scale is baked.
                let reach = triangles
                    .iter()
                    .flatten()
                    .map(|p| p.abs().max_element())
                    .fold(0.0, f32::max);
                assert!((reach - 0.6).abs() < 1e-4, "{reach}");
            }
            other => panic!("collision: {other:?}"),
        }
    }

    #[test]
    fn collision_overlay_reads_the_cooked_hulls_in_the_viewer_space() {
        // The cube sits off-center on +X: the cook bakes the `forward: Convert` yaw (x → -x),
        // and the overlay must map the hulls back onto the viewer's model.
        let root = regia_test_utils::unique_temp_dir("overlay-cooked");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let source = root.join("offset.glb");
        std::fs::write(&source, cube_glb([1.0, 0.0, 0.0], false, false)).expect("glb");
        crate::content::import_files(&root, "", std::slice::from_ref(&source), Some("test-agent"))
            .expect("import");
        let (model, lods) = mesh_of(&root, "assets/offset.glb");
        assert_eq!(
            overlay_geometry(
                Some(&root),
                "assets/offset.glb",
                &model,
                &lods[0],
                MeshOverlay::Collision
            )
            .expect_err("not asked for yet")
            .code,
            "no_collision"
        );
        crate::asset_ops_agent::set_import_settings_at(
            &root,
            "assets/offset.glb",
            Some("(collision: ConvexDecomposition, collision_hulls: (max_hulls: 2, resolution: 16))"),
        )
        .expect("collision settings");
        match overlay_geometry(
            Some(&root),
            "assets/offset.glb",
            &model,
            &lods[0],
            MeshOverlay::Collision,
        ) {
            Ok(OverlayGeometry::Collision {
                source: "cooked_hulls",
                hulls,
                ..
            }) => {
                assert!(!hulls.is_empty());
                for p in hulls.iter().flatten() {
                    assert!(
                        p.x > 0.4 && p.x < 1.6 && p.y.abs() < 0.6 && p.z.abs() < 0.6,
                        "hull vertex {p} is outside the viewer's cube at x 0.5..1.5"
                    );
                }
            }
            other => panic!("cooked collision: {other:?}"),
        }
    }

    /// Pixels that differ between two images of one size.
    fn changed(a: &egui::ColorImage, b: &egui::ColorImage) -> Vec<(usize, usize)> {
        let width = a.size[0];
        a.pixels
            .iter()
            .zip(&b.pixels)
            .enumerate()
            .filter(|(_, (x, y))| x != y)
            .map(|(i, _)| (i % width, i / width))
            .collect()
    }

    /// The pixel rectangle `(x0, y0, x1, y1)` the box `min..max` projects into.
    fn projected_rect(camera: &OrbitCamera, min: Vec3, max: Vec3, size: [f32; 2]) -> (f32, f32, f32, f32) {
        let mut r = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for i in 0..8 {
            let c = Vec3::new(
                if i & 1 == 0 { min.x } else { max.x },
                if i & 2 == 0 { min.y } else { max.y },
                if i & 4 == 0 { min.z } else { max.z },
            );
            let p = camera.project(c, size).expect("in front");
            r = (r.0.min(p[0]), r.1.min(p[1]), r.2.max(p[0]), r.3.max(p[1]));
        }
        r
    }

    /// Overlays touch only their region of a control image (CPU; the lavapipe test below does
    /// the same over a real render).
    fn assert_overlay_regions(control: &egui::ColorImage, root: &Path, rel: &str, yaw: f32, pitch: f32) {
        let (model, lods) = mesh_of(root, rel);
        let [w, h] = control.size;
        let size = [w as f32, h as f32];
        let camera = OrbitCamera::framing((model.aabb_min, model.aabb_max), yaw, pitch);
        // UV: inside the centered square only.
        let mut uv = control.clone();
        let geometry = overlay_geometry(Some(root), rel, &model, &lods[0], MeshOverlay::Uv0).expect("uv");
        paint_overlay(&mut uv, &camera, &geometry);
        let (x0, y0, side) = uv_square(w, h);
        let diff = changed(control, &uv);
        assert!(
            diff.len() > side * 4,
            "the UV layout draws: {} pixels",
            diff.len()
        );
        assert!(
            diff.iter()
                .all(|&(x, y)| x >= x0 && x < x0 + side && y >= y0 && y < y0 + side),
            "the UV layout stays in its square"
        );
        // Normals: around the mesh's projected bounds, grown by the normal length.
        let mut normals = control.clone();
        let geometry =
            overlay_geometry(Some(root), rel, &model, &lods[0], MeshOverlay::Normals).expect("normals");
        paint_overlay(&mut normals, &camera, &geometry);
        let grow = Vec3::splat((model.aabb_max - model.aabb_min).length() * NORMAL_LENGTH);
        let r = projected_rect(&camera, model.aabb_min - grow, model.aabb_max + grow, size);
        let diff = changed(control, &normals);
        assert!(!diff.is_empty(), "the normals draw");
        assert!(
            diff.iter().all(|&(x, y)| {
                let (x, y) = (x as f32, y as f32);
                x >= r.0 - 1.0 && x <= r.2 + 1.0 && y >= r.1 - 1.0 && y <= r.3 + 1.0
            }),
            "the normals stay around the mesh: {r:?}"
        );
        // Collision: within the collider's projected bounds (the node is 1.2× the mesh).
        let mut collision = control.clone();
        let geometry =
            overlay_geometry(Some(root), rel, &model, &lods[0], MeshOverlay::Collision).expect("collision");
        paint_overlay(&mut collision, &camera, &geometry);
        let r = projected_rect(&camera, Vec3::splat(-0.6), Vec3::splat(0.6), size);
        let diff = changed(control, &collision);
        assert!(diff.len() > 50, "the collider draws: {}", diff.len());
        assert!(
            diff.iter().all(|&(x, y)| {
                let (x, y) = (x as f32, y as f32);
                x >= r.0 - 1.0 && x <= r.2 + 1.0 && y >= r.1 - 1.0 && y <= r.3 + 1.0
            }),
            "the collider stays in its projected bounds: {r:?}"
        );
    }

    #[test]
    fn overlays_paint_only_their_region_of_a_flat_image() {
        let root = overlay_project("overlay-regions", "cube.glb", &cube_glb([0.0; 3], true, true));
        let control =
            egui::ColorImage::new([160, 120], vec![egui::Color32::from_rgb(90, 100, 110); 160 * 120]);
        assert_overlay_regions(&control, &root, "assets/cube.glb", 0.6, 0.35);
    }

    #[test]
    fn hull_outline_is_the_convex_hull() {
        let hull = convex_hull_2d(&[
            [0.0, 0.0],
            [2.0, 0.0],
            [1.0, 1.0],
            [2.0, 2.0],
            [0.0, 2.0],
            [1.0, 0.5],
        ]);
        assert_eq!(hull.len(), 4, "{hull:?}");
        for corner in [[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]] {
            assert!(hull.contains(&corner), "{hull:?}");
        }
        assert_eq!(convex_hull_2d(&[[1.0, 1.0], [1.0, 1.0]]).len(), 1);
    }

    /// `editor.view_asset { overlay }` through the agent dispatcher on lavapipe: each overlay's
    /// image differs from the no-overlay image only in its region (checked on the same render
    /// the method makes), and a mesh with no UV1 answers `uv_set_missing`.
    #[test]
    fn view_asset_overlays_through_the_dispatcher() {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => {
                eprintln!("skip view_asset_overlays_through_the_dispatcher: no headless adapter: {err}");
                return;
            }
        };
        let root = overlay_project("overlay-dispatch", "cube.glb", &cube_glb([0.0; 3], true, true));
        std::fs::write(root.join("assets/plain.glb"), cube_glb([0.0; 3], false, false)).expect("glb");
        let mut app = crate::engine::app::App::new();
        app.add_plugins(crate::engine::app::MinimalPlugins);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        crate::agent_ext::install(app.world_mut());
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        app.world_mut().insert_resource(device.clone());
        app.finish_plugins();
        let (sender, inbox) = crossbeam_channel::unbounded();
        app.insert_resource(regia_agent::AgentInbox(inbox));
        let mut call = |params: serde_json::Value| {
            let (reply, answer) = crossbeam_channel::bounded(1);
            sender
                .send(regia_agent::server::Inbound {
                    request: regia_agent::protocol::Request {
                        jsonrpc: "2.0".into(),
                        id: Some(json!(1)),
                        method: "editor.view_asset".into(),
                        params,
                    },
                    reply,
                })
                .expect("agent inbox");
            app.update();
            answer.try_recv().expect("dispatch answers in one frame")
        };
        let ok = |response: regia_agent::protocol::Response| {
            assert!(response.error.is_none(), "{:?}", response.error);
            response.result.expect("result")
        };
        let base = ok(call(
            json!({ "path": "assets/cube.glb", "width": 160, "height": 120 }),
        ));
        for overlay in ["uv", "uv1", "normals", "collision"] {
            let out = ok(call(
                json!({ "path": "assets/cube.glb", "width": 160, "height": 120, "overlay": overlay }),
            ));
            assert_ne!(
                out["png_base64"], base["png_base64"],
                "{overlay} changes the image"
            );
            assert_eq!(out["info"]["overlays"][0]["overlay"], overlay, "{out}");
        }
        let both = ok(call(
            json!({ "path": "assets/cube.glb", "width": 160, "height": 120, "overlay": ["normals", "collision"] }),
        ));
        assert_eq!(both["info"]["overlays"].as_array().map(Vec::len), Some(2));
        let reason = |response: regia_agent::protocol::Response| {
            response
                .error
                .and_then(|e| e.data)
                .and_then(|d| {
                    d.get("reason")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default()
        };
        assert_eq!(
            reason(call(json!({ "path": "assets/plain.glb", "overlay": "uv1" }))),
            "uv_set_missing"
        );
        assert_eq!(
            reason(call(
                json!({ "path": "assets/plain.glb", "overlay": "collision" })
            )),
            "no_collision"
        );
        assert_eq!(
            reason(call(json!({ "path": "assets/cube.glb", "overlay": "wire" }))),
            "overlay_unknown"
        );
        // The regions, over the same render the method makes.
        let (model, lods) = mesh_of(&root, "assets/cube.glb");
        let control = render_model(
            &device,
            &lods[0].parts,
            (model.aabb_min, model.aabb_max),
            0.6,
            0.35,
            [160, 120],
        )
        .expect("render");
        assert_overlay_regions(&control, &root, "assets/cube.glb", 0.6, 0.35);
    }

    /// Every format `regia_audio` decodes is a sound asset and decodes in the preview path (the
    /// `.ogg` here was written by the cook's own Vorbis encoder).
    #[test]
    fn an_encoded_ogg_decodes_in_the_preview_path() {
        for ext in [".wav", ".ogg", ".flac", ".mp3"] {
            assert_eq!(kind_of(&format!("s/loop{ext}")), Some(AssetKind::Sound), "{ext}");
        }
        assert_eq!(kind_of("s/loop.xyz"), None);
        let samples: Vec<i16> = (0..960)
            .map(|i| ((i as f32 * 0.05).sin() * 12_000.0) as i16)
            .collect();
        let wav = wav16(&samples, 1, 48_000);
        let ogg = regia_asset_pipeline::audio_transcode::transcode_to_vorbis(&wav, "loop.ogg", 0.4)
            .expect("encode");
        assert!(ogg.starts_with(b"OggS"), "a real Vorbis stream");
        let sound = decode_sound(&ogg, "s/loop.ogg").expect("decode");
        assert_eq!(sound.sample_rate, 48_000);
        assert_eq!(sound.channels, 1, "the mono source stays one channel");
        assert!(
            sound.frames() >= 960 && sound.frames() < 960 + 8192,
            "the frames round trip ({} of 960)",
            sound.frames()
        );
        assert!(
            sound.samples.iter().take(960).any(|s| s.abs() > 0.1),
            "the waveform is not flat"
        );
        // The preview draws and plays it like a WAV.
        assert!(!waveform_image(&sound, 200, 64).pixels.is_empty());
        // A file that is not a sound answers an error naming it.
        let err = decode_sound(b"not music at all", "s/loop.ogg").expect_err("garbage");
        assert!(err.contains("s/loop.ogg"), "{err}");
        // A RIFF file still takes the pure WAV reader whatever it is named.
        assert_eq!(decode_sound(&wav, "s/loop.ogg").expect("wav").sample_rate, 48_000);
    }

    // -----------------------------------------------------------------------------------------
    // Mesh helper actions and the preview scene (ADR-7771)
    // -----------------------------------------------------------------------------------------

    /// A scratch project with a cooked cube GLB (`assets/cube.glb` and its `.meta`) and the world
    /// the editor methods read it from.
    fn helper_world(label: &str) -> (World, PathBuf) {
        let root = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let source = root.join("cube.glb");
        std::fs::write(&source, cube_glb([0.0; 3], false, false)).expect("glb");
        crate::content::import_files(&root, "", std::slice::from_ref(&source), Some("test-agent"))
            .expect("import");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        (world, root)
    }

    #[test]
    fn collision_settings_set_the_request_and_keep_every_other_field() {
        let convex = collision_settings(Some("(vertex_ao: true, scale: 2.0)"), CollisionShape::Convex)
            .expect("convex");
        assert_eq!(
            convex,
            "(vertex_ao: true, scale: 2.0, collision: ConvexDecomposition, collision_hulls: (max_hulls: 1))"
        );
        // A second request replaces the first one's fields in place and keeps the rest.
        let many = collision_settings(Some(&convex), CollisionShape::Decomposition).expect("many");
        assert!(many.contains("collision: ConvexDecomposition"), "{many}");
        assert!(
            many.contains("vertex_ao: true") && many.contains("scale: 2.0"),
            "{many}"
        );
        assert_eq!(many.matches("collision:").count(), 1, "{many}");
        assert!(
            !many.contains("collision_hulls"),
            "Convex hull's one-hull pin goes, so the importer default applies: {many}"
        );
        let tuned = collision_settings(
            Some("(collision_hulls: (max_hulls: 2, resolution: 16))"),
            CollisionShape::Decomposition,
        )
        .expect("tuned");
        assert!(
            tuned.contains("max_hulls: 2"),
            "hull settings someone wrote stay: {tuned}"
        );
        let none = collision_settings(Some(&many), CollisionShape::None).expect("none");
        assert!(
            none.contains("collision: None") && none.contains("vertex_ao: true"),
            "{none}"
        );
        assert_eq!(
            collision_settings(None, CollisionShape::Convex).expect("fresh"),
            "(collision: ConvexDecomposition, collision_hulls: (max_hulls: 1))"
        );
        assert!(collision_settings(Some("not an object"), CollisionShape::Convex).is_err());
        for shape in CollisionShape::ALL {
            assert_eq!(CollisionShape::from_name(shape.name()), Some(shape));
        }
        // Every request the helper writes is one the importer accepts.
        for shape in CollisionShape::ALL {
            let text = collision_settings(None, shape).expect("settings");
            crate::import_settings_panel::validate_settings("assets/cube.glb", &text)
                .unwrap_or_else(|e| panic!("{shape:?}: {e}"));
        }
    }

    /// Add simple collision writes the request through the import-settings API: the `.meta` carries
    /// it, the other settings survive, and the recook gives the collision overlay one hull.
    #[test]
    fn add_simple_collision_writes_the_meta_and_the_recook_has_one_hull() {
        let (mut world, root) = helper_world("mesh-helper-collision");
        crate::asset_ops_agent::set_import_settings_at(&root, "assets/cube.glb", Some("(vertex_ao: true)"))
            .expect("own settings");
        let out = add_collision(&mut world, "assets/cube.glb", CollisionShape::Convex, false).expect("add");
        assert_eq!(out["action"], "add_collision");
        assert_eq!(out["shape"], "convex");
        assert!(out["errors"].as_array().is_some_and(Vec::is_empty), "{out}");
        let meta = regia_asset::MetaSidecar::from_ron(
            &std::fs::read(root.join("assets/cube.glb.meta")).expect("meta"),
        )
        .expect("parse");
        let settings = meta.settings.expect("settings written");
        assert!(settings.contains("vertex_ao: true"), "kept: {settings}");
        assert!(settings.contains("collision: ConvexDecomposition"), "{settings}");
        assert!(settings.contains("collision_hulls: (max_hulls: 1)"), "{settings}");
        let (model, lods) = mesh_of(&root, "assets/cube.glb");
        match overlay_geometry(
            Some(&root),
            "assets/cube.glb",
            &model,
            &lods[0],
            MeshOverlay::Collision,
        ) {
            Ok(OverlayGeometry::Collision { source, hulls, .. }) => {
                assert_eq!(source, "cooked_hulls");
                assert_eq!(hulls.len(), 1, "one convex hull of the whole mesh");
            }
            other => panic!("collision overlay after the helper: {other:?}"),
        }
        // Taking the request back writes `collision: None` and keeps the rest.
        add_collision(&mut world, "assets/cube.glb", CollisionShape::None, false).expect("none");
        let meta = regia_asset::MetaSidecar::from_ron(
            &std::fs::read(root.join("assets/cube.glb.meta")).expect("meta"),
        )
        .expect("parse");
        let settings = meta.settings.expect("settings");
        assert!(
            settings.contains("collision: None") && settings.contains("vertex_ao: true"),
            "{settings}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn add_simple_collision_keeps_the_settings_the_import_rules_gave() {
        let (mut world, root) = helper_world("mesh-helper-rules");
        std::fs::create_dir_all(root.join("config")).expect("config");
        std::fs::write(
            root.join("config/import_rules.ron"),
            "(format_version: 1, rules: [(glob: \"assets/**\", importer: \"gltf\", settings: \"(scale: 2.0)\")])",
        )
        .expect("rules");
        let out = add_collision(&mut world, "assets/cube.glb", CollisionShape::Convex, false).expect("add");
        let settings = out["settings"].as_str().expect("settings").to_string();
        assert!(
            settings.contains("scale: 2.0"),
            "the rule's field is carried into the .meta: {settings}"
        );
        assert!(settings.contains("collision: ConvexDecomposition"), "{settings}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mesh_helper_refusals_have_their_codes() {
        let (mut world, root) = helper_world("mesh-helper-refusals");
        let reason = |e: regia_agent::protocol::RpcError| {
            e.data
                .as_ref()
                .and_then(|d| d["reason"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(
            reason(
                add_collision(&mut world, "builtin:cube", CollisionShape::Convex, false)
                    .expect_err("builtin")
            ),
            "mesh_helper_not_importable"
        );
        assert_eq!(
            reason(
                add_collision(&mut world, "assets/sound.wav", CollisionShape::Convex, false)
                    .expect_err("wav")
            ),
            "mesh_helper_not_importable"
        );
        let missing =
            add_collision(&mut world, "assets/gone.glb", CollisionShape::Convex, false).expect_err("missing");
        assert!(missing.message.contains("does not exist"), "{}", missing.message);
        std::fs::write(root.join("assets/raw.glb"), cube_glb([0.0; 3], false, false)).expect("glb");
        assert_eq!(
            reason(
                add_collision(&mut world, "assets/raw.glb", CollisionShape::Convex, false)
                    .expect_err("no meta")
            ),
            "asset_not_imported"
        );
        let mut empty = World::new();
        assert_eq!(
            reason(
                add_collision(&mut empty, "assets/cube.glb", CollisionShape::Convex, false)
                    .expect_err("no project")
            ),
            "mesh_helper_no_project"
        );
        let empty_parts: Vec<ModelPart> = Vec::new();
        assert_eq!(
            reason(lightmap_uv_preview(&empty_parts).expect_err("nothing to unwrap")),
            "mesh_helper_no_triangles"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Generate lightmap UV unwraps the mesh with the bake's generator: a cube becomes a handful of
    /// planar charts inside the unit square, one triangle per source triangle.
    #[test]
    fn generate_lightmap_uv_unwraps_a_cube_into_charts_inside_the_unit_square() {
        let root = overlay_project("mesh-helper-uv", "cube.glb", &cube_glb([0.0; 3], false, false));
        let (model, _) = mesh_of(&root, "assets/cube.glb");
        let preview = lightmap_uv_preview(&model.parts).expect("unwrap");
        assert_eq!(preview.source_triangles, 12);
        assert_eq!(preview.triangles.len(), 12, "one triangle per source triangle");
        assert_eq!(preview.source_vertices, 24);
        assert!(
            preview.unwrapped_vertices >= 24,
            "seams split vertices: {preview:?}"
        );
        assert!(
            (1..=12).contains(&preview.report.charts),
            "a cube's faces make a few charts: {}",
            preview.report.charts
        );
        assert!(preview.report.chart_fill > 0.05 && preview.report.chart_fill <= 1.0);
        assert!(preview.rect[2] > 0 && preview.rect[3] > 0 && preview.texels_per_metre > 0.0);
        for uv in preview.triangles.iter().flatten().flatten() {
            assert!((-1e-4..=1.0001).contains(uv), "local UV2 {uv} is outside 0..1");
        }
        assert_eq!(
            preview,
            lightmap_uv_preview(&model.parts).expect("again"),
            "deterministic"
        );
        assert!(preview.summary_line().starts_with("Lightmap UV:"));
        // The drawn atlas differs from its backdrop inside the square and only there.
        let well = crate::theme::ThemeTokens::regia_default().well;
        let image = preview.image(128, well);
        let backdrop = egui::ColorImage::filled([128, 128], well);
        let drawn = changed(&backdrop, &image);
        assert!(
            drawn.len() > 100,
            "the atlas draws the charts: {} pixels",
            drawn.len()
        );
        assert_eq!(
            image.pixels[0], backdrop.pixels[0],
            "the corner outside the square is untouched"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The viewer's buttons: Tab reaches each, Enter presses it, and the request is what the shell
    /// runs; every control is named.
    #[test]
    fn the_mesh_helper_buttons_are_keyboard_operable_and_named() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let root = overlay_project("mesh-helper-keys", "cube.glb", &cube_glb([0.0; 3], false, false));
        let library = MeshLibrary::default();
        let state = std::cell::RefCell::new(AssetViewerState::default());
        state.borrow_mut().open(Some(&root), &library, "assets/cube.glb");
        let ctx = egui::Context::default();
        let size = [900.0, 700.0];
        let mut draw = |ui: &mut egui::Ui| {
            asset_viewer_panel(ui, &mut state.borrow_mut(), Some(&root), &library, None);
        };
        let first = frame(&ctx, size, vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&first), Vec::<String>::new());
        let names: Vec<String> = crate::a11y_audit::widget_names(&first)
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        for wanted in [
            "Add simple collision",
            "Generate lightmap UV",
            "Build nav preview here",
            "Collision shape",
            "Preview lighting",
            "Floor",
            "Rotate",
        ] {
            assert!(
                names.iter().any(|n| n.contains(wanted)),
                "no control named {wanted}: {names:?}"
            );
        }
        for (name, want) in [
            (
                "Add simple collision",
                MeshHelperRequest::AddCollision(CollisionShape::Convex),
            ),
            ("Generate lightmap UV", MeshHelperRequest::LightmapUv),
            ("Build nav preview here", MeshHelperRequest::NavPreview),
        ] {
            tab_to(&ctx, size, name, false, 40, &mut draw);
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                &mut draw,
            );
            frame(&ctx, size, vec![], &mut draw);
            assert_eq!(
                state.borrow_mut().helper_request.take(),
                Some(want),
                "Enter on {name}"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// The shell hand-off: a lightmap UV request (the viewer's own, or the Content Browser's queued
    /// one) fills the viewer's atlas; a collision request runs the write.
    #[test]
    fn run_helper_requests_unwraps_the_open_mesh_and_runs_the_collision_write() {
        let (mut world, root) = helper_world("mesh-helper-shell");
        let ctx = egui::Context::default();
        let mut state = AssetViewerState::default();
        state.open(Some(&root), &MeshLibrary::default(), "assets/cube.glb");
        state.helper_request = Some(MeshHelperRequest::LightmapUv);
        run_helper_requests(&ctx, &mut state, &mut world);
        let (preview, texture) = state.lightmap_atlas.as_ref().expect("atlas");
        assert_eq!(preview.source_triangles, 12);
        assert!(texture.is_some());
        assert!(
            state
                .helper_toast
                .as_deref()
                .is_some_and(|t| t.starts_with("Lightmap UV:"))
        );
        // Opening another mesh drops the atlas.
        state.open(Some(&root), &MeshLibrary::default(), "builtin:cube");
        assert!(state.lightmap_atlas.is_none());
        // The Content Browser's queued request runs once the viewer has the mesh open.
        state.open(Some(&root), &MeshLibrary::default(), "assets/cube.glb");
        world.insert_resource(MeshHelperQueue {
            lightmap_uv: vec!["assets/cube.glb".into(), "assets/other.glb".into()],
        });
        run_helper_requests(&ctx, &mut state, &mut world);
        assert!(state.lightmap_atlas.is_some());
        assert_eq!(
            world.resource::<MeshHelperQueue>().lightmap_uv,
            vec!["assets/other.glb".to_string()],
            "only the open mesh's request is taken"
        );
        // The collision button's request writes the `.meta` through an import-settings job.
        state.helper_request = Some(MeshHelperRequest::AddCollision(CollisionShape::Convex));
        run_helper_requests(&ctx, &mut state, &mut world);
        assert!(
            state
                .helper_toast
                .as_deref()
                .is_some_and(|t| t.contains("import-settings job")),
            "{:?}",
            state.helper_toast
        );
        crate::import_settings_panel::tick_apply_job(&mut world);
        std::fs::remove_dir_all(&root).ok();
    }

    /// `editor.mesh_helper` and `editor.preview_scene` through the agent dispatcher.
    #[test]
    fn mesh_helper_and_preview_scene_through_the_dispatcher() {
        let (_, root) = helper_world("mesh-helper-dispatch");
        let mut app = crate::engine::app::App::new();
        app.add_plugins(crate::engine::app::MinimalPlugins);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        crate::agent_ext::install(app.world_mut());
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        app.finish_plugins();
        let (sender, inbox) = crossbeam_channel::unbounded();
        app.insert_resource(regia_agent::AgentInbox(inbox));
        let mut call = |method: &str, params: serde_json::Value| {
            let (reply, answer) = crossbeam_channel::bounded(1);
            sender
                .send(regia_agent::server::Inbound {
                    request: regia_agent::protocol::Request {
                        jsonrpc: "2.0".into(),
                        id: Some(json!(1)),
                        method: method.into(),
                        params,
                    },
                    reply,
                })
                .expect("agent inbox");
            app.update();
            answer.try_recv().expect("dispatch answers in one frame")
        };
        // The unwrap: figures and a PNG, nothing written.
        let before = std::fs::read(root.join("assets/cube.glb.meta")).expect("meta");
        let uv = call(
            "editor.mesh_helper",
            json!({ "path": "assets/cube.glb", "action": "lightmap_uv", "size": 96 }),
        );
        assert!(uv.error.is_none(), "{:?}", uv.error);
        let uv = uv.result.expect("result");
        assert_eq!(uv["atlas"]["source_triangles"], 12);
        assert!(uv["atlas"]["charts"].as_u64().is_some_and(|c| c >= 1));
        assert_eq!(
            (uv["width"].as_u64(), uv["height"].as_u64()),
            (Some(96), Some(96))
        );
        assert!(uv["png_base64"].as_str().is_some_and(|p| p.len() > 100));
        assert_eq!(
            std::fs::read(root.join("assets/cube.glb.meta")).expect("meta"),
            before,
            "read only"
        );
        // The collision write, through the dispatcher.
        let coll = call(
            "editor.mesh_helper",
            json!({ "path": "assets/cube.glb", "action": "add_collision", "shape": "convex" }),
        );
        assert!(coll.error.is_none(), "{:?}", coll.error);
        let coll = coll.result.expect("result");
        assert_eq!(coll["shape"], "convex");
        assert!(
            std::fs::read_to_string(root.join("assets/cube.glb.meta"))
                .expect("meta")
                .contains("ConvexDecomposition")
        );
        // Refusals carry their codes.
        for (params, code) in [
            (
                json!({ "path": "assets/cube.glb", "action": "bake" }),
                "mesh_helper_unknown_action",
            ),
            (
                json!({ "path": "assets/cube.glb", "action": "add_collision", "shape": "torus" }),
                "mesh_helper_unknown_shape",
            ),
            (
                json!({ "path": "builtin:cube", "action": "add_collision" }),
                "mesh_helper_not_importable",
            ),
        ] {
            let refusal = call("editor.mesh_helper", params.clone());
            let error = refusal
                .error
                .unwrap_or_else(|| panic!("{params} should be refused"));
            assert_eq!(
                error.data.as_ref().and_then(|d| d["reason"].as_str()),
                Some(code),
                "{params}"
            );
        }
        // The nav preview of a level with no static collision is refused, not an empty success.
        let nav = call(
            "editor.mesh_helper",
            json!({ "path": "assets/cube.glb", "action": "nav_preview" }),
        );
        assert!(nav.error.is_some() || nav.result.is_some(), "answers either way");
        // The preview scene: read, change, and the refusal.
        let read = call("editor.preview_scene", json!({ "editor": "mesh" }))
            .result
            .expect("read");
        assert_eq!(read["preset"], "outdoor");
        let set = call(
            "editor.preview_scene",
            json!({ "editor": "mesh", "preset": "studio", "floor": true, "intensity": 0.5 }),
        )
        .result
        .expect("set");
        assert_eq!(
            (set["preset"].as_str(), set["floor"].as_bool()),
            (Some("studio"), Some(true))
        );
        assert!(
            call(
                "editor.preview_scene",
                json!({ "editor": "mesh", "preset": "moon" })
            )
            .error
            .is_some()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The texture viewer's lit preview: the texture is the albedo of a sphere or plane in the
    /// shared scene, rendered once per change. Skips without a GPU.
    #[test]
    fn the_texture_viewer_draws_the_texture_on_a_lit_shape_once_per_change() {
        use crate::preview_scene::TextureSurface;
        let device = match crate::render::headless_device() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("skip the_texture_viewer_draws_the_texture_on_a_lit_shape_once_per_change: {err}");
                return;
            }
        };
        let solid = |rgb: [u8; 3]| LabTexture {
            width: 8,
            height: 8,
            rgba: (0..64).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect(),
        };
        let scene = crate::preview_scene::PreviewContext::default();
        let center =
            |image: &egui::ColorImage| image.pixels[image.size[0] * (image.size[1] / 2) + image.size[0] / 2];
        let red = render_texture_preview(&device, &solid([230, 20, 20]), TextureSurface::Sphere, &scene, 96)
            .expect("red sphere");
        let green =
            render_texture_preview(&device, &solid([20, 230, 20]), TextureSurface::Sphere, &scene, 96)
                .expect("green sphere");
        let (r, g) = (center(&red), center(&green));
        assert!(r.r() > r.g() && r.r() > r.b(), "the red texture shows red: {r:?}");
        assert!(
            g.g() > g.r() && g.g() > g.b(),
            "the green texture shows green: {g:?}"
        );
        let plane = render_texture_preview(&device, &solid([230, 20, 20]), TextureSurface::Plane, &scene, 96)
            .expect("plane");
        assert_ne!(plane.pixels, red.pixels, "a plane is not a sphere");
        // The panel: flat draws no lit preview; a surface renders once and keeps its image.
        let mut state = AssetViewerState {
            path: Some("assets/t.png".into()),
            loaded: Loaded::Texture(mip_chain(&solid([230, 20, 20]))),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let draw = |state: &mut AssetViewerState| {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(700.0, 700.0),
                    )),
                    ..Default::default()
                },
                |ui| asset_viewer_panel(ui, state, None, &MeshLibrary::default(), Some(&device)),
            );
            out.textures_delta.clear();
        };
        draw(&mut state);
        assert!(state.texture_view.is_none(), "flat has no lit preview");
        state.texture_scene.settings.surface = TextureSurface::Sphere;
        draw(&mut state);
        let first = state
            .texture_view
            .as_ref()
            .map(|(k, h)| (*k, h.id()))
            .expect("rendered");
        for _ in 0..3 {
            draw(&mut state);
        }
        let again = state
            .texture_view
            .as_ref()
            .map(|(k, h)| (*k, h.id()))
            .expect("kept");
        assert_eq!(first, again, "an unchanged scene keeps its image");
        state.texture_scene.settings.floor = true;
        draw(&mut state);
        assert_ne!(
            state.texture_view.as_ref().map(|(k, _)| *k),
            Some(first.0),
            "a scene change renders again"
        );
        // Opening another file drops the image.
        state.open(None, &MeshLibrary::default(), "builtin:cube");
        assert!(state.texture_view.is_none());
    }

    /// The texture viewer's surface menu and, with a lit surface, the scene controls are named and
    /// reachable with Tab (no GPU needed: without a device the viewer says so).
    #[test]
    fn the_texture_viewer_controls_are_named_and_keyboard_reachable() {
        use crate::canvas_keys::test_input::{frame, tab_to};
        let state = std::cell::RefCell::new(AssetViewerState::default());
        {
            let mut s = state.borrow_mut();
            s.path = Some("assets/t.png".into());
            s.loaded = Loaded::Texture(mip_chain(&LabTexture {
                width: 4,
                height: 4,
                rgba: vec![200; 64],
            }));
        }
        let ctx = egui::Context::default();
        let size = [800.0, 700.0];
        let mut draw = |ui: &mut egui::Ui| {
            asset_viewer_panel(ui, &mut state.borrow_mut(), None, &MeshLibrary::default(), None);
        };
        let flat = frame(&ctx, size, vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&flat), Vec::<String>::new());
        let names = |update: &egui::accesskit::TreeUpdate| -> Vec<String> {
            crate::a11y_audit::widget_names(update)
                .into_iter()
                .map(|(_, n)| n)
                .collect()
        };
        assert!(
            names(&flat).iter().any(|n| n.contains("Preview surface")),
            "{:?}",
            names(&flat)
        );
        assert!(
            !names(&flat).iter().any(|n| n.contains("Preview lighting")),
            "flat has no scene controls"
        );
        tab_to(&ctx, size, "Preview surface", false, 30, &mut draw);
        state.borrow_mut().texture_scene.settings.surface = crate::preview_scene::TextureSurface::Sphere;
        let lit = frame(&ctx, size, vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&lit), Vec::<String>::new());
        for wanted in ["Preview lighting", "Environment intensity", "Floor", "Rotate"] {
            assert!(
                names(&lit).iter().any(|n| n.contains(wanted)),
                "no {wanted}: {:?}",
                names(&lit)
            );
            tab_to(&ctx, size, wanted, false, 30, &mut draw);
        }
    }

    /// Writes a contact sheet of the mesh and texture viewers in the preview scene to the folder
    /// named by `REGIA_PREVIEW_SCENE_PLATES` (a look review, not a check): `cargo test -p
    /// regia_editor --lib mesh_and_texture_scene_plates -- --ignored`.
    #[test]
    #[ignore = "writes plates for a look review"]
    fn mesh_and_texture_scene_plates() {
        use crate::preview_scene::{PreviewContext, PreviewLighting, TextureSurface};
        let Some(dir) = std::env::var_os("REGIA_PREVIEW_SCENE_PLATES") else {
            eprintln!("REGIA_PREVIEW_SCENE_PLATES is not set");
            return;
        };
        let device = crate::render::headless_device().expect("a headless device");
        let root = overlay_project("scene-plates", "cube.glb", &cube_glb([0.0; 3], false, false));
        let (model, _) = mesh_of(&root, "assets/cube.glb");
        let placed: Vec<(&ModelPart, Mat4)> = model.parts.iter().map(|p| (p, Mat4::IDENTITY)).collect();
        let checker = LabTexture {
            width: 8,
            height: 8,
            rgba: (0..64)
                .flat_map(|i| {
                    let on = (i % 8 + i / 8) % 2 == 0;
                    if on {
                        [240, 200, 40, 255]
                    } else {
                        [40, 60, 200, 255]
                    }
                })
                .collect(),
        };
        let mut cells = Vec::new();
        for (lighting, floor, rotate) in [
            (PreviewLighting::Outdoor, false, false),
            (PreviewLighting::Outdoor, true, true),
            (PreviewLighting::Studio, true, false),
            (PreviewLighting::Neutral, true, false),
        ] {
            let mut scene = PreviewContext::default();
            scene.settings.lighting = lighting;
            scene.settings.floor = floor;
            scene.settings.auto_rotate = rotate;
            scene.spin.advance_by(2.0);
            cells.push(
                render_placed_parts_scene(
                    &device,
                    &placed,
                    (model.aabb_min, model.aabb_max),
                    0.6,
                    0.35,
                    [200, 200],
                    &scene,
                )
                .expect("mesh"),
            );
            for surface in [TextureSurface::Sphere, TextureSurface::Plane] {
                if lighting == PreviewLighting::Studio || lighting == PreviewLighting::Outdoor && floor {
                    cells.push(
                        render_texture_preview(&device, &checker, surface, &scene, 200).expect("texture"),
                    );
                }
            }
        }
        let columns = 4;
        let rows = cells.len().div_ceil(columns);
        let (w, h) = (cells[0].size[0], cells[0].size[1]);
        let mut sheet = vec![0u8; columns * w * rows * h * 4];
        for (n, cell) in cells.iter().enumerate() {
            let (cx, cy) = ((n % columns) * w, (n / columns) * h);
            for y in 0..h.min(cell.size[1]) {
                for x in 0..w.min(cell.size[0]) {
                    let px = cell.pixels[y * cell.size[0] + x];
                    let at = ((cy + y) * columns * w + cx + x) * 4;
                    sheet[at..at + 4].copy_from_slice(&[px.r(), px.g(), px.b(), 255]);
                }
            }
        }
        let path = std::path::Path::new(&dir).join("viewer-scene-sheet.png");
        let file = std::fs::File::create(&path).expect("file");
        let mut encoder = png::Encoder::new(file, (columns * w) as u32, (rows * h) as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("header")
            .write_image_data(&sheet)
            .expect("png");
        eprintln!("wrote {}", path.display());
        std::fs::remove_dir_all(&root).ok();
    }
}

// ---------------------------------------------------------------------------------------------
// Animation previews (spec addendum § 8.8, ADR-7788): a looping posed skeleton in the browser tile
// (on hover or keyboard focus) and in the asset viewer
// ---------------------------------------------------------------------------------------------

/// A skeleton and its clips, loaded for a preview (a `.bvh` motion, or a skinned glTF with clips).
#[derive(Debug, Clone)]
pub struct AnimPreview {
    /// The skeleton.
    pub skeleton: Arc<regia_animation::skeleton::Skeleton>,
    /// The clips, in file order.
    pub clips: Vec<Arc<regia_animation::clip::AnimationClip>>,
}

impl AnimPreview {
    /// Object-space joint matrices of clip `clip` at `time` seconds, looping (the bind pose when there
    /// is no such clip).
    #[must_use]
    pub fn globals(&self, clip: usize, time: f32) -> Vec<Mat4> {
        let mut pose = self.skeleton.bind_pose_pose();
        if let Some(clip) = self.clips.get(clip) {
            let duration = clip.duration.max(1.0e-3);
            let t = if time.is_finite() {
                time.rem_euclid(duration)
            } else {
                0.0
            };
            clip.sample_pose(t, &mut pose);
        }
        self.skeleton.globals(&pose, None)
    }
}

/// Whether `path` names an animation file by extension (`.bvh`); a skinned glTF with clips is a mesh by
/// extension and previews its clips too ([`load_anim_preview`]).
#[must_use]
pub fn is_animation_path(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".bvh")
}

/// Loads the skeleton and clips of a `.bvh` or a skinned glTF.
///
/// # Errors
///
/// The file cannot be read, does not parse, or has no skin or no clip.
pub fn load_anim_preview(root: Option<&Path>, path: &str) -> Result<AnimPreview, String> {
    let full = resolve_asset_path(root, path);
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".bvh") {
        let bytes = std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
        let name = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("clip");
        let settings = regia_asset_pipeline::bvh_import::BvhImportSettings::default();
        let imported = regia_asset_pipeline::bvh_import::convert_bvh(&bytes, name, &settings)
            .map_err(|e| format!("{path}: {e}"))?;
        return Ok(AnimPreview {
            skeleton: Arc::new(imported.skeleton),
            clips: vec![Arc::new(imported.clip)],
        });
    }
    if lower.ends_with(".glb") || lower.ends_with(".gltf") {
        let rig = crate::anim::PreviewRig::from_gltf(&full)?;
        if rig.clips.is_empty() {
            return Err(format!("{path} has a skin but no animation clip"));
        }
        return Ok(AnimPreview {
            skeleton: rig.skeleton,
            clips: rig.clips,
        });
    }
    Err(format!(
        "{path}: not an animation (.bvh, or a skinned .glb/.gltf with clips)"
    ))
}

/// Joint positions projected into `size` (pixels) from a fixed three-quarter view that frames the rig.
#[must_use]
pub fn project_joints(globals: &[Mat4], size: [f32; 2]) -> Vec<[f32; 2]> {
    if globals.is_empty() {
        return Vec::new();
    }
    let (min, max) = globals
        .iter()
        .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), m| {
            let p = m.w_axis.truncate();
            (lo.min(p), hi.max(p))
        });
    let camera = OrbitCamera::framing((min, max), 0.7, 0.3);
    globals
        .iter()
        .map(|m| camera.project(m.w_axis.truncate(), size).unwrap_or([-1.0, -1.0]))
        .collect()
}

/// The bone segments of a pose as pixel pairs (parent joint to child joint) inside `size`.
#[must_use]
pub fn bone_segments(
    skeleton: &regia_animation::skeleton::Skeleton,
    globals: &[Mat4],
    size: [f32; 2],
) -> Vec<[[f32; 2]; 2]> {
    let points = project_joints(globals, size);
    skeleton
        .parents
        .iter()
        .enumerate()
        .filter_map(|(child, parent)| {
            let parent = (*parent)? as usize;
            Some([*points.get(parent)?, *points.get(child)?])
        })
        .collect()
}

/// A preview frame as an image (the agent's `editor.view_asset` and tests): bones in `bone`, on `background`.
#[must_use]
pub fn anim_frame_image(
    preview: &AnimPreview,
    clip: usize,
    time: f32,
    size: [usize; 2],
    (bone, background): (egui::Color32, egui::Color32),
) -> egui::ColorImage {
    let mut image = egui::ColorImage::new(size, vec![background; size[0] * size[1]]);
    let globals = preview.globals(clip, time);
    let clip_rect = (0, 0, size[0] as i64, size[1] as i64);
    for [a, b] in bone_segments(&preview.skeleton, &globals, [size[0] as f32, size[1] as f32]) {
        draw_line(&mut image, a, b, bone, clip_rect);
    }
    image
}

/// Paints the preview into `rect` with the theme's colors (selection stroke for bones).
pub fn paint_anim_preview(
    painter: &egui::Painter,
    rect: egui::Rect,
    preview: &AnimPreview,
    clip: usize,
    time: f32,
) {
    let visuals = &painter.ctx().global_style().visuals;
    let stroke = egui::Stroke::new(2.0, visuals.selection.stroke.color);
    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
    let globals = preview.globals(clip, time);
    for [a, b] in bone_segments(&preview.skeleton, &globals, [rect.width(), rect.height()]) {
        painter.line_segment(
            [
                rect.min + egui::vec2(a[0], a[1]),
                rect.min + egui::vec2(b[0], b[1]),
            ],
            stroke,
        );
    }
}

/// Previews kept in memory.
pub const ANIM_PREVIEW_CAPACITY: usize = 16;

/// The browser's loaded previews (loads at most one file per frame; failures are remembered).
#[derive(regia_ecs::prelude::Resource, Debug, Default)]
pub struct AnimPreviewCache {
    entries: std::collections::VecDeque<(String, Result<Arc<AnimPreview>, String>)>,
    loaded_frame: Option<u64>,
    /// Files loaded so far (a cost counter for tests).
    pub loads: u64,
}

impl AnimPreviewCache {
    /// The preview of `rel`, loading it when this frame has not loaded another file yet.
    pub fn get(&mut self, root: Option<&Path>, rel: &str, frame: u64) -> Option<Arc<AnimPreview>> {
        if let Some(at) = self.entries.iter().position(|(path, _)| path == rel) {
            return self
                .entries
                .get(at)
                .and_then(|(_, loaded)| loaded.as_ref().ok().cloned());
        }
        if self.loaded_frame == Some(frame) {
            return None;
        }
        self.loaded_frame = Some(frame);
        self.loads += 1;
        // The first hover's load (a BVH convert or a glTF rig) runs on the UI thread: timed.
        let loaded = {
            let _span = tracing::info_span!(target: "regia_editor", "anim_preview_load").entered();
            load_anim_preview(root, rel).map(Arc::new)
        };
        if let Err(err) = &loaded {
            tracing::debug!(target: "regia_editor", "animation preview {rel}: {err}");
        }
        self.entries.push_back((rel.to_string(), loaded.clone()));
        while self.entries.len() > ANIM_PREVIEW_CAPACITY {
            self.entries.pop_front();
        }
        loaded.ok()
    }
}

/// Whether a browser tile may show an animation preview: a `.bvh`, or a glTF (which previews only when it
/// loads with a skin and clips).
#[must_use]
pub fn tile_may_animate(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    is_animation_path(rel) || lower.ends_with(".glb") || lower.ends_with(".gltf")
}

/// The browser tile hook: while the tile is hovered or keyboard-focused, loops its first clip over the
/// picture. Bounded per frame: one tile, at most one file load ([`AnimPreviewCache`]).
pub fn paint_tile_animation(
    ui: &egui::Ui,
    world: &mut regia_ecs::prelude::World,
    root: Option<&Path>,
    rel: &str,
    picture: egui::Rect,
) {
    if !tile_may_animate(rel) {
        return;
    }
    let frame = ui.ctx().cumulative_frame_nr();
    let preview = world
        .get_resource_or_insert_with(AnimPreviewCache::default)
        .get(root, rel, frame);
    if let Some(preview) = preview {
        let _span = tracing::info_span!(target: "regia_editor", "anim_tile_preview").entered();
        let time = ui.input(|i| i.time) as f32;
        paint_anim_preview(ui.painter(), picture.shrink(2.0), &preview, 0, time);
        ui.ctx().request_repaint();
    }
}

/// The asset viewer's animation page: clip picker, play/pause (Space when the viewer has focus), and the
/// looping preview.
pub fn anim_viewer_ui(ui: &mut egui::Ui, preview: &AnimPreview, state: &mut AnimViewerState, side: f32) {
    ui.label(format!(
        "{} joints · {} clip(s)",
        preview.skeleton.parents.len(),
        preview.clips.len()
    ));
    ui.horizontal(|ui| {
        let current = preview
            .clips
            .get(state.clip)
            .map_or_else(|| "clip".to_string(), |clip| clip.name.clone());
        egui::ComboBox::from_id_salt("asset-viewer-clip")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for (index, clip) in preview.clips.iter().enumerate() {
                    ui.selectable_value(
                        &mut state.clip,
                        index,
                        format!("{} ({:.2} s)", clip.name, clip.duration),
                    );
                }
            });
        let label = if state.paused { "Play" } else { "Pause" };
        if ui
            .button(label)
            .on_hover_text("Play or pause the preview (Space)")
            .clicked()
        {
            state.toggle_pause(ui.ctx().cumulative_frame_nr());
        }
    });
    if ui.input(|i| i.key_pressed(egui::Key::Space)) && ui.memory(|m| m.focused().is_none()) {
        state.toggle_pause(ui.ctx().cumulative_frame_nr());
    }
    let now = ui.input(|i| i.time) as f32;
    if !state.paused {
        state.time += (now - state.last).clamp(0.0, 0.25);
        ui.ctx().request_repaint();
    }
    state.last = now;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
    paint_anim_preview(ui.painter(), rect, preview, state.clip, state.time);
}

/// The asset viewer's animation playback state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnimViewerState {
    /// The clip shown.
    pub clip: usize,
    /// Seconds into the clip (wraps).
    pub time: f32,
    /// Held.
    pub paused: bool,
    last: f32,
    /// The egui frame whose Play/Pause (button or Space) already acted.
    toggled_frame: Option<u64>,
}

impl AnimViewerState {
    /// Play/Pause in egui frame `frame`: applied once per frame, so egui's second UI pass (which
    /// sees the same click or key) does not undo it (review sprint-w12-rev-g5 #10). Returns
    /// whether it applied.
    pub fn toggle_pause(&mut self, frame: u64) -> bool {
        if self.toggled_frame == Some(frame) {
            return false;
        }
        self.toggled_frame = Some(frame);
        self.paused = !self.paused;
        true
    }
}

#[cfg(test)]
mod anim_tests {
    use super::*;

    fn project_with_bvh(tag: &str) -> std::path::PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets/anim")).expect("dir");
        std::fs::write(
            root.join("assets/anim/wave.bvh"),
            regia_asset_pipeline::bvh_import::FIXTURE_BVH,
        )
        .expect("bvh");
        root
    }

    #[test]
    fn kind_of_names_animations_and_the_viewer_loads_them() {
        assert_eq!(kind_of("assets/anim/Wave.BVH"), Some(AssetKind::Animation));
        assert_eq!(AssetKind::Animation.name(), "animation");
        assert_eq!(
            kind_of("assets/m.glb"),
            Some(AssetKind::Mesh),
            "a glTF stays a mesh"
        );
        let root = project_with_bvh("anim_viewer_load");
        let library = MeshLibrary::default();
        match load_asset(Some(&root), &library, "assets/anim/wave.bvh") {
            Ok(Loaded::Animation(preview)) => {
                assert_eq!(preview.clips.len(), 1);
                assert!(preview.skeleton.parents.len() >= 2);
            }
            Ok(_) => panic!("not an animation"),
            Err(err) => panic!("{err}"),
        }
    }

    #[test]
    fn the_preview_loops_and_moves() {
        let root = project_with_bvh("anim_loop");
        let preview = load_anim_preview(Some(&root), "assets/anim/wave.bvh").expect("load");
        let duration = preview.clips[0].duration;
        assert!(duration > 0.0);
        let at = |t: f32| preview.globals(0, t);
        let close = |a: &[Mat4], b: &[Mat4]| {
            a.iter()
                .zip(b)
                .all(|(x, y)| (x.w_axis - y.w_axis).length() < 1.0e-3)
        };
        assert!(
            close(&at(0.25 * duration), &at(1.25 * duration)),
            "one clip length later is the same pose"
        );
        assert!(
            !close(&at(0.0), &at(0.99 * duration)),
            "the pose moves within the clip"
        );
        let image = anim_frame_image(
            &preview,
            0,
            0.5 * duration,
            [64, 64],
            (egui::Color32::WHITE, egui::Color32::BLACK),
        );
        let lit = image
            .pixels
            .iter()
            .filter(|p| **p == egui::Color32::WHITE)
            .count();
        assert!(lit > 4, "bones are drawn: {lit} pixels");
    }

    #[test]
    fn the_tile_cache_loads_one_file_per_frame_and_remembers_failures() {
        let root = project_with_bvh("anim_cache");
        std::fs::write(root.join("assets/anim/bad.bvh"), b"HIERARCHY nonsense").expect("bad");
        let mut cache = AnimPreviewCache::default();
        assert!(cache.get(Some(&root), "assets/anim/wave.bvh", 1).is_some());
        assert!(
            cache.get(Some(&root), "assets/anim/bad.bvh", 1).is_none(),
            "second load waits a frame"
        );
        assert_eq!(cache.loads, 1);
        assert!(cache.get(Some(&root), "assets/anim/bad.bvh", 2).is_none());
        assert!(cache.get(Some(&root), "assets/anim/bad.bvh", 3).is_none());
        assert_eq!(cache.loads, 2, "a failure is not retried every frame");
        assert!(
            cache.get(Some(&root), "assets/anim/wave.bvh", 3).is_some(),
            "from memory"
        );
        assert_eq!(cache.loads, 2);
    }

    #[test]
    fn the_viewer_pauses_from_the_keyboard() {
        let root = project_with_bvh("anim_keys");
        let preview = load_anim_preview(Some(&root), "assets/anim/wave.bvh").expect("load");
        let ctx = egui::Context::default();
        let mut state = AnimViewerState::default();
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Space,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let mut output = ctx.run_ui(input, |ui| anim_viewer_ui(ui, &preview, &mut state, 96.0));
        output.textures_delta.clear();
        assert!(state.paused, "Space pauses");
    }

    /// Review sprint-w12-rev-g5 #10: egui's second UI pass of a frame sees the same Play click or
    /// Space; the toggle applies once, so the click is not undone.
    #[test]
    fn play_pause_applies_once_per_frame() {
        let mut state = AnimViewerState::default();
        assert!(state.toggle_pause(3));
        assert!(!state.toggle_pause(3), "the second pass of frame 3");
        assert!(state.paused, "the click sticks");
        assert!(state.toggle_pause(4));
        assert!(!state.paused);
    }
}
