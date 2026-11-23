// SPDX-License-Identifier: MPL-2.0
//! GPU side of per-instance glass ([`crate::glass`]): the `GLASS` variant of the forward shader,
//! its group 3 (refraction copy + crack page + parameters), and the page uploads.
//!
//! Only frames with [`crate::glass::GlassInstance`]s touch any of this; every other draw keeps its
//! pipelines and bind groups, so frames without glass components are byte-identical.

use std::collections::HashMap;

use naga_oil::compose::ShaderDefValue;

use super::{
    PipelineKey, depth_tex_entry, extent3d, lab_color_pipeline_entry, samp_entry, shader_module,
    storage_uniform, tex_entry,
};
use crate::RenderError;
use crate::glass::{GlassInstance, mask::mip_chain_rg8};

/// Size of [`GlassUniform`] in bytes.
const UNIFORM_BYTES: u64 = std::mem::size_of::<GlassUniform>() as u64;

/// `GlassParams` in `lab_forward.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct GlassUniform {
    mask_from_world: [[f32; 4]; 4],
    axis_u: [f32; 4],
    axis_v: [f32; 4],
    scatter: [f32; 4],
    shade: [f32; 4],
    holes: [f32; 4],
    inner: [f32; 4],
    view: [f32; 4],
}

/// The three transmission pipelines of one target configuration, `GLASS` variant.
#[derive(Clone)]
pub(super) struct GlassSet {
    /// `fs_transmit`: dst × transmittance (holes discarded).
    pub mul: wgpu::RenderPipeline,
    /// `fs_refract`: + transmittance × (bent − unbent), with crack bends.
    pub refract: wgpu::RenderPipeline,
    /// `fs_main`: + reflection and the frosted crack scatter.
    pub add: wgpu::RenderPipeline,
    /// `fs_glass_add`: [`Self::add`] weighted by the glass front layer (frames with the
    /// refraction copy, whose group 3 binds the layer).
    pub add_front: wgpu::RenderPipeline,
    /// `fs_canopy_add`: [`Self::add`] for a viewer-inside instance on the cheap path
    /// ([`crate::glass::CanopyShading::Cheap`]).
    pub add_canopy: wgpu::RenderPipeline,
    /// `fs_canopy_add_front`: [`Self::add_front`] on the cheap path.
    pub add_front_canopy: wgpu::RenderPipeline,
}

impl GlassSet {
    /// The own-light pipeline of an instance: `front` on frames with the glass front layer,
    /// `cheap` for a viewer-inside instance on the cheap path.
    pub(super) fn add_for(&self, front: bool, cheap: bool) -> &wgpu::RenderPipeline {
        match (front, cheap) {
            (false, false) => &self.add,
            (true, false) => &self.add_front,
            (false, true) => &self.add_canopy,
            (true, true) => &self.add_front_canopy,
        }
    }
}

/// Lazily built layout and pipelines (kept in [`super::Pipes`]).
///
/// One shader module per `encode` bit: cull mode is pipeline state, not a shader define, so the
/// back-face-culled set and the cull-none set share the module.
#[derive(Default)]
pub(super) struct GlassPipes {
    bgl: Option<wgpu::BindGroupLayout>,
    layout: Option<wgpu::PipelineLayout>,
    /// `encode` → compiled `GLASS` module.
    modules: HashMap<bool, wgpu::ShaderModule>,
    sets: HashMap<PipelineKey, GlassSet>,
}

impl GlassPipes {
    /// Builds the culled and cull-none sets for `key`'s format, encode and sample count.
    ///
    /// `key.double_sided` is ignored: both cull modes are built. A later [`Self::set`] of either
    /// key returns the cached pipelines and creates none, so a frame that was prewarmed does not
    /// compile glass pipelines.
    pub(super) fn prewarm(
        &mut self,
        d: &wgpu::Device,
        groups: [&wgpu::BindGroupLayout; 3],
        key: PipelineKey,
    ) -> Result<(), RenderError> {
        let _span = tracing::info_span!("glass_pipeline_prewarm").entered();
        // This family keeps the full forward module under either studio key (ADR-2308).
        let key = key.full_studio();
        let culled = PipelineKey {
            double_sided: false,
            ..key
        };
        let cull_off = PipelineKey {
            double_sided: true,
            ..key
        };
        self.set(d, groups, culled)?;
        crate::pipeline_warm::warm_checkpoint()?;
        self.set(d, groups, cull_off)?;
        Ok(())
    }

