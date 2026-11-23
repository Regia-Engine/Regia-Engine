// SPDX-License-Identifier: MPL-2.0
//! Clustered point and spot lights for a shared draw (the terrain).
//!
//! The forward pass lights meshes from the froxel cluster lists (`lab_forward.wgsl`). A shared
//! draw ([`crate::SharedPassDraw`]) records into the same colour pass but has its own shader,
//! so it gets the frame's light buffers through [`crate::SharedPassDraw::set_engine_lights`]
//! and the lighting code through [`shared_local_lights_wgsl`]: the forward pass's froxel
//! lookup, windowed falloff, spot cone, shadow-atlas lookup and isotropic BRDF, composed from
//! the same engine modules (`regia::clustering`, `regia::pbr::*`, `regia::shadows`) and
//! written out as plain WGSL a host can paste into its own source.

use crate::RenderError;

/// Bindings [`shared_local_lights_wgsl`] declares, from its first binding on: the frame
/// uniform, the lights, the cluster counts and indices, the shadow tiles, the shadow atlas and
/// a comparison sampler ([`shared_lights_layout_entries`]).
pub const SHARED_LIGHTS_BINDINGS: u32 = 7;

/// Storage buffers the bindings add to the fragment stage (lights, counts, indices, tiles).
/// A host checks `max_storage_buffers_per_shader_stage` against its own count plus this.
pub const SHARED_LIGHTS_STORAGE_BUFFERS: u32 = 4;

/// Uniform buffers the bindings add to the fragment stage (the frame uniform).
pub const SHARED_LIGHTS_UNIFORM_BUFFERS: u32 = 1;

/// Bytes of the frame uniform the shader reads (its leading fields); a stand-in buffer bound
/// while the frame has no lights must be at least this large (zeroed: no lights).
pub const SHARED_LIGHTS_FRAME_BYTES: u64 = crate::frame::LAB_FRAME_LIGHTS_PREFIX as u64;

/// The `GpuLight` record size in the lights buffer, bytes (a stand-in holds at least one).
pub const SHARED_LIGHTS_LIGHT_BYTES: u64 = 64;

/// The `ShadowTile` record size in the tiles buffer, bytes (a stand-in holds at least one).
pub const SHARED_LIGHTS_TILE_BYTES: u64 = 96;

const SOURCE: &str = include_str!("../shaders/shared_local_lights.wgsl");

/// The frame's light data a shared draw shades with: the buffers and shadow atlas the forward
/// pass binds, by handle (cloning is cheap). Rebuilt every frame; compare with `==` to see
/// whether a bind group must be rebuilt.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineLights {
    /// The forward pass's frame uniform (`LabFrame`). Read: the view matrix, the cluster
    /// near/far, the colour target size, the light count and per-cluster cap, the shadow filter
    /// mode, the atlas size, the noise index and the PCSS tap counts.
    pub frame: wgpu::Buffer,
    /// The uploaded point and spot lights (`GpuLight`, 64 bytes each).
    pub lights: wgpu::Buffer,
    /// Lights per froxel (`u32` per cluster).
    pub counts: wgpu::Buffer,
    /// Light indices per froxel (`u32`, `max lights per cluster` slots per cluster).
    pub indices: wgpu::Buffer,
    /// The local-light shadow tiles (`ShadowTile`, 96 bytes each).
    pub tiles: wgpu::Buffer,
    /// The local-light shadow atlas (`Depth32Float`).
    pub atlas: wgpu::TextureView,
}

impl EngineLights {
    /// The bind-group entries for [`shared_lights_layout_entries`] from `first_binding`;
    /// `sampler` is a `LessEqual` comparison sampler with linear filtering and clamped
    /// addressing (the forward pass's shadow sampler).
    #[must_use]
    pub fn bind_entries<'a>(
        &'a self,
        first_binding: u32,
        sampler: &'a wgpu::Sampler,
    ) -> [wgpu::BindGroupEntry<'a>; 7] {
        let b = first_binding;
        [
            wgpu::BindGroupEntry {
                binding: b,
                resource: self.frame.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: b + 1,
                resource: self.lights.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: b + 2,
                resource: self.counts.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: b + 3,
                resource: self.indices.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: b + 4,
                resource: self.tiles.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: b + 5,
                resource: wgpu::BindingResource::TextureView(&self.atlas),
            },
            wgpu::BindGroupEntry {
                binding: b + 6,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ]
    }

    /// Zeroed stand-ins with the right usages and minimum sizes, for a shared draw that has
    /// no frame lights bound yet (the zero light count makes the shader add nothing).
    /// `atlas` is any `Depth32Float` 2-D view.
    #[must_use]
    pub fn stand_in(device: &wgpu::Device, atlas: wgpu::TextureView) -> Self {
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let storage = wgpu::BufferUsages::STORAGE;
        Self {
            frame: buffer(
                "shared lights frame (off)",
                SHARED_LIGHTS_FRAME_BYTES,
                wgpu::BufferUsages::UNIFORM,
            ),
            lights: buffer("shared lights (off)", SHARED_LIGHTS_LIGHT_BYTES, storage),
            counts: buffer("shared light counts (off)", 16, storage),
            indices: buffer("shared light indices (off)", 16, storage),
            tiles: buffer("shared light tiles (off)", SHARED_LIGHTS_TILE_BYTES, storage),
            atlas,
        }
    }
}

