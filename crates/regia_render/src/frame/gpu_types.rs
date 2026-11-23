// SPDX-License-Identifier: MPL-2.0
//! GPU-side layouts of the frame uniform, lights, objects, materials and shadow tiles, and
//! their packing (moved out of `frame.rs` unchanged, ADR-1552).

use super::*;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct LabFrame {
    pub(super) view_proj: [[f32; 4]; 4],
    pub(super) view: [[f32; 4]; 4],
    /// `xyz` eye. `w` is 1 when the frame is an interior: neutral ambient floor,
    /// and probe bounce is added instead of replacing the sky.
    pub(super) camera_pos: [f32; 4],
    pub(super) params: [f32; 4],
    pub(super) screen: [f32; 4],
    pub(super) extra: [f32; 4],
    /// `x` = post enabled, `y` = EV100 exposure multiplier (`1 / (1.2 · 2^EV100)`),
    /// `z` = procedural-sky radiance scale, `w` = flat ambient lux.
    pub(super) post_exposure: [f32; 4],
    /// Fog params: density, height_falloff, base_height, enabled.
    pub(super) fog_params: [f32; 4],
    /// Fog colour rgb (a unused).
    pub(super) fog_color: [f32; 4],
    /// `x` = shadow filter mode, `y` = GI (2 when probes are bound), `z` =
    /// reflection-probe budget, `w` = temporal noise index (0 without temporal AA).
    pub(super) quality: [f32; 4],
    /// Previous frame's un-jittered view-projection (motion vectors).
    pub(super) prev_view_proj: [[f32; 4]; 4],
    /// This frame's view-projection without the TAAU jitter.
    pub(super) unjittered_view_proj: [[f32; 4]; 4],
    /// `x` = PCSS blocker-search taps, `y` = PCSS filter taps (`0` keeps 12/16).
    /// `z` = 1 when the profile allows parallax occlusion. `w` = local-shadow
    /// debug (0 off, 1 shadow factor, 2 atlas tile). `quality.w` stays the
    /// temporal noise index.
    pub(super) pcss: [f32; 4],
    /// Interior volume that contains the eye: centre (`w` = 1 when set). Pixels
    /// inside it take the interior shade; the rest of the view keeps the sky.
    pub(super) interior_center: [f32; 4],
    /// Half extents of [`Self::interior_center`]'s box; `w` = 1 when masked materials use
    /// the hashed alpha test (TSR frames).
    pub(super) interior_half: [f32; 4],
    /// Planet-shine fill: `xyz` toward the sky body, `w` = 1 when on.
    pub(super) fill_dir: [f32; 4],
    /// Planet-shine colour · lux; `w` = material texture mip bias (negative under TSR
    /// upscaling, 0 otherwise).
    pub(super) fill_color: [f32; 4],
    /// Specular AA: `x` = screen-space variance (0 off), `y` = kernel threshold, `z` = Toksvig
    /// quantisation floor ([`FrameQuality::specular_aa`]). `w` = 1 when the IBL is a reference
    /// studio's environment ([`FrameRenderer::set_studio_sky`]), which the sky draw then shows
    /// as the background.
    pub(super) specular_aa: [f32; 4],
    /// Cloud transmittance map (ADR-320): extent, layer base and top, `w` = 1 when it shadows
    /// the sun term. Zero leaves the forward pass as before.
    pub(super) cloud_sun: [f32; 4],
    /// The image light's ground plane ([`crate::EnvGroundPlane`]): height, radius (0 = unbounded),
    /// centre x, z.
    pub(super) env_ground: [f32; 4],
    /// Its radiance (cd/m²); `w` = 1 when the plane is on.
    pub(super) env_ground_light: [f32; 4],
    /// `x` = image-light debug view (0 lit, 1 image light only, 2 occlusion terms), `y` = sky
    /// draw's background (0 the environment, 1 solid, 2 gradient, 3 image), `z` = local captures,
    /// `w` = their highest mip.
    pub(super) env_view: [f32; 4],
    /// Background parameters (`lab_forward.wgsl`).
    pub(super) background: [[f32; 4]; 3],
    /// Local captures: box centre, `w` = blend weight.
    pub(super) capture_center: [[f32; 4]; 4],
    /// Local captures: half extents, `w` = fade distance.
    pub(super) capture_half: [[f32; 4]; 4],
    /// Surface weather (spec 99): wetness, puddles, snow cover, ripple. All zero skips it.
    pub(super) weather: [f32; 4],
    /// The weather clock, the occlusion tile's index (-1 none), the occlusion extent, and the
    /// precipitation's extinction (1/m).
    pub(super) weather2: [f32; 4],
    /// Graph clock when bit 7 of [`Self::interior_half`] `.w` is set: time, previous time,
    /// wind x, wind z. Time-reading graphs read this instead of parameter slot 0 (ADR-5160).
    pub(super) clock: [f32; 4],
    /// Lights-and-views block (ADR-7211, [`crate::LightsViewsUniform`]): `x` = the forward
    /// shader's debug view, `y` = extra directional lights, `z` = ambient volumes.
    pub(super) view_mode: [f32; 4],
    /// Extra directional lights: travel direction (`w` = 1), color × lux.
    pub(super) extra_dir: [[f32; 4]; 2 * crate::MAX_EXTRA_DIRECTIONAL],
    /// Ambient volumes: centre (`w` = blend distance), half extents, irradiance.
    pub(super) ambient_box: [[f32; 4]; 3 * crate::MAX_AMBIENT_VOLUMES],
    /// Ambient volumes: rotation from the world into each box's frame (quaternion `xyzw`).
    pub(super) ambient_rot: [[f32; 4]; crate::MAX_AMBIENT_VOLUMES],
}