    /// The pipelines for `key`, built on first use.
    ///
    /// [`Self::prewarm`] at level load is what keeps this off the frame path.
    pub(super) fn set(
        &mut self,
        d: &wgpu::Device,
        groups: [&wgpu::BindGroupLayout; 3],
        key: PipelineKey,
    ) -> Result<(GlassSet, wgpu::BindGroupLayout), RenderError> {
        // Glass blends: it is not in the depth the sun trace starts from and keeps the shadow map
        // under either forward module, so both keys share one set. It keeps the full module under
        // either studio key too (ADR-2308).
        let key = PipelineKey {
            sun_rt: false,
            studio_off: false,
            restir_di: false,
            ..key
        };
        if let (Some(set), Some(bgl)) = (self.sets.get(&key), self.bgl.as_ref()) {
            return Ok((set.clone(), bgl.clone()));
        }
        let bgl = self
            .bgl
            .get_or_insert_with(|| {
                d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("lab glass bgl"),
                    entries: &[
                        tex_entry(0),
                        depth_tex_entry(1),
                        samp_entry(2),
                        storage_uniform(3, wgpu::ShaderStages::FRAGMENT),
                        tex_entry(4),
                        tex_entry(5),
                        samp_entry(6),
                        depth_tex_entry(7),
                        tex_entry(8),
                    ],
                })
            })
            .clone();
        let layout = self
            .layout
            .get_or_insert_with(|| {
                // The material texture table (group 4) on the table path (ADR-863).
                let table = crate::texture_table::table_layout(d);
                d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lab glass layout"),
                    bind_group_layouts: &crate::texture_table::with_table(
                        &[Some(groups[0]), Some(groups[1]), Some(groups[2]), Some(&bgl)],
                        table.as_ref(),
                    ),
                    immediate_size: 0,
                })
            })
            .clone();
        let module = self.module(d, key.encode)?;
        let multisample = wgpu::MultisampleState {
            count: key.samples,
            mask: u64::MAX,
            alpha_to_coverage_enabled: false,
        };
        let keep_alpha = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        let add = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: keep_alpha,
        };
        let pipeline = |label, entry, blend| {
            lab_color_pipeline_entry(
                d,
                &layout,
                &module,
                label,
                entry,
                Some(blend),
                false,
                key.format,
                key.double_sided,
                multisample,
            )
        };
        let set = GlassSet {
            mul: pipeline(
                "lab glass multiply",
                "fs_transmit",
                wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::Dst,
                        dst_factor: wgpu::BlendFactor::Zero,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: keep_alpha,
                },
            ),
            refract: pipeline("lab glass refract", "fs_refract", add),
            add: pipeline("lab glass add", "fs_main", add),
            add_front: pipeline("lab glass add (front layer)", "fs_glass_add", add),
            add_canopy: pipeline("lab glass add (own canopy)", "fs_canopy_add", add),
            add_front_canopy: pipeline(
                "lab glass add (own canopy, front layer)",
                "fs_canopy_add_front",
                add,
            ),
        };
        self.sets.insert(key, set.clone());
        Ok((set, bgl))
    }

    /// The `GLASS` module for `encode`, compiled once and shared by both cull modes.
    fn module(&mut self, d: &wgpu::Device, encode: bool) -> Result<wgpu::ShaderModule, RenderError> {
        if let Some(module) = self.modules.get(&encode) {
            return Ok(module.clone());
        }
        let mut defs = HashMap::new();
        defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
        defs.insert("GLASS".to_string(), ShaderDefValue::Bool(true));
        if !encode {
            defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
        }
        let module = shader_module(
            d,
            "lab_forward_glass",
            include_str!("../../shaders/lab_forward.wgsl"),
            "lab_forward.wgsl",
            defs,
        )?;
        self.modules.insert(encode, module.clone());
        Ok(module)
    }
}

