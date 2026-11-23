// SPDX-License-Identifier: MPL-2.0
//! GPU side of the thin film ([`crate::thin_film`], ADR-1977): the `THIN_FILM` variant of the
//! forward shader and its group 3 (the material's uniform block and packed map).
//!
//! Only draws of a material with the film touch any of this. Every other draw keeps its pipelines,
//! bind groups and material block, so a frame without the film is unchanged. A filmed draw takes
//! the CPU colour loop (never the GPU-driven batches or the visibility buffer), as a masked
//! instance or a lightmap receiver does; its depth prepass and shadows stay as they were. Group 3
//! bindings 17-18 do not collide with the refraction (0-2, 7, 8), glass (3-6), mask-decal (9-11) or
//! lightmap (12-16) bindings declared in the same shader file.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use naga_oil::compose::ShaderDefValue;

use super::{
    Draw, FrameRenderer, GlassPipelineTarget, LabMaterial, PipelineKey, RenderError,
    lab_color_pipeline_entry, make_buffer, shader_module, storage_uniform, tex_entry,
};
use crate::RenderDevice;
use crate::pipeline_warm::DeferredPipeline;
use crate::thin_film::{THIN_FILM_BLOCK_BYTES, ThinFilmBlock, ThinFilmStats};

/// Group 3 binding of the material's film block (`film_block`).
const BLOCK_BINDING: u32 = 17;
/// Group 3 binding of the packed coverage and thickness map (`film_map`).
const MAP_BINDING: u32 = 18;

/// The `THIN_FILM` forward variants of one target configuration.
#[derive(Clone)]
pub(super) struct FilmSet {
    /// Opaque and alpha-tested colour (`fs_main`, depth equal to the prepass).
    opaque: DeferredPipeline<wgpu::RenderPipeline>,
    /// Alpha-blended colour.
    blend: DeferredPipeline<wgpu::RenderPipeline>,
    /// Premultiplied colour.
    premul: DeferredPipeline<wgpu::RenderPipeline>,
    /// Additive colour.
    add: DeferredPipeline<wgpu::RenderPipeline>,
}

impl FilmSet {
    /// The variant a draw of `alpha_mode` uses.
    fn for_alpha(&self, alpha_mode: u32) -> &DeferredPipeline<wgpu::RenderPipeline> {
        match alpha_mode {
            mode if mode < super::ALPHA_BLEND => &self.opaque,
            super::ALPHA_PREMULTIPLIED => &self.premul,
            super::ALPHA_ADDITIVE => &self.add,
            _ => &self.blend,
        }
    }
}

/// Group 3 layout, the pipeline layout and the variant sets (kept in [`super::Pipes`], so a
/// renderer that adopts warmed pipelines adopts these too).
#[derive(Default)]
pub(super) struct FilmPipes {
    /// The layout and a process-unique id: a binding built against another layout rebuilds.
    bgl: Option<(wgpu::BindGroupLayout, u64)>,
    layout: Option<wgpu::PipelineLayout>,
    /// `(encode, sun_rt)` → the module, composed by the first variant built.
    modules: HashMap<(bool, bool), Arc<OnceLock<wgpu::ShaderModule>>>,
    sets: HashMap<PipelineKey, FilmSet>,
}

fn next_layout_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl FilmPipes {
    /// Group 3 layout (created on first use) and its id.
    fn bgl(&mut self, d: &wgpu::Device) -> (wgpu::BindGroupLayout, u64) {
        self.bgl
            .get_or_insert_with(|| {
                let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("lab thin film bgl"),
                    entries: &[
                        storage_uniform(BLOCK_BINDING, wgpu::ShaderStages::FRAGMENT),
                        tex_entry(MAP_BINDING),
                    ],
                });
                (bgl, next_layout_id())
            })
            .clone()
    }

    /// Game-frame warms: the other forward module (ADR-2035) of every set, built where the set's
    /// own variant is. Returns how many sets it asked for.
    pub(super) fn warm_sun_twins(&mut self, d: &wgpu::Device, groups: [&wgpu::BindGroupLayout; 3]) -> usize {
        let twins: Vec<(PipelineKey, FilmSet)> = self
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
            for (drawn, twin) in [
                (&built.opaque, &set.opaque),
                (&built.blend, &set.blend),
                (&built.premul, &set.premul),
                (&built.add, &set.add),
            ] {
                if drawn.is_built() {
                    twin.get();
                }
            }
        }
        asked
    }

    /// The variants for `key`. Creating them registers background builds (no compile here while
    /// warm workers run; without workers they build now, like every deferred variant).
    fn set(&mut self, d: &wgpu::Device, groups: [&wgpu::BindGroupLayout; 3], key: PipelineKey) -> FilmSet {
        // The film module is keyed by (encode, sun_rt) only and never reads the studio axis
        // (ADR-2308), so both `studio_off` values build the same pipelines: cache them under one
        // key, or a frame whose studio is dormant misses the set `prewarm_thin_film` built.
        let key = PipelineKey {
            studio_off: false,
            restir_di: false,
            ..key
        };
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
                    label: Some("lab thin film layout"),
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
        let add = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        let set = FilmSet {
            opaque: variant("lab thin film", None, true),
            blend: variant(
                "lab thin film blend",
                Some(wgpu::BlendState::ALPHA_BLENDING),
                false,
            ),
            premul: variant(
                "lab thin film premul",
                Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                false,
            ),
            add: variant(
                "lab thin film additive",
                Some(wgpu::BlendState {
                    color: add,
                    alpha: add,
                }),
                false,
            ),
        };
        self.sets.insert(key, set.clone());
        set
    }
}