/// `(time, previous time, wind x, wind z)` for [`LabFrame::clock`] and the shadow and velocity
/// uniforms.
pub(super) fn graph_clock_row(time: (f32, f32), wind: [f32; 2]) -> [f32; 4] {
    [time.0, time.1, wind[0], wind[1]]
}

/// Bytes of [`LabFrame`] the shared draw's light code reads (`shared_local_lights.wgsl`'s
/// `EllFrame`: every field before `interior_center`).
pub(crate) const LAB_FRAME_LIGHTS_PREFIX: usize = std::mem::offset_of!(LabFrame, interior_center);

/// `(field, offset)` of [`LabFrame`]'s first [`LAB_FRAME_LIGHTS_PREFIX`] bytes, for the test that
/// keeps `shared_local_lights.wgsl`'s `EllFrame` equal to it.
#[cfg(test)]
pub(crate) fn lab_frame_prefix_offsets() -> Vec<(String, u32)> {
    use std::mem::offset_of;
    [
        ("view_proj", offset_of!(LabFrame, view_proj)),
        ("view", offset_of!(LabFrame, view)),
        ("camera_pos", offset_of!(LabFrame, camera_pos)),
        ("params", offset_of!(LabFrame, params)),
        ("screen", offset_of!(LabFrame, screen)),
        ("extra", offset_of!(LabFrame, extra)),
        ("post_exposure", offset_of!(LabFrame, post_exposure)),
        ("fog_params", offset_of!(LabFrame, fog_params)),
        ("fog_color", offset_of!(LabFrame, fog_color)),
        ("quality", offset_of!(LabFrame, quality)),
        ("prev_view_proj", offset_of!(LabFrame, prev_view_proj)),
        ("unjittered_view_proj", offset_of!(LabFrame, unjittered_view_proj)),
        ("pcss", offset_of!(LabFrame, pcss)),
    ]
    .into_iter()
    .map(|(name, offset)| (name.to_owned(), offset as u32))
    .collect()
}

/// Size of one [`GpuLight`] record.
#[cfg(test)]
pub(crate) const GPU_LIGHT_BYTES: usize = size_of::<GpuLight>();