/// One instance's GPU state: its uniform buffer and its uploaded page.
struct InstanceGpu {
    uniform: wgpu::Buffer,
    uploaded: Option<GlassUniform>,
    /// (mask view, normal view, page version, page size).
    page: Option<(wgpu::TextureView, wgpu::TextureView, u64, u32)>,
    last_frame: u64,
}

/// 1×1 stand-ins for frames without refraction or instances without damage.
struct Dummies {
    scene: wgpu::TextureView,
    depth: wgpu::TextureView,
    mask: wgpu::TextureView,
    normal: wgpu::TextureView,
    /// White: a front-layer transmittance of 1 (the layer is bound only with refraction).
    white: wgpu::TextureView,
    sampler: wgpu::Sampler,
}

/// Per-renderer glass GPU caches.
#[derive(Default)]
pub(super) struct GlassGpu {
    instances: HashMap<u64, InstanceGpu>,
    dummies: Option<Dummies>,
    frame: u64,
}

/// Frame inputs for [`GlassGpu::prepare`].
pub(super) struct GlassFrame<'a> {
    /// Refraction copy and the scene depth, when this frame refracts.
    pub refract: Option<(&'a wgpu::TextureView, &'a wgpu::TextureView)>,
    /// The glass front layer's depth and transmittance, when this frame refracts.
    pub front: Option<(&'a wgpu::TextureView, &'a wgpu::TextureView)>,
    /// Radians per internal pixel (page mip selection).
    pub pixel_angle: f32,
    /// [`crate::glass::CanopyGlass::nearest_layer_only`] on the cheap path (`view.x` 2 for a
    /// viewer-inside instance).
    pub nearest_layer_only: bool,
}

impl GlassGpu {
    /// Uploads changed pages and parameters and builds this frame's group-3 bind group for each
    /// of `instances` (same order). Instances unused for 120 frames are dropped.
    pub(super) fn prepare(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        bgl: &wgpu::BindGroupLayout,
        instances: &[GlassInstance],
        frame: &GlassFrame<'_>,
    ) -> Vec<wgpu::BindGroup> {
        let _span = tracing::info_span!("glass_prepare").entered();
        self.frame += 1;
        let dummies = self.dummies.get_or_insert_with(|| make_dummies(d, q));
        let mut out = Vec::with_capacity(instances.len());
        for instance in instances {
            let entry = sync_instance(&mut self.instances, self.frame, d, q, instance);
            let uniform = uniform_for(instance, frame.pixel_angle, frame.nearest_layer_only);
            if entry.uploaded != Some(uniform) {
                q.write_buffer(&entry.uniform, 0, bytemuck::bytes_of(&uniform));
                entry.uploaded = Some(uniform);
            }
            let (scene, depth) = frame.refract.unwrap_or((&dummies.scene, &dummies.depth));
            let (front_depth, front_t) = frame.front.unwrap_or((&dummies.depth, &dummies.white));
            let (mask, normal) = entry
                .page
                .as_ref()
                .map_or((&dummies.mask, &dummies.normal), |(m, n, _, _)| (m, n));
            out.push(d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lab glass bg"),
                layout: bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(scene),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(depth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&dummies.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: entry.uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::TextureView(mask),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: wgpu::BindingResource::TextureView(normal),
                    },
                    wgpu::BindGroupEntry {
                        binding: 6,
                        resource: wgpu::BindingResource::Sampler(&dummies.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 7,
                        resource: wgpu::BindingResource::TextureView(front_depth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 8,
                        resource: wgpu::BindingResource::TextureView(front_t),
                    },
                ],
            }));
        }
        let now = self.frame;
        self.instances.retain(|_, e| now - e.last_frame < 120);
        out
    }
}

