// SPDX-License-Identifier: MPL-2.0
//! The single-pass visibility resolve (ADR-2071, the plan's step in ADR-356): one full-screen draw
//! shades every standard material, on the texture table path only.
//!
//! The per-material resolve binds one material group per draw and rejects other materials' pixels
//! with a material-depth pass. Here group 2 binds the whole material arena and a map from the
//! frame's material slots to arena records, and each pixel finds its record through its instance.
//! Textures come from the table by per-pixel index, which is why this needs the table: without it
//! the textures of two materials cannot be reached from one draw. There is no material-depth pass
//! and no depth attachment.
//!
//! The forward shader reads its constants through the name `material`. A `var<private>` copy of
//! the record would be one struct, which ADR-026 forbids, and a per-field copy would be 30 private
//! variables, so [`single_pass_source`] rewrites the name to an access of this pixel's record when
//! it builds the module's source: only the fields a pixel uses are loaded, and only this variant
//! sees the rewrite.
//!
//! Every sample reachable from the entry point uses explicit gradients, and the table index carries
//! the `NonUniform` decoration. naga does not check derivative uniformity in the fragment stage
//! (ADR-356, fact 4), so `tests.rs` walks the composed module for that.

use std::collections::HashMap;

use super::SinglePassFallback;
use crate::device::TrackedQueue;
use crate::shader_lib::compose_source;
use crate::{RenderError, TrackedDevice};

/// The shader define of this variant.
pub(crate) const VIS_SINGLE_PASS_DEF: &str = "VIS_SINGLE_PASS";
/// The def that holds the arena's stride, in bytes.
pub(crate) const VIS_RECORD_STRIDE_DEF: &str = "VIS_RECORD_STRIDE";
/// A material slot with no record this frame (`VIS_EMPTY` in `vis_resolve.wgsl`).
const NO_RECORD: u32 = u32::MAX;
/// Fragment-stage storage buffers beyond [`super::RESOLVE_STORAGE_BUFFERS`]: the arena and the map.
const MATERIAL_STORAGE_BUFFERS: u32 = 2;

/// What the last frame's resolve did (`render.get_visbuffer` `resolve`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisResolveReport {
    /// The profile asked for the single-pass resolve.
    pub requested: bool,
    /// The last resolve shaded all its materials in one draw.
    pub single_pass: bool,
    /// Why a resolve that asked for it shaded per material.
    pub fallback: Option<SinglePassFallback>,
    /// Draws of the last resolve (1 on the single pass; 1 + materials per material).
    pub draws: u64,
    /// Materials the last resolve shaded.
    pub materials: u32,
    /// Of those, the ones shaded through the `LIGHTMAP` variant: the materials with a lightmap
    /// receiver instance (ADR-2689). The others keep the plain shade pipeline.
    pub lightmap_materials: u32,
}

/// Why `device` cannot run the single pass, or `None` when it can.
#[must_use]
pub(crate) fn fallback_on(device: &wgpu::Device) -> Option<SinglePassFallback> {
    fallback_for(
        crate::texture_table::device_support(device).capacity().is_some(),
        device.limits().max_storage_buffers_per_shader_stage,
        crate::residency_feedback::device_feedback_mode(device)
            != crate::residency_feedback::FeedbackMode::Off,
    )
}

fn fallback_for(table: bool, storage_buffers: u32, feedback: bool) -> Option<SinglePassFallback> {
    if !table {
        return Some(SinglePassFallback::NoTextureTable);
    }
    let needed = super::RESOLVE_STORAGE_BUFFERS + MATERIAL_STORAGE_BUFFERS + u32::from(feedback);
    (storage_buffers < needed).then_some(SinglePassFallback::StorageLimits)
}

