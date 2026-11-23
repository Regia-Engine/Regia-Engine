// SPDX-License-Identifier: MPL-2.0
//! GPU side of material mask recolor and decal slots ([`crate::mask_decals`]): the `MASK_DECALS`
//! variant of the forward shader, its group 3 (the instance's uniform block, mask and atlas), and
//! the per-instance entries.
//!
//! Only draws of instances set with [`FrameRenderer::set_mask_decals`] touch any of this. Every
//! other draw keeps its pipelines, bind groups and object block, so frames without masked
//! instances are unchanged. Group 3 bindings 9-11 do not collide with the refraction (0-2, 7, 8)
//! or glass (3-6) groups declared in the same shader file.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use naga_oil::compose::ShaderDefValue;

use super::{
    FrameRenderer, PipelineKey, lab_color_pipeline_entry, shader_module, storage_uniform, tex_entry,
};
use crate::mask_decals::{GpuMaskBlock, MaskDecalInstance, MaskDecalStats, MaskDecalTextures};
use crate::pipeline_warm::DeferredPipeline;
use crate::texture_residency::{MaterialImage, ShareKey, UploadEnv, bind_material_image};

/// Group 3 binding of the instance's uniform block.
const BLOCK_BINDING: u32 = 9;
/// Group 3 binding of the mask texture.
const MASK_BINDING: u32 = 10;
/// Group 3 binding of the decal atlas.
const ATLAS_BINDING: u32 = 11;

/// The `MASK_DECALS` forward variants of one target configuration.
#[derive(Clone)]
pub(super) struct MaskSet {
    /// Opaque and alpha-tested colour (`fs_main`, depth equal to the prepass).
    pub opaque: DeferredPipeline<wgpu::RenderPipeline>,
    /// Alpha-blended colour.
    pub blend: DeferredPipeline<wgpu::RenderPipeline>,
}

/// Group 3 layout, the pipeline layout and the variant sets (kept in [`super::Pipes`], so a
/// renderer that adopts warmed pipelines adopts these too).
#[derive(Default)]
pub(super) struct MaskPipes {
    /// The layout and a process-unique id: an entry built against another layout rebinds.
    bgl: Option<(wgpu::BindGroupLayout, u64)>,
    layout: Option<wgpu::PipelineLayout>,
    /// `(encode, sun_rt)` → the module, composed by the first variant built (on a warm worker or in a warm).
    modules: HashMap<(bool, bool), Arc<OnceLock<wgpu::ShaderModule>>>,
    sets: HashMap<PipelineKey, MaskSet>,
}

fn next_layout_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl MaskPipes {
    /// Group 3 layout (created on first use) and its id.
    pub fn bgl(&mut self, d: &wgpu::Device) -> (wgpu::BindGroupLayout, u64) {
        self.bgl
            .get_or_insert_with(|| {
                let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("lab mask decals bgl"),
                    entries: &[
                        storage_uniform(BLOCK_BINDING, wgpu::ShaderStages::FRAGMENT),
                        tex_entry(MASK_BINDING),
                        tex_entry(ATLAS_BINDING),
                    ],
                });
                (bgl, next_layout_id())
            })
            .clone()
    }

    /// Game-frame warms: the other forward module (ADR-2035) of every set, built where the set's
    /// own variant is. Returns how many sets it asked for.
    pub(super) fn warm_sun_twins(&mut self, d: &wgpu::Device, groups: [&wgpu::BindGroupLayout; 3]) -> usize {
        let twins: Vec<(PipelineKey, MaskSet)> = self
            .sets
            .iter()
            .map(|(key, set)| {
                (
                    PipelineKey {
                        sun_rt: !key.sun_rt,
                        ..*key
                    },
                    set.clone(),
                )
            })
            .collect();
        let asked = twins.len();
        for (key, built) in twins {
            let set = self.set(d, groups, key);
            if built.opaque.is_built() {
                set.opaque.get();
            }
            if built.blend.is_built() {
                set.blend.get();
            }
        }
        asked
    }

    /// The variants for `key`. Creating them registers background builds (no compile here while
    /// warm workers run; without workers they build now, like every deferred variant).
    pub fn set(
        &mut self,
        d: &wgpu::Device,
        groups: [&wgpu::BindGroupLayout; 3],
        key: PipelineKey,
    ) -> MaskSet {
        // This family keeps the full forward module under either studio key (ADR-2308).
        let key = key.full_studio();
        if let Some(set) = self.sets.get(&key) {
            return set.clone();
        }
        let (bgl, _) = self.bgl(d);
        let layout = self
            .layout
            .get_or_insert_with(|| {
                // The material texture table (group 4) on the table path (ADR-863).
                let table = crate::texture_table::table_layout(d);
                d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lab mask decals layout"),
                    bind_group_layouts: &crate::texture_table::with_table(
                        &[Some(groups[0]), Some(groups[1]), Some(groups[2]), Some(&bgl)],
                        table.as_ref(),
                    ),
                    immediate_size: 0,
                })
            })
            .clone();
        let module = self.modules.entry((key.encode, key.sun_rt)).or_default().clone();
        let multisample = wgpu::MultisampleState {
            count: key.samples,
            mask: u64::MAX,
            alpha_to_coverage_enabled: false,
        };
        let variant = |label: &'static str, blend: Option<wgpu::BlendState>, depth_write: bool| {
            let (device, layout, module) = (wgpu::Device::clone(d), layout.clone(), module.clone());
            DeferredPipeline::new(label, move || {
                let module = module.get_or_init(|| compile(&device, key.encode, key.sun_rt));
                lab_color_pipeline_entry(
                    &device,
                    &layout,
                    module,
                    label,
                    "fs_main",
                    blend,
                    depth_write,
                    key.format,
                    key.double_sided,
                    multisample,
                )
            })
        };
        let set = MaskSet {
            opaque: variant("lab mask decals", None, true),
            blend: variant(
                "lab mask decals blend",
                Some(wgpu::BlendState::ALPHA_BLENDING),
                false,
            ),
        };
        self.sets.insert(key, set.clone());
        set
    }
}