impl GlassGpu {
    /// Group 3 of the translucent shadow casters (`translucent_shadow.wgsl` `GLASS`) for each
    /// of `instances` (same order): the parameters and the crack page. Uploads what
    /// [`Self::prepare`] would, so the frame's later `prepare` finds them current; a new
    /// instance's parameters are written with no pixel angle (the shadow pass does not read
    /// it, and `prepare` rewrites them before the frame is submitted).
    pub(super) fn caster_groups(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        bgl: &wgpu::BindGroupLayout,
        instances: &[GlassInstance],
    ) -> Vec<wgpu::BindGroup> {
        let dummies = self.dummies.get_or_insert_with(|| make_dummies(d, q));
        let mut out = Vec::with_capacity(instances.len());
        for instance in instances {
            let entry = sync_instance(&mut self.instances, self.frame, d, q, instance);
            if entry.uploaded.is_none() {
                let uniform = uniform_for(instance, 0.0, false);
                q.write_buffer(&entry.uniform, 0, bytemuck::bytes_of(&uniform));
                entry.uploaded = Some(uniform);
            }
            let mask = entry.page.as_ref().map_or(&dummies.mask, |(m, _, _, _)| m);
            out.push(d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("translucent shadow glass bg"),
                layout: bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: entry.uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(mask),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&dummies.sampler),
                    },
                ],
            }));
        }
        out
    }
}

/// `instance`'s GPU entry, created on first use, with its crack page current.
fn sync_instance<'a>(
    instances: &'a mut HashMap<u64, InstanceGpu>,
    frame: u64,
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    instance: &GlassInstance,
) -> &'a mut InstanceGpu {
    let entry = instances
        .entry(instance.object_key)
        .or_insert_with(|| InstanceGpu {
            uniform: d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("glass params"),
                size: UNIFORM_BYTES,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            uploaded: None,
            page: None,
            last_frame: 0,
        });
    entry.last_frame = frame;
    if let Some(damage) = &instance.damage {
        let mask = &damage.mask;
        let stale = entry
            .page
            .as_ref()
            .is_none_or(|(_, _, version, size)| *version != mask.version || *size != mask.size);
        if stale {
            let (mask_view, normal_view) = upload_page(d, q, mask);
            entry.page = Some((mask_view, normal_view, mask.version, mask.size));
        }
    } else {
        entry.page = None;
    }
    entry
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn uniform_for(instance: &GlassInstance, pixel_angle: f32, nearest_layer_only: bool) -> GlassUniform {
    let view = [
        match (instance.inside, nearest_layer_only) {
            (false, _) => 0.0,
            (true, false) => 1.0,
            (true, true) => 2.0,
        },
        instance.specular_scale,
        pixel_angle,
        instance.damage.as_ref().map_or(1.0, |d| d.mask.size as f32),
    ];
    let Some(d) = &instance.damage else {
        return GlassUniform {
            mask_from_world: [[0.0; 4]; 4],
            axis_u: [1.0, 0.0, 0.0, 0.0],
            axis_v: [0.0, 1.0, 0.0, 1.0],
            scatter: [1.0, 1.0, 1.0, 1.0],
            shade: [0.0; 4],
            holes: [1.0, 0.0, 0.0, 0.0],
            inner: [1.0, 1.0, 1.0, 0.0],
            view,
        };
    };
    let s = &d.shading;
    let lin = |c: [f32; 3]| [srgb_to_linear(c[0]), srgb_to_linear(c[1]), srgb_to_linear(c[2])];
    let scatter = lin(s.crack_scatter);
    let inner = lin(d.inner_tint);
    let pressure = d.glass_type == regia_asset::glasscrack::GlassType::PressurePane;
    GlassUniform {
        mask_from_world: d.mask_from_world,
        axis_u: [
            d.axis_u[0],
            d.axis_u[1],
            d.axis_u[2],
            if d.uv0 { 1.0 } else { 0.0 },
        ],
        axis_v: [d.axis_v[0], d.axis_v[1], d.axis_v[2], d.metres_per_uv],
        scatter: [scatter[0], scatter[1], scatter[2], s.crack_transmission],
        shade: [
            s.crack_roughness,
            s.crazing_haze,
            s.crazing_roughness,
            s.normal_bend,
        ],
        holes: [
            s.hole_cutoff,
            s.edge_jitter,
            if pressure { 1.0 } else { 0.0 },
            if pressure { d.pane_gap_m } else { 0.0 },
        ],
        inner: [inner[0], inner[1], inner[2], 1.0],
        view,
    }
}