/// The `THIN_FILM` forward module (alpha test compiled in, like the CPU-path forward module).
fn compile(d: &wgpu::Device, encode: bool, sun_rt: bool) -> wgpu::ShaderModule {
    let mut defs = HashMap::new();
    defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    defs.insert("THIN_FILM".to_string(), ShaderDefValue::Bool(true));
    if !encode {
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    }
    super::add_sun_rt_def(&mut defs, sun_rt);
    // The engine's own shader with a fixed define set: `shader_check` validates this combination,
    // so a failure here is a build defect, as for the other forward variants.
    shader_module(
        d,
        "lab_forward_thin_film",
        include_str!("../../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        defs,
    )
    .expect("THIN_FILM forward variant")
}

/// A filmed material's group 3. Held by its [`super::MaterialEntry`], so it goes with the entry;
/// the map's texture sits in the entry's own `fixed` and `shares` lists.
pub(super) struct FilmBinding {
    buf: wgpu::Buffer,
    bg: wgpu::BindGroup,
    uploaded: ThinFilmBlock,
    /// [`FilmPipes::bgl`] id `bg` was built against.
    layout_id: u64,
}

impl FilmBinding {
    /// The material's group 3.
    pub(super) fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bg
    }
}

/// The frame's counts.
#[derive(Default)]
pub(crate) struct ThinFilmState {
    /// Draws of a filmed material the variant cannot shade, this frame.
    unsupported: usize,
    stats: ThinFilmStats,
}

impl ThinFilmState {
    /// Starts a frame's draw list.
    pub(super) fn begin_frame(&mut self) {
        self.unsupported = 0;
    }

    /// Counts a draw of a filmed material the variant cannot shade.
    pub(super) fn note_unsupported(&mut self) {
        self.unsupported += 1;
    }
}

/// The frame's `THIN_FILM` variants, cull-on and cull-off.
pub(super) struct FilmFrame {
    sets: [Option<FilmSet>; 2],
}

impl FilmFrame {
    /// The built pipeline of a filmed draw.
    pub(super) fn pipeline(&self, cull_off: bool, alpha_mode: u32) -> Option<&wgpu::RenderPipeline> {
        self.sets[usize::from(cull_off)]
            .as_ref()?
            .for_alpha(alpha_mode)
            .try_get()
    }
}

/// The `THIN_FILM` variants of the frame's filmed draws, resolved like the mask-decal variants:
/// built here where this thread may wait; mid-play a draw whose variant is not built yet keeps the
/// base Fresnel this frame (its `film` is cleared). Records the frame's [`ThinFilmStats`].
pub(super) fn resolve_film_sets(
    pipes: &mut FilmPipes,
    groups: [&wgpu::BindGroupLayout; 3],
    state: &mut ThinFilmState,
    d: &wgpu::Device,
    (key, key_ds): (PipelineKey, PipelineKey),
    draws: &mut [Draw],
) -> FilmFrame {
    let mut frame = FilmFrame { sets: [None, None] };
    state.stats.unsupported = state.unsupported;
    state.stats.drawn = 0;
    state.stats.fallback = 0;
    if !draws.iter().any(|draw| draw.film) {
        return frame;
    }
    let _span = tracing::info_span!("thin_film_resolve").entered();
    let _crumb = crate::breadcrumbs::frame_pass("thin_film_resolve");
    frame.sets[0] = Some(pipes.set(d, groups, key));
    if draws.iter().any(|draw| draw.film && draw.cull_off) {
        frame.sets[1] = Some(pipes.set(d, groups, key_ds));
    }
    let variant = |draw: &Draw| -> Option<DeferredPipeline<wgpu::RenderPipeline>> {
        let set = frame.sets[usize::from(draw.cull_off)].as_ref()?;
        Some(set.for_alpha(draw.alpha_mode).clone())
    };
    if crate::pipeline_warm::may_block() {
        let mut need: Vec<DeferredPipeline<wgpu::RenderPipeline>> = Vec::new();
        for v in draws.iter().filter(|draw| draw.film).filter_map(variant) {
            if !v.is_built() && !need.iter().any(|n| n.same(&v)) {
                need.push(v);
            }
        }
        let jobs: Vec<_> = need
            .into_iter()
            .map(|p| {
                move || {
                    p.get();
                }
            })
            .collect();
        crate::pipeline_warm::join_all(jobs);
    }
    let (mut drawn, mut fallback) = (0, 0);
    for draw in draws.iter_mut().filter(|draw| draw.film) {
        if variant(draw).is_none_or(|v| v.try_get().is_none()) {
            draw.film = false;
            if draw.visible {
                fallback += 1;
            }
        } else if draw.visible {
            drawn += 1;
        }
    }
    state.stats.drawn = drawn;
    state.stats.fallback = fallback;
    frame
}