/// The `MASK_DECALS` forward module (alpha test compiled in, like the CPU-path forward module).
fn compile(d: &wgpu::Device, encode: bool, sun_rt: bool) -> wgpu::ShaderModule {
    let mut defs = HashMap::new();
    defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    defs.insert("MASK_DECALS".to_string(), ShaderDefValue::Bool(true));
    if !encode {
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    }
    super::add_sun_rt_def(&mut defs, sun_rt);
    // The engine's own shader with a fixed define set: `shader_check` validates this combination,
    // so a failure here is a build defect, as for the other forward variants.
    shader_module(
        d,
        "lab_forward_mask_decals",
        include_str!("../../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        defs,
    )
    .expect("MASK_DECALS forward variant")
}

/// One instance's GPU state.
struct MaskEntry {
    buf: wgpu::Buffer,
    bg: wgpu::BindGroup,
    uploaded: GpuMaskBlock,
    /// The textures `bg` binds (compared by `Arc` identity).
    textures: Arc<MaskDecalTextures>,
    /// [`MaskPipes::bgl`] id `bg` was built against.
    layout_id: u64,
    /// Shared-texture references `bg` holds, released with the entry.
    shares: Vec<ShareKey>,
    /// Private uploads `bg` binds (a shared image the device could only take decoded).
    _fixed: Vec<wgpu::Texture>,
}

/// Per-renderer state: the instance table and its GPU entries.
#[derive(Default)]
pub(crate) struct MaskDecalState {
    instances: HashMap<u64, MaskDecalInstance>,
    entries: HashMap<u64, MaskEntry>,
    /// Hash of the instance table (screen-space cache key; 0 without instances).
    content_key: u64,
    stats: MaskDecalStats,
}

impl MaskDecalState {
    /// Whether no instance is set (every draw takes the standard path).
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Whether `object_key` has an instance.
    pub fn has(&self, object_key: u64) -> bool {
        self.instances.contains_key(&object_key)
    }

    /// The bind group of `object_key`'s instance, once prepared.
    pub fn bind_group(&self, object_key: u64) -> Option<&wgpu::BindGroup> {
        self.entries.get(&object_key).map(|entry| &entry.bg)
    }

    /// Changes whenever any instance's values or textures change (0 without instances).
    pub fn content_key(&self) -> u64 {
        self.content_key
    }

    /// The instance table without GPU entries (a shader reload rebuilds the layouts).
    pub fn carry_instances(&mut self) -> Self {
        Self {
            instances: std::mem::take(&mut self.instances),
            content_key: self.content_key,
            ..Self::default()
        }
    }

    /// Starts a frame's counters.
    pub fn begin_frame(&mut self) {
        self.stats.masked_draws = 0;
        self.stats.fallback_draws = 0;
        self.stats.unsupported_draws = 0;
        self.stats.block_writes = 0;
    }

    /// Ends a frame's draw collection with its counts.
    pub fn note_draws(&mut self, masked: usize, fallback: usize, unsupported: usize) {
        self.stats.masked_draws = masked;
        self.stats.fallback_draws = fallback;
        self.stats.unsupported_draws = unsupported;
    }
}

impl FrameRenderer {
    /// Replaces the masked instances (material mask recolor and decal slots,
    /// [`crate::mask_decals`]) for subsequent frames. Instances are matched to draws by
    /// [`crate::FrameMesh::object_key`]; pass [`crate::ExtractedFrame::mask_decals`] every frame.
    /// An empty slice draws every mesh as without the feature. An instance that leaves the list
    /// releases its uniform block, bind group and texture references at once.
    pub fn set_mask_decals(&mut self, instances: &[MaskDecalInstance]) {
        use std::hash::{Hash, Hasher};
        let state = &mut self.mask_decals;
        state.instances.clear();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for instance in instances {
            instance.object_key.hash(&mut hasher);
            bytemuck::bytes_of(&instance.decals.pack()).hash(&mut hasher);
            (Arc::as_ptr(&instance.decals.textures) as usize).hash(&mut hasher);
            state.instances.insert(instance.object_key, instance.clone());
        }
        state.content_key = if instances.is_empty() {
            0
        } else {
            hasher.finish() | 1
        };
        state.stats.instances = state.instances.len();
        let gone: Vec<u64> = state
            .entries
            .keys()
            .filter(|key| !state.instances.contains_key(key))
            .copied()
            .collect();
        for key in gone {
            if let Some(entry) = self.mask_decals.entries.remove(&key) {
                for share in entry.shares {
                    self.shared_textures.release(share);
                }
            }
        }
        self.mask_decals.stats.gpu_entries = self.mask_decals.entries.len();
    }

    /// What the last frame did with masked instances.
    #[must_use]
    pub fn mask_decal_stats(&self) -> MaskDecalStats {
        self.mask_decals.stats
    }

    /// Ensures `object_key`'s uniform block and bind group, uploading the block when its values
    /// changed. `false` when the key has no instance or the pipelines do not exist yet.
    pub(super) fn prepare_mask_decal(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        object_key: u64,
        now: u64,
    ) -> bool {
        let Some(instance) = self.mask_decals.instances.get(&object_key) else {
            return false;
        };
        let Some(pipes) = self.pipes.as_mut() else {
            return false;
        };
        let block = instance.decals.pack();
        let textures = instance.decals.textures.clone();
        let (bgl, layout_id) = pipes.mask.bgl(d);
        let state = &mut self.mask_decals;
        if let Some(entry) = state.entries.get_mut(&object_key)
            && entry.layout_id == layout_id
            && Arc::ptr_eq(&entry.textures, &textures)
        {
            if entry.uploaded != block {
                self.belt.defer(d, &entry.buf, 0, bytemuck::bytes_of(&block));
                entry.uploaded = block;
                state.stats.block_writes += 1;
            }
            return true;
        }
        // New instance, new textures, or a rebuilt layout: (re)bind. The block buffer stays.
        let old = state.entries.remove(&object_key);
        let buf = match old.as_ref() {
            Some(entry) => entry.buf.clone(),
            None => {
                state.stats.blocks_created += 1;
                super::make_buffer(
                    d,
                    "mask decal block",
                    crate::mask_decals::BLOCK_BYTES as u64,
                    wgpu::BufferUsages::UNIFORM,
                )
            }
        };
        let env = UploadEnv {
            features: d.features(),
            material_key: object_key,
            name: None,
            now,
            device: d,
            queue: q,
        };
        let mut shares = Vec::new();
        let mut fixed = Vec::new();
        let mut bind = |image: Option<&crate::SharedImage>, slot: &'static str| {
            let bound = image.and_then(|image| {
                bind_material_image(
                    &mut self.shared_textures,
                    &env,
                    &MaterialImage::Shared(image.clone()),
                    slot,
                    // A shared image keeps its own color space.
                    false,
                )
            });
            match bound {
                Some(bound) => {
                    shares.extend(bound.share);
                    fixed.extend(bound.fixed.map(|(_, texture)| texture));
                    bound.view
                }
                // Not sampled: the header flag is off for a missing image.
                None => pipes.white_view.clone(),
            }
        };
        let mask_view = bind(textures.mask.as_ref(), "mask_decals.mask");
        let atlas_view = bind(textures.atlas.as_ref(), "mask_decals.atlas");
        let bg = crate::device::TrackedDevice::observe(d).create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mask decals bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: BLOCK_BINDING,
                    resource: buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: MASK_BINDING,
                    resource: wgpu::BindingResource::TextureView(&mask_view),
                },
                wgpu::BindGroupEntry {
                    binding: ATLAS_BINDING,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
            ],
        });
        self.belt.defer(d, &buf, 0, bytemuck::bytes_of(&block));
        state.stats.block_writes += 1;
        if let Some(old) = old {
            // Released after the new binds took their references: a shared image kept across
            // the rebind is not dropped and uploaded again.
            for share in old.shares {
                self.shared_textures.release(share);
            }
        }
        state.entries.insert(
            object_key,
            MaskEntry {
                buf,
                bg,
                uploaded: block,
                textures,
                layout_id,
                shares,
                _fixed: fixed,
            },
        );
        state.stats.gpu_entries = state.entries.len();
        true
    }
}