/// `true` when `c` continues a WGSL identifier.
fn identifier_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `source` with every read of the identifier `material` turned into this pixel's record. A field
/// named `material` (after a `.`) and longer identifiers stay, and so do `@group` declaration
/// lines, which the `VIS_SINGLE_PASS` branch of the source has already replaced.
#[must_use]
fn rewrite_material_reads(source: &str) -> String {
    const NAME: &str = "material";
    const RECORD: &str = "vis_material_records[vis_material_record].record";
    let mut out = String::with_capacity(source.len() + source.len() / 16);
    for line in source.split_inclusive('\n') {
        if line.trim_start().starts_with("@group") || line.trim_start().starts_with("//") {
            out.push_str(line);
            continue;
        }
        let bytes = line.as_bytes();
        let mut start = 0;
        while let Some(found) = line[start..].find(NAME) {
            let at = start + found;
            let end = at + NAME.len();
            let free_before = at == 0 || !(identifier_byte(bytes[at - 1]) || bytes[at - 1] == b'.');
            let free_after = end == bytes.len() || !identifier_byte(bytes[end]);
            out.push_str(&line[start..at]);
            out.push_str(if free_before && free_after { RECORD } else { NAME });
            start = end;
        }
        out.push_str(&line[start..]);
    }
    out
}

/// Source of the single-pass module: the forward shader with `material` rewritten, plus the resolve.
pub(crate) fn single_pass_source() -> &'static str {
    static SOURCE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!(
            "{}\n{}",
            rewrite_material_reads(include_str!("../../shaders/lab_forward.wgsl")),
            include_str!("vis_resolve.wgsl")
        )
    });
    SOURCE.as_str()
}

/// The defs of the single-pass module on `device` for one colour target and forward module.
fn module_defs(
    device: &wgpu::Device,
    encode: bool,
    sun_rt: bool,
    stride: u64,
) -> HashMap<String, naga_oil::compose::ShaderDefValue> {
    use naga_oil::compose::ShaderDefValue;
    let mut defs = HashMap::new();
    defs.insert("VISBUFFER".to_string(), ShaderDefValue::Bool(true));
    defs.insert(VIS_SINGLE_PASS_DEF.to_string(), ShaderDefValue::Bool(true));
    defs.insert(
        VIS_RECORD_STRIDE_DEF.to_string(),
        ShaderDefValue::UInt(u32::try_from(stride).unwrap_or(u32::MAX)),
    );
    if !encode {
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    }
    if sun_rt {
        crate::hwrt::SunShadowVariant::RayQuery.add_forward_defs(&mut defs);
    }
    crate::bindless::add_material_defs(device, &mut defs);
    defs
}