impl FrameRenderer {
    /// What the forward pass did with thin-film materials in the last frame.
    #[must_use]
    pub fn thin_film_stats(&self) -> ThinFilmStats {
        ThinFilmStats {
            materials: self.materials.values().filter(|e| e.film.is_some()).count(),
            ..self.thin_film.stats
        }
    }

    /// Ensures the group 3 of the filmed material `key` (its entry already exists): the block
    /// rewritten when the values changed, the binding built once per layout. `false` when the
    /// pipelines or the entry do not exist.
    pub(super) fn prepare_thin_film(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        key: u64,
        mat: &LabMaterial,
        now: u64,
    ) -> bool {
        let Some(pipes) = self.pipes.as_mut() else {
            return false;
        };
        let (bgl, layout_id) = pipes.film.bgl(d);
        let Some(entry) = self.materials.get_mut(&key) else {
            return false;
        };
        let block = mat.thin_film.pack();
        if let Some(binding) = entry.film.as_mut()
            && binding.layout_id == layout_id
        {
            if binding.uploaded != block {
                self.belt.defer(d, &binding.buf, 0, bytemuck::bytes_of(&block));
                binding.uploaded = block;
            }
            return true;
        }
        let _span = tracing::info_span!("thin_film_upload").entered();
        let env = crate::texture_residency::UploadEnv {
            features: d.features(),
            material_key: key,
            name: entry.name.as_ref(),
            now,
            device: d,
            queue: q,
        };
        let view = super::materials::keep_slot(
            crate::texture_residency::bind_packed_slot(
                &mut self.shared_textures,
                &env,
                crate::texture_residency::PackedSource {
                    image: None,
                    compressed: None,
                    rgba: mat.thin_film_map.as_ref(),
                    slot: "thin_film",
                    // White: full coverage at the maximum thickness, the extension's no-texture rule.
                    fallback: &pipes.white_view,
                },
            ),
            &mut entry.fixed,
            &mut entry.shares,
        );
        let buf = entry.film.take().map_or_else(
            || {
                make_buffer(
                    d,
                    "thin film block",
                    THIN_FILM_BLOCK_BYTES,
                    wgpu::BufferUsages::UNIFORM,
                )
            },
            |old| old.buf,
        );
        let bg = crate::device::TrackedDevice::observe(d).create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("thin film bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: BLOCK_BINDING,
                    resource: buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: MAP_BINDING,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
            ],
        });
        self.belt.defer(d, &buf, 0, bytemuck::bytes_of(&block));
        entry.film = Some(FilmBinding {
            buf,
            bg,
            uploaded: block,
            layout_id,
        });
        true
    }

    /// Builds the `THIN_FILM` pipelines `meshes` draw with for `target`, so a level's filmed
    /// material does not fall back to the base Fresnel on its first frames. [`Self::prewarm`]
    /// calls this for the HDR targets.
    ///
    /// # Errors
    ///
    /// The warm queue running this shut down ([`crate::warm_cancelled`]).
    pub(super) fn prewarm_thin_film(
        &mut self,
        device: &RenderDevice,
        target: GlassPipelineTarget,
        materials: &[&LabMaterial],
    ) -> Result<(), RenderError> {
        let d = &device.device;
        let Some(pipes) = self.pipes.as_mut() else {
            return Ok(());
        };
        let samples = if target.samples >= 4 { 4 } else { 1 };
        for mat in materials {
            let key = PipelineKey {
                format: target.format,
                encode: target.encode,
                double_sided: mat.double_sided,
                samples,
                sun_rt: self.culler.forward_sun_rt(),
                studio_off: false,
                restir_di: false,
            };
            let groups = [&pipes.view_bgl, &pipes.object_bgl, &pipes.mat_bgl];
            let set = pipes.film.set(d, groups, key);
            crate::pipeline_warm::warm_checkpoint()?;
            set.for_alpha(mat.alpha_mode).get();
        }
        Ok(())
    }
}