/// The fragment-stage layout entries of [`shared_local_lights_wgsl`]'s bindings, from
/// `first_binding` ([`SHARED_LIGHTS_BINDINGS`] of them, in [`EngineLights::bind_entries`]
/// order).
#[must_use]
pub fn shared_lights_layout_entries(first_binding: u32) -> [wgpu::BindGroupLayoutEntry; 7] {
    let frag = wgpu::ShaderStages::FRAGMENT;
    let storage = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: frag,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let b = first_binding;
    [
        wgpu::BindGroupLayoutEntry {
            binding: b,
            visibility: frag,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        },
        storage(b + 1),
        storage(b + 2),
        storage(b + 3),
        storage(b + 4),
        wgpu::BindGroupLayoutEntry {
            binding: b + 5,
            visibility: frag,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Depth,
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: b + 6,
            visibility: frag,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
            count: None,
        },
    ]
}

/// Plain WGSL (no `#import`s) declaring the light bindings at `@group(group)` from
/// `first_binding` on and the functions
///
/// - `engine_local_lights_on() -> bool`: the frame has lights (uniform; skip the call
///   otherwise), and
/// - `engine_local_lights(pix, p, n, n_geo, v, diffuse_color, f0, perceptual) -> vec3<f32>`:
///   the radiance the frame's point and spot lights reflect toward `v` (scene units, not
///   pre-exposed), with the forward pass's froxel lookup (`pix` in framebuffer pixels),
///   attenuation, spot cone, shadow atlas (normal offset along `n_geo`) and isotropic BRDF.
///
/// Engine-module helpers carry naga_oil's mangled names, so they cannot clash with the host's.
/// Fitted area emitters, translucent-caster tint, clearcoat and anisotropy are mesh-only.
///
/// # Errors
///
/// [`RenderError::Shader`] when the composition or the WGSL write fails (an engine module
/// edit broke it).
pub fn shared_local_lights_wgsl(group: u32, first_binding: u32) -> Result<String, RenderError> {
    compose_shared_lights(group, first_binding, None)
}

/// [`shared_local_lights_wgsl`] that also reads the lights' IES profiles (`LightProfile`, ADR-3247): it declares
/// the forward pass's emitter probe atlas at `profiles_binding` of `group` ([`shared_light_profiles_layout_entry`],
/// bound with [`crate::SharedPassDraw::set_engine_light_profiles`]) and shapes a profiled point or spot light's
/// intensity as a mesh's. A stand-in atlas smaller than a profile band (no frame atlas yet) leaves profiled lights
/// unshaped.
///
/// # Errors
///
/// As [`shared_local_lights_wgsl`].
pub fn shared_local_lights_profiled_wgsl(
    group: u32,
    first_binding: u32,
    profiles_binding: u32,
) -> Result<String, RenderError> {
    compose_shared_lights(group, first_binding, Some(profiles_binding))
}