fn make_dummies(d: &wgpu::Device, q: &crate::device::TrackedQueue) -> Dummies {
    let one = |format: wgpu::TextureFormat, bytes: &[u8], label: &str| {
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: extent3d(1, 1),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        if !bytes.is_empty() {
            q.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes.len() as u32),
                    rows_per_image: Some(1),
                },
                extent3d(1, 1),
            );
        }
        texture.create_view(&wgpu::TextureViewDescriptor::default())
    };
    Dummies {
        scene: one(
            wgpu::TextureFormat::Rgba8Unorm,
            &[0, 0, 0, 0],
            "glass dummy scene",
        ),
        depth: one(wgpu::TextureFormat::Depth32Float, &[], "glass dummy depth"),
        mask: one(wgpu::TextureFormat::Rgba8Unorm, &[0, 0, 0, 0], "glass dummy mask"),
        normal: one(wgpu::TextureFormat::Rg8Unorm, &[128, 128], "glass dummy normal"),
        white: one(
            wgpu::TextureFormat::Rgba8Unorm,
            &[255, 255, 255, 255],
            "glass dummy front layer",
        ),
        sampler: d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glass sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        }),
    }
}

/// Uploads a page with full mip chains (mask RGBA8, normal RG8).
fn upload_page(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    mask: &crate::glass::GlassMask,
) -> (wgpu::TextureView, wgpu::TextureView) {
    let _span = tracing::info_span!("glass_page_upload").entered();
    let n = mask.size.max(1);
    let levels = n.ilog2() + 1;
    let make = |format: wgpu::TextureFormat, label: &str| {
        crate::frame_stats::note_texture(Some(label), u64::from(n * n) * 4);
        d.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: extent3d(n, n),
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    };
    let write = |texture: &wgpu::Texture, level: u32, size: u32, texel: u32, bytes: &[u8]| {
        q.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size * texel),
                rows_per_image: Some(size),
            },
            extent3d(size, size),
        );
    };
    let mask_tex = make(wgpu::TextureFormat::Rgba8Unorm, "glass crack mask");
    write(&mask_tex, 0, n, 4, &mask.rgba);
    for (level, (w, _, bytes)) in crate::mips::mip_chain_rgba8(n, n, &mask.rgba, false)
        .iter()
        .enumerate()
    {
        write(&mask_tex, level as u32 + 1, *w, 4, bytes);
    }
    let normal_tex = make(wgpu::TextureFormat::Rg8Unorm, "glass crack normal");
    write(&normal_tex, 0, n, 2, &mask.normal);
    for (level, (w, bytes)) in mip_chain_rg8(n, &mask.normal).iter().enumerate() {
        write(&normal_tex, level as u32 + 1, *w, 2, bytes);
    }
    (
        mask_tex.create_view(&wgpu::TextureViewDescriptor::default()),
        normal_tex.create_view(&wgpu::TextureViewDescriptor::default()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(inside: bool) -> GlassInstance {
        GlassInstance {
            object_key: 7,
            inside,
            specular_scale: if inside { 0.15 } else { 1.0 },
            damage: None,
        }
    }

    #[test]
    fn view_x_maps_exterior_inside_and_nearest_layer_only() {
        // `GlassParams::view.x` in `lab_forward.wgsl`: 0 exterior, 1 viewer inside, 2 viewer
        // inside with the nearest-layer option (`fs_canopy_add_front` drops the layers behind).
        let cases = [
            (false, false, 0.0),
            (false, true, 0.0),
            (true, false, 1.0),
            (true, true, 2.0),
        ];
        for (inside, nearest, want) in cases {
            let u = uniform_for(&instance(inside), 0.002, nearest);
            assert_eq!(u.view[0], want, "inside {inside}, nearest_layer_only {nearest}");
            assert_eq!(u.view[1], instance(inside).specular_scale);
            assert_eq!(u.view[2], 0.002);
            // No damage: no page bound, so the shader's crack terms are off.
            assert_eq!(u.inner[3], 0.0);
        }
    }
}