/// The defs the shader tests compose this variant with: a table device with the narrowest stride.
#[cfg(test)]
pub(crate) fn checked_defs() -> HashMap<String, naga_oil::compose::ShaderDefValue> {
    use naga_oil::compose::ShaderDefValue;
    [
        ("VISBUFFER", ShaderDefValue::Bool(true)),
        (VIS_SINGLE_PASS_DEF, ShaderDefValue::Bool(true)),
        (VIS_RECORD_STRIDE_DEF, ShaderDefValue::UInt(368)),
        ("skip_srgb_encode", ShaderDefValue::Bool(true)),
        (crate::bindless::MATERIAL_SSBO_DEF, ShaderDefValue::Bool(true)),
        (
            crate::texture_table::TEXTURE_TABLE_DEF,
            ShaderDefValue::Bool(true),
        ),
        (
            crate::texture_table::TEXTURE_TABLE_SIZE_DEF,
            ShaderDefValue::UInt(4096),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

/// What a single-pass pipeline is built for: the arena's stride, a colour target and a forward
/// module.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SinglePassTarget {
    /// Bytes between two arena records.
    pub(crate) stride: u64,
    /// The colour format.
    pub(crate) format: wgpu::TextureFormat,
    /// The pipeline encodes sRGB in the shader.
    pub(crate) encode: bool,
    /// The ray-traced sun's forward module.
    pub(crate) sun_rt: bool,
}

/// The pipelines and the frame's map of the single-pass resolve.
#[derive(Default)]
pub(crate) struct SinglePass {
    bgl: Option<wgpu::BindGroupLayout>,
    layout: Option<wgpu::PipelineLayout>,
    pipelines: HashMap<(wgpu::TextureFormat, bool, bool), wgpu::RenderPipeline>,
    /// The arena stride the pipelines were composed with.
    stride: Option<u64>,
    /// The frame's map, kept so an unchanged one writes nothing.
    map: Vec<u32>,
    /// The map's buffer and its capacity in records.
    map_buffer: Option<(wgpu::Buffer, usize)>,
    /// Bumped when the map's buffer is replaced.
    map_generation: u64,
    /// `(arena id, arena epoch, map generation, group)` of the last build.
    group: Option<(u64, u32, u64, wgpu::BindGroup)>,
    /// What the last resolve did.
    pub(crate) report: VisResolveReport,
    /// Scratch for the next map.
    next: Vec<u32>,
}

impl SinglePass {
    /// Takes `warmed`'s pipelines, where this holds none (`VisBufferPass::adopt_pipelines`).
    pub(crate) fn adopt_pipelines(&mut self, warmed: &mut Self) {
        if self.pipelines.is_empty() {
            self.bgl = warmed.bgl.take();
            self.layout = warmed.layout.take();
            self.pipelines = std::mem::take(&mut warmed.pipelines);
            self.stride = warmed.stride.take();
        }
    }

    /// Group 2 of the single-pass resolve (the arena and the slot map); the masked raster binds it
    /// as its group 1 (ADR-3915).
    pub(crate) fn group_layout(&mut self, d: &TrackedDevice) -> wgpu::BindGroupLayout {
        self.bgl
            .get_or_insert_with(|| {
                let entry = |binding, min: u64| wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: std::num::NonZeroU64::new(min),
                    },
                    count: None,
                };
                d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("vis single-pass materials"),
                    entries: &[entry(0, 0), entry(1, 4)],
                })
            })
            .clone()
    }

    /// The shading pipeline for a colour target and forward module, built once.
    pub(crate) fn pipeline(
        &mut self,
        d: &TrackedDevice,
        resolve_bgl: &wgpu::BindGroupLayout,
        view_bgl: &wgpu::BindGroupLayout,
        target: SinglePassTarget,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        let SinglePassTarget {
            stride,
            format,
            encode,
            sun_rt,
        } = target;
        if let Some(pipeline) = self.pipelines.get(&(format, encode, sun_rt)) {
            return Ok(pipeline.clone());
        }
        self.stride = Some(stride);
        let group = self.group_layout(d);
        let layout = self
            .layout
            .get_or_insert_with(|| {
                let table = crate::texture_table::table_layout(d);
                d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("vis single-pass resolve"),
                    bind_group_layouts: &crate::texture_table::with_table(
                        &[Some(view_bgl), Some(resolve_bgl), Some(&group)],
                        table.as_ref(),
                    ),
                    immediate_size: 0,
                })
            })
            .clone();
        let naga = compose_source(
            single_pass_source(),
            "vis_resolve_single.wgsl",
            module_defs(d, encode, sun_rt, stride),
        )?;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vis single-pass resolve"),
            source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(naga)),
        });
        let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vis single-pass resolve"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_vis_fullscreen"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_vis_resolve"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(d).as_deref(),
        });
        self.pipelines.insert((format, encode, sun_rt), pipeline.clone());
        Ok(pipeline)
    }

    /// The arena stride of the pipelines built here.
    pub(crate) fn stride(&self) -> Option<u64> {
        self.stride
    }

    /// The forward modules and targets already built here, with `sun_rt` flipped (warms).
    pub(crate) fn twins(&self) -> Vec<(wgpu::TextureFormat, bool, bool)> {
        self.pipelines.keys().map(|&(f, e, s)| (f, e, !s)).collect()
    }

    /// Writes the frame's slot map: `records` holds `(material slot, arena record)` pairs. An
    /// unchanged map uploads nothing, and the buffer only grows.
    pub(crate) fn write_map(
        &mut self,
        d: &TrackedDevice,
        q: &TrackedQueue,
        slots: usize,
        records: impl Iterator<Item = (u32, u32)>,
    ) {
        self.next.clear();
        self.next.resize(slots.max(1), NO_RECORD);
        for (slot, record) in records {
            if let Some(entry) = self.next.get_mut(slot as usize) {
                *entry = record;
            }
        }
        let needed = self.next.len();
        let grown = self.map_buffer.as_ref().is_none_or(|(_, have)| *have < needed);
        if grown {
            let capacity = needed.next_power_of_two().max(64);
            let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vis single-pass slot map"),
                size: capacity as u64 * 4,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.map_buffer = Some((buffer, capacity));
            self.map_generation += 1;
        }
        if grown || self.map != self.next {
            if let Some((buffer, _)) = &self.map_buffer {
                q.write_buffer(buffer, 0, bytemuck::cast_slice(&self.next));
            }
            std::mem::swap(&mut self.map, &mut self.next);
        }
    }

    /// Group 2 of the single-pass pipeline: `arena`'s whole buffer and the map. Rebuilt only when
    /// the arena's buffer or the map's buffer is replaced.
    pub(crate) fn bind_group(
        &mut self,
        d: &TrackedDevice,
        arena: &crate::bindless::MaterialArena,
    ) -> Option<wgpu::BindGroup> {
        let key = (arena.id(), arena.epoch(), self.map_generation);
        if let Some((id, epoch, generation, group)) = &self.group
            && (*id, *epoch, *generation) == key
        {
            return Some(group.clone());
        }
        let layout = self.group_layout(d);
        let (map, _) = self.map_buffer.as_ref()?;
        let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vis single-pass materials"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(arena.whole_binding()),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: map.as_entire_binding(),
                },
            ],
        });
        self.group = Some((key.0, key.1, key.2, group.clone()));
        Some(group)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_names_the_first_missing_requirement() {
        assert_eq!(
            fallback_for(false, 64, false),
            Some(SinglePassFallback::NoTextureTable)
        );
        assert_eq!(fallback_for(true, 64, false), None);
        // The resolve's buffers (11 since ADR-4906: the view group binds seven) plus the arena and
        // the map.
        let needed = super::super::RESOLVE_STORAGE_BUFFERS + MATERIAL_STORAGE_BUFFERS;
        assert_eq!(needed, 13);
        assert_eq!(fallback_for(true, needed, false), None);
        assert_eq!(
            fallback_for(true, needed - 1, false),
            Some(SinglePassFallback::StorageLimits)
        );
        assert_eq!(
            fallback_for(true, needed, true),
            Some(SinglePassFallback::StorageLimits),
            "residency feedback takes a storage buffer too"
        );
    }

    #[test]
    fn only_reads_of_the_name_are_rewritten() {
        let record = "vis_material_records[vis_material_record].record";
        for (text, want) in [
            (
                "let a = material.base_color;\n",
                format!("let a = {record}.base_color;\n"),
            ),
            ("    return material;\n", format!("    return {record};\n")),
            ("(material.a+material.b)", format!("({record}.a+{record}.b)")),
            (
                "fn material_constants() -> LabMaterial {\n",
                "fn material_constants() -> LabMaterial {\n".to_string(),
            ),
            ("x.material.y", "x.material.y".to_string()),
            ("my_material.y", "my_material.y".to_string()),
            ("material_shading", "material_shading".to_string()),
            (
                "@group(2) @binding(0) var<uniform> material: LabMaterial;\n",
                "@group(2) @binding(0) var<uniform> material: LabMaterial;\n".to_string(),
            ),
            ("// material. stays\n", "// material. stays\n".to_string()),
            ("no match\n", "no match\n".to_string()),
        ] {
            assert_eq!(rewrite_material_reads(text), want, "{text}");
        }
    }

    #[test]
    fn the_rewrite_leaves_no_read_of_the_old_binding() {
        // Composed, it must resolve every `material` read: the old binding is gone under the def.
        let source = single_pass_source();
        assert!(source.contains("vis_material_records[vis_material_record].record.base_color"));
    }
}