/// The fragment-stage layout entry of the profile atlas [`shared_local_lights_profiled_wgsl`] declares at
/// `binding`: a non-filterable 2-D float texture (the frame's `R32Float` emitter probe atlas, read with
/// `textureLoad`). One sampled texture on top of the light bindings.
#[must_use]
pub fn shared_light_profiles_layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn compose_shared_lights(
    group: u32,
    first_binding: u32,
    profiles: Option<u32>,
) -> Result<String, RenderError> {
    let mut source = SOURCE.replace("ELL_GROUP", &group.to_string());
    let mut defs = std::collections::HashMap::new();
    if let Some(binding) = profiles {
        source = source.replace("ELL_BINDING_PROFILES", &binding.to_string());
        defs.insert(
            "ELL_PROFILES".to_string(),
            naga_oil::compose::ShaderDefValue::Bool(true),
        );
    }
    for (i, name) in [
        "FRAME", "LIGHTS", "COUNTS", "INDICES", "TILES", "ATLAS", "SAMPLER",
    ]
    .iter()
    .enumerate()
    {
        source = source.replace(
            &format!("ELL_BINDING_{name}"),
            &(first_binding + i as u32).to_string(),
        );
    }
    let module = crate::compose_source(&source, "shared_local_lights.wgsl", defs)?;
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|err| RenderError::Shader(format!("shared_local_lights.wgsl: {err:?}")))?;
    naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty())
        .map_err(|err| RenderError::Shader(format!("shared_local_lights.wgsl: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snippet_composes_and_parses_inside_a_host_shader() {
        let snippet = shared_local_lights_wgsl(3, 10).expect("composes");
        assert!(snippet.contains("@group(3) @binding(10)"), "{snippet}");
        assert!(snippet.contains("@group(3) @binding(16)"), "{snippet}");
        assert!(snippet.contains("fn engine_local_lights("), "{snippet}");
        assert!(snippet.contains("fn engine_local_lights_on("), "{snippet}");
        assert!(!snippet.contains("#import"));
        // A host shader with its own helpers of the same plain names (`d_ggx`, `cube_face`)
        // still parses: the engine helpers arrive mangled.
        let host = format!(
            "{snippet}
fn d_ggx(x: f32) -> f32 {{ return x; }}
fn cluster_index(x: u32) -> u32 {{ return x; }}
@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {{
    var c = vec3<f32>(d_ggx(0.5) + f32(cluster_index(1u)));
    if (engine_local_lights_on()) {{
        c += engine_local_lights(pos.xy, vec3<f32>(0.0), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, 1.0, 0.0),
            vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.5), vec3<f32>(0.04), 0.8);
    }}
    return vec4<f32>(c, 1.0);
}}
"
        );
        let module = naga::front::wgsl::parse_str(&host).expect("host parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        )
        .validate(&module)
        .expect("host validates");
    }

    /// ADR-3247: the profiled snippet declares the atlas at the host's binding, reads the profile, and still parses
    /// in a host shader; the plain one declares no atlas.
    #[test]
    fn the_profiled_snippet_declares_the_atlas_and_parses_inside_a_host_shader() {
        let plain = shared_local_lights_wgsl(3, 10).expect("composes");
        assert!(!plain.contains("@binding(19)"), "{plain}");
        assert!(!plain.contains("ell_profiles"), "{plain}");
        let snippet = shared_local_lights_profiled_wgsl(3, 10, 19).expect("composes");
        assert!(snippet.contains("@group(3) @binding(19)"), "{snippet}");
        assert!(snippet.contains("ell_profiles"), "{snippet}");
        let host = format!(
            "{snippet}
@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {{
    var c = vec3<f32>(0.0);
    if (engine_local_lights_on()) {{
        c += engine_local_lights(pos.xy, vec3<f32>(0.0), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, 1.0, 0.0),
            vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.5), vec3<f32>(0.04), 0.8);
    }}
    return vec4<f32>(c, 1.0);
}}
"
        );
        let module = naga::front::wgsl::parse_str(&host).expect("host parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        )
        .validate(&module)
        .expect("host validates");
        let entry = shared_light_profiles_layout_entry(19);
        assert_eq!(entry.binding, 19);
        assert!(matches!(
            entry.ty,
            wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                ..
            }
        ));
    }

    #[test]
    fn the_layout_matches_the_snippet_bindings() {
        let entries = shared_lights_layout_entries(10);
        assert_eq!(entries.len() as u32, SHARED_LIGHTS_BINDINGS);
        let storage = entries
            .iter()
            .filter(|e| {
                matches!(
                    e.ty,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { .. },
                        ..
                    }
                )
            })
            .count() as u32;
        assert_eq!(storage, SHARED_LIGHTS_STORAGE_BUFFERS);
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.binding, 10 + i as u32);
            assert_eq!(e.visibility, wgpu::ShaderStages::FRAGMENT);
        }
    }

    #[test]
    fn record_sizes_match_the_frame_layouts() {
        assert_eq!(SHARED_LIGHTS_LIGHT_BYTES, crate::frame::GPU_LIGHT_BYTES as u64);
        assert_eq!(SHARED_LIGHTS_TILE_BYTES, crate::frame::SHADOW_TILE_BYTES as u64);
        // view_proj, view (2 × 64), 8 vec4 (128), prev / unjittered view_proj (128), pcss (16).
        assert_eq!(SHARED_LIGHTS_FRAME_BYTES, 128 + 128 + 128 + 16);
    }

    #[test]
    fn ell_frame_prefix_matches_the_lab_frame() {
        let source = SOURCE
            .replace("ELL_GROUP", "0")
            .replace("ELL_BINDING_FRAME", "0")
            .replace("ELL_BINDING_LIGHTS", "1")
            .replace("ELL_BINDING_COUNTS", "2")
            .replace("ELL_BINDING_INDICES", "3")
            .replace("ELL_BINDING_TILES", "4")
            .replace("ELL_BINDING_ATLAS", "5")
            .replace("ELL_BINDING_SAMPLER", "6");
        let module =
            crate::compose_source(&source, "shared_local_lights.wgsl", Default::default()).expect("composes");
        let offsets = |name: &str| -> Vec<(String, u32)> {
            module
                .types
                .iter()
                .find(|(_, t)| t.name.as_deref() == Some(name))
                .and_then(|(_, t)| match &t.inner {
                    naga::TypeInner::Struct { members, .. } => Some(
                        members
                            .iter()
                            .map(|m| (m.name.clone().unwrap_or_default(), m.offset))
                            .collect(),
                    ),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("struct {name}"))
        };
        assert_eq!(offsets("EllFrame"), crate::frame::lab_frame_prefix_offsets());
        let light = offsets("EllLight");
        assert_eq!(
            light.last().map(|(_, o)| o + 16),
            Some(SHARED_LIGHTS_LIGHT_BYTES as u32)
        );
        let tile = offsets("EllShadowTile");
        assert_eq!(
            tile.last().map(|(_, o)| o + 16),
            Some(SHARED_LIGHTS_TILE_BYTES as u32)
        );
    }
}