/// Size of one [`ShadowTileGpu`] record.
#[cfg(test)]
pub(crate) const SHADOW_TILE_BYTES: usize = size_of::<ShadowTileGpu>();

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct GpuLight {
    pub(super) position_kind: [f32; 4],
    pub(super) color_intensity: [f32; 4],
    pub(super) direction_radius: [f32; 4],
    pub(super) cone_shadow: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct ObjectData {
    pub(super) model: [[f32; 4]; 4],
    /// `x` is the first joint of this mesh in the shared palette.
    /// `y` is [`crate::FrameMesh::lod_dither`] as `f32` bits (`0` = drawn whole).
    /// `z` and `w` are the render-override lanes.
    pub(super) skin: [u32; 4],
}

// `lab_shadow.wgsl` and `velocity.wgsl` declare this block as an opaque `vec4` array before
// a material graph's parameters (`surface_graph::GRAPH_STANDARD_VEC4S`).
const _: () = assert!(size_of::<GpuMaterial>() == crate::surface_graph::GRAPH_STANDARD_VEC4S * 16);

/// Bytes of a standard material's arena record on the texture table path (ADR-863): the
/// constants, then the table indices (`LabMaterial::texture_indices` in the shaders).
pub(super) const TABLE_RECORD_BYTES: u64 =
    size_of::<GpuMaterial>() as u64 + crate::texture_table::TABLE_INDEX_BYTES;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct GpuMaterial {
    pub(super) base_color: [f32; 4],
    pub(super) factors: [f32; 4],
    pub(super) emissive: [f32; 4],
    pub(super) alpha: [f32; 4],
    /// Detail albedo, normal, and roughness strengths.
    pub(super) detail: [f32; 4],
    /// Detail UV scale, fade start, fade end.
    pub(super) detail_fade: [f32; 4],
    /// Clearcoat, clearcoat roughness, anisotropy, rotation.
    pub(super) coat: [f32; 4],
    /// Specular occlusion, multi-scatter, parallax scale, parallax max steps.
    pub(super) response: [f32; 4],
    /// Wear intensity, roughness bias, metallic bias, source.
    pub(super) wear: [f32; 4],
    /// Wear tint and contrast.
    pub(super) wear_tint: [f32; 4],
    /// Parallax min steps, wear bias, transmission, dielectric F0.
    pub(super) pom: [f32; 4],
    /// Volume transmittance at normal incidence (rgb, `KHR_materials_volume`), w = volume
    /// thickness (the screen-space refraction offset).
    pub(super) transmit: [f32; 4],
    /// Shading model block (`crate::shading_model`): zeros are the standard model.
    pub(super) shading: [[f32; 4]; crate::shading_model::SHADING_VEC4S],
    /// Hair cards (`crate::hair`): shifts and exponents; strengths, transmission and strategy;
    /// tip color and alpha; ramp range, root alpha and shift jitter; root color and streak
    /// density. All zero unless the material enables hair.
    pub(super) hair: [[f32; 4]; 5],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct ShadowTileGpu {
    pub(super) view_proj: [[f32; 4]; 4],
    pub(super) uv: [f32; 4],
    /// Receiver-side bias (see the WGSL `ShadowTile::bias`): `x` = texel size
    /// (for perspective tiles, per unit view depth), `y` = depth-extent term
    /// (`1/near − 1/far` perspective, `1/extent` orthographic), `z` = 1.0 when
    /// the tile is perspective, `w` = source radius in metres (PCSS penumbra).
    pub(super) bias: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct ShadowVp {
    pub(super) light_view_proj: [[f32; 4]; 4],
    /// Time, previous time, wind x, wind z. Read when [`Self::clock_live`] `.x` is above 0.5.
    pub(super) clock: [f32; 4],
    /// `x` is 1 when a graph reads [`Self::clock`] instead of its material slot 0.
    pub(super) clock_live: [f32; 4],
}

pub(super) fn pack_light(light: &crate::LabLocalLight, shadow_tile: i32) -> GpuLight {
    // Point lights carry luminous power in lumens; the shader's inverse-square term expects
    // candela. KHR_lights_punctual § 5.3: luminous intensity (cd) = luminous power (lm) / 4π.
    let lumens_to_candela: f32 = 1.0 / (4.0 * std::f32::consts::PI);
    match *light {
        crate::LabLocalLight::Point(p) => GpuLight {
            position_kind: [p.position[0], p.position[1], p.position[2], 1.0],
            color_intensity: [
                p.color[0],
                p.color[1],
                p.color[2],
                p.intensity * lumens_to_candela,
            ],
            direction_radius: [0.0, -1.0, 0.0, p.radius],
            // w: the sphere source of the specular (`LightSourceSize`); 0 = punctual.
            cone_shadow: [
                1.0,
                0.0,
                shadow_tile as f32,
                crate::light_source::specular_source_radius(p.source_radius),
            ],
        },
        crate::LabLocalLight::Spot(s) => {
            // A shadowed spot is lit only inside the angle its shadow tile covers; outside the
            // tile the shadow lookup reads "lit", so a wider cone would light through geometry.
            let (inner, outer) = if shadow_tile >= 0 {
                let outer = s.outer_angle.min(super::shadow_plan::SPOT_SHADOW_MAX_HALF_ANGLE);
                (s.inner_angle.min(outer), outer)
            } else {
                (s.inner_angle, s.outer_angle)
            };
            GpuLight {
                position_kind: [s.position[0], s.position[1], s.position[2], 2.0],
                color_intensity: [s.color[0], s.color[1], s.color[2], s.intensity],
                direction_radius: [s.direction[0], s.direction[1], s.direction[2], s.range],
                cone_shadow: [
                    inner.cos(),
                    outer.cos(),
                    shadow_tile as f32,
                    crate::light_source::specular_source_radius(s.source_radius),
                ],
            }
        }
    }
}

pub(super) fn pack_material(mat: &LabMaterial) -> GpuMaterial {
    GpuMaterial {
        base_color: mat.base_color,
        factors: [
            mat.metallic,
            mat.roughness,
            mat.normal_scale,
            mat.occlusion_strength,
        ],
        emissive: [
            mat.emissive[0],
            mat.emissive[1],
            mat.emissive[2],
            mat.emissive_intensity,
        ],
        alpha: [
            crate::hair::effective_alpha_mode(mat) as f32,
            mat.alpha_cutoff,
            if mat.unlit { 1.0 } else { 0.0 },
            // Bit 0: packed ORM. Bit 1: per-vertex occlusion (`crate::vertex_ao`). Above them the
            // weather response, 0 by default (`WeatherResponse::packed_bits`).
            (if mat.orm_packed
                && (mat.occlusion_map.is_some()
                    || mat.compressed.as_ref().is_some_and(|c| c.occlusion.is_some()))
            {
                1.0
            } else {
                0.0
            }) + crate::vertex_ao::flag_bit(mat)
                + mat.weather.packed_bits() as f32,
        ],
        detail: [
            mat.hard_surface.detail_albedo_strength,
            mat.hard_surface.detail_normal_strength,
            mat.hard_surface.detail_roughness_strength,
            0.0,
        ],
        detail_fade: [
            mat.hard_surface.detail_uv_scale[0],
            mat.hard_surface.detail_uv_scale[1],
            mat.hard_surface.detail_fade_start,
            mat.hard_surface.detail_fade_end,
        ],
        coat: [
            mat.hard_surface.clearcoat,
            mat.hard_surface.clearcoat_roughness,
            mat.hard_surface.anisotropy,
            mat.hard_surface.anisotropy_rotation,
        ],
        response: [
            mat.hard_surface.specular_occlusion,
            mat.hard_surface.multi_scatter,
            mat.hard_surface.pom_scale,
            mat.hard_surface.pom_max_steps,
        ],
        wear: [
            mat.hard_surface.edge_intensity,
            mat.hard_surface.edge_roughness_bias,
            mat.hard_surface.edge_metallic_bias,
            mat.hard_surface.edge_source,
        ],
        wear_tint: [
            mat.hard_surface.edge_tint[0],
            mat.hard_surface.edge_tint[1],
            mat.hard_surface.edge_tint[2],
            mat.hard_surface.edge_contrast,
        ],
        pom: [
            mat.hard_surface.pom_min_steps,
            mat.hard_surface.edge_bias,
            mat.transmission.clamp(0.0, 1.0),
            dielectric_f0(mat.ior),
        ],
        transmit: [
            mat.transmission_tint[0].clamp(0.0, 1.0),
            mat.transmission_tint[1].clamp(0.0, 1.0),
            mat.transmission_tint[2].clamp(0.0, 1.0),
            mat.transmission_thickness.max(0.0),
        ],
        shading: [[0.0; 4]; crate::shading_model::SHADING_VEC4S],
        // Rough transmission (ADR-2075) rides in `hair_a.x` of a material without hair.
        hair: crate::rough_transmission::pack_into_hair(mat, crate::hair::pack_hair(&mat.hair)),
    }
}

/// Normal-incidence reflectance of a dielectric of index `ior` against air,
/// `((ior − 1) / (ior + 1))²` (Fresnel). A missing or unphysical IOR falls back to 1.5
/// (0.04, the glTF default).
pub(crate) fn dielectric_f0(ior: f32) -> f32 {
    let ior = if ior.is_finite() && ior >= 1.0 { ior } else { 1.5 };
    let r = (ior - 1.0) / (ior + 1.0);
    r * r
}

#[cfg(test)]
mod masked_raster_words {
    use super::GpuMaterial;
    use std::mem::offset_of;

    /// The alpha-tested cluster raster reads material arena records by word (ADR-3915): a layout
    /// change here must fail until `visbuffer/vis_raster_masked.wgsl` follows it.
    #[test]
    fn the_masked_raster_reads_the_alpha_words_of_the_record() {
        let shader = include_str!("../visbuffer/vis_raster_masked.wgsl");
        let word = |array_offset: usize, index: usize| (array_offset + index * 4) / 4;
        for (name, at) in [
            ("WORD_BASE_ALPHA", word(offset_of!(GpuMaterial, base_color), 3)),
            ("WORD_ALPHA_CUTOFF", word(offset_of!(GpuMaterial, alpha), 1)),
            ("WORD_ALPHA_FLAGS", word(offset_of!(GpuMaterial, alpha), 3)),
            ("WORD_MIN_LOD", word(offset_of!(GpuMaterial, detail), 3)),
        ] {
            let line = format!("const {name}: u32 = {at}u;");
            assert!(shader.contains(&line), "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spot(outer: f32) -> crate::LabLocalLight {
        crate::LabLocalLight::Spot(crate::LabSpotLight {
            position: [0.0; 3],
            direction: [0.0, 0.0, -1.0],
            color: [1.0; 3],
            intensity: 1.0,
            range: 10.0,
            inner_angle: outer * 0.5,
            outer_angle: outer,
            cast_shadows: true,
            source_radius: None,
        })
    }

    #[test]
    fn a_shadowed_wide_spot_is_lit_only_where_its_tile_reaches() {
        let wide = std::f32::consts::FRAC_PI_2;
        let shadowed = pack_light(&spot(wide), 0);
        assert!(
            (shadowed.cone_shadow[1] - super::shadow_plan::SPOT_SHADOW_MAX_HALF_ANGLE.cos()).abs() < 1e-6
        );
        // The unshadowed light keeps the authored cone.
        let free = pack_light(&spot(wide), -1);
        assert!((free.cone_shadow[1] - wide.cos()).abs() < 1e-6);
        // A narrow shadowed spot is untouched.
        let narrow = pack_light(&spot(0.5), 0);
        assert!((narrow.cone_shadow[1] - 0.5f32.cos()).abs() < 1e-6);
    }
}
