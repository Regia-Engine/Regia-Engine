// SPDX-License-Identifier: MPL-2.0
//! Pipeline construction for the frame renderer: shader modules, the per-format forward
//! pipeline set and the lab colour pipelines (moved out of `frame.rs` unchanged, ADR-1552).

use super::*;

pub(super) fn shader_module(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    file: &str,
    mut defs: HashMap<String, ShaderDefValue>,
) -> Result<wgpu::ShaderModule, RenderError> {
    // The material binding matches this device's material layout (`bindless`).
    crate::bindless::add_material_defs(device, &mut defs);
    let module = compose_source(source, file, defs)?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
    }))
}

/// Anisotropy clamp of the material sampler (trilinear + 8× anisotropic).
pub(super) const MATERIAL_ANISOTROPY: u16 = 8;

/// [`crate::device_pipelines::DevicePipelines`] key of a device-level pipeline of the frame
/// renderer: its name and the shader epoch (a committed dev shader edit builds new ones).
pub(crate) fn shared_pipes_key(name: &str) -> u64 {
    crate::device_pipelines::pipeline_key(&(name, crate::shader_reload::shader_epoch()))
}

pub(super) fn build_pipes(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    shared: &crate::device_pipelines::DevicePipelines,
    anisotropy: u16,
) -> Result<Pipes, RenderError> {
    let d = &crate::device::TrackedDevice::observe(d);
    let mut forward_defs = HashMap::new();
    // One module draws both opaque and mask on the CPU path. The GPU opaque
    // pipeline is a separate compile without this def, so early-Z stays on.
    forward_defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    let forward_mod = shader_module(
        d,
        "lab_forward",
        include_str!("../../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        forward_defs,
    )?;
    let shadow_mod = shader_module(
        d,
        "lab_shadow",
        include_str!("../../shaders/lab_shadow.wgsl"),
        "lab_shadow.wgsl",
        HashMap::new(),
    )?;
    let cluster_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("cluster bgl"),
        entries: &[
            storage_uniform(0, wgpu::ShaderStages::COMPUTE),
            storage_entry(1, wgpu::ShaderStages::COMPUTE, true),
            storage_entry(2, wgpu::ShaderStages::COMPUTE, false),
            storage_entry(3, wgpu::ShaderStages::COMPUTE, false),
        ],
    });
    let mut view_entries = vec![
        storage_uniform(0, wgpu::ShaderStages::VERTEX_FRAGMENT),
        storage_uniform(1, wgpu::ShaderStages::FRAGMENT),
        storage_entry(2, wgpu::ShaderStages::FRAGMENT, true),
        storage_entry(3, wgpu::ShaderStages::FRAGMENT, true),
        storage_entry(4, wgpu::ShaderStages::FRAGMENT, true),
        depth_tex_entry(5),
        depth_tex_entry(6),
        compare_samp_entry(7),
        storage_uniform(8, wgpu::ShaderStages::FRAGMENT),
        depth_tex_entry(9),
        storage_entry(10, wgpu::ShaderStages::FRAGMENT, true),
        tex_entry(11),
        tex_entry(12),
        tex_entry(13),
        samp_entry(14),
        depth_tex_entry(15),
        depth_tex_entry(16),
        // SSAO (layer 0) and the bent normals (layer 1, ADR-1375): a 2D array.
        tex_array_entry(17),
        // Non-filtering: the sun cascades gather depth through it (`plane_quad_lit`).
        point_samp_entry(18),
        storage_entry(19, wgpu::ShaderStages::FRAGMENT, true),
        storage_uniform(20, wgpu::ShaderStages::FRAGMENT),
        storage_entry(21, wgpu::ShaderStages::FRAGMENT, true),
        tex_entry(22),
        storage_uniform(23, wgpu::ShaderStages::FRAGMENT),
        storage_uniform(24, wgpu::ShaderStages::FRAGMENT),
        storage_entry(25, wgpu::ShaderStages::FRAGMENT, true),
        // Translucent shadows (`translucent_shadow`): cascade transmittance and depth
        // arrays, the atlas pair, and their linear clamp sampler.
        tex_array_entry(26),
        depth_tex_array_entry(27),
        tex_entry(28),
        depth_tex_entry(29),
        samp_entry(30),
        storage_uniform(31, wgpu::ShaderStages::FRAGMENT),
        // GGX LTC table (`ltc`): Rgba32Float, read with `textureLoad`. Bindings 32/33: 22 is
        // the contact shadows', 23 the mesh occluders', 24/25 the irradiance probes', 26-31 the
        // translucent shadows'.
        wgpu::BindGroupLayoutEntry {
            binding: 32,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        // Emissive lights' distance probes (`emissive_lights`): R32Float, `textureLoad`.
        wgpu::BindGroupLayoutEntry {
            binding: 33,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        // Cloud transmittance map (`outdoor`, ADR-320): R16Float, `textureLoad`; white without one.
        wgpu::BindGroupLayoutEntry {
            binding: 34,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        // Local reflection captures and the studio background image (ADR-2000): a 1×1
        // stand-in until a frame has one.
        tex_array_entry(35),
        tex_entry(36),
    ];
    // The many-light reservoirs (ADR-2652): read by the `RESTIR_DI` opaque permutation only, a
    // stand-in buffer elsewhere. Devices without room for it keep the layout they had.
    let many_light_binding = many_light::binding_supported(&d.limits());
    if many_light_binding {
        view_entries.push(storage_entry(37, wgpu::ShaderStages::FRAGMENT, true));
    }
    // The blue-noise table (ADR-2652): every forward module of such a device declares it
    // (`FRAME_BLUE_NOISE`); a 1×1 stand-in while a frame does not use it.
    let blue_noise_binding = crate::blue_noise::forward_binding_supported(&d.limits());
    if blue_noise_binding {
        view_entries.push(tex_array_entry(38));
    }
    // The forward decals' storage buffer (ADR-3255): process switch on and room on the device.
    let forward_decal_binding = crate::forward_decals::binding_supported(&d.limits());
    if forward_decal_binding {
        view_entries.push(crate::forward_decals::layout_entry());
    }
    let view_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("lab view bgl"),
        entries: &view_entries,
    });
    let many_light_stand_in = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("many light stand-in"),
        size: 16,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let object_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("lab object bgl"),
        entries: &[
            storage_uniform(0, wgpu::ShaderStages::VERTEX),
            storage_entry(1, wgpu::ShaderStages::VERTEX, true),
        ],
    });
    // Vertex-visible too: a material graph's vertex stage (world position offset) reads its
    // parameters and may fetch textures at an explicit level.
    let material_stage = |mut entry: wgpu::BindGroupLayoutEntry| {
        entry.visibility = wgpu::ShaderStages::VERTEX_FRAGMENT;
        entry
    };
    // Binding 0: the material constants, a uniform buffer or (tier `MaterialSsbo`) a read-only
    // storage buffer record (`bindless`, ADR-316); the modules get the matching define.
    let material_ssbo = crate::bindless::material_ssbo(d);
    let graph_mat_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("lab material bgl"),
        entries: &[
            crate::bindless::material_constants_entry(material_ssbo),
            material_stage(tex_entry(1)),
            material_stage(samp_entry(2)),
            material_stage(tex_entry(3)),
            material_stage(tex_entry(4)),
            material_stage(tex_entry(5)),
            material_stage(tex_entry(6)),
            material_stage(tex_entry(7)),
            material_stage(tex_entry(8)),
        ],
    });
    // Texture table path (ADR-863): the standard material group is the arena record at a dynamic
    // offset (the texture indices ride in the record), one group for every standard material,
    // and every standard pipeline binds the table as group 4.
    let table_bgl = crate::texture_table::table_layout(d);
    let mat_bgl = if table_bgl.is_some() {
        d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lab material record bgl"),
            entries: &[crate::bindless::material_record_entry(TABLE_RECORD_BYTES)],
        })
    } else {
        graph_mat_bgl.clone()
    };
    let shadow_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("shadow vp bgl"),
        entries: &[storage_uniform(0, wgpu::ShaderStages::VERTEX)],
    });
    let forward_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("lab forward layout"),
        bind_group_layouts: &crate::texture_table::with_table(
            &[Some(&view_bgl), Some(&object_bgl), Some(&mat_bgl)],
            table_bgl.as_ref(),
        ),
        immediate_size: 0,
    });
    let graph_forward_layout = if table_bgl.is_some() {
        d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("graph forward layout"),
            bind_group_layouts: &[Some(&view_bgl), Some(&object_bgl), Some(&graph_mat_bgl)],
            immediate_size: 0,
        })
    } else {
        forward_layout.clone()
    };
    let refract_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("lab refraction bgl"),
        // 7 / 8: the glass front layer's depth and transmittance (`front_layer_weight`).
        entries: &[
            tex_entry(0),
            depth_tex_entry(1),
            samp_entry(2),
            depth_tex_entry(7),
            tex_entry(8),
        ],
    });
    let refract_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("lab refraction layout"),
        bind_group_layouts: &crate::texture_table::with_table(
            &[
                Some(&view_bgl),
                Some(&object_bgl),
                Some(&mat_bgl),
                Some(&refract_bgl),
            ],
            table_bgl.as_ref(),
        ),
        immediate_size: 0,
    });
    let refract_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("lab refraction sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let shadow_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("lab shadow layout"),
        bind_group_layouts: &[Some(&shadow_bgl), Some(&object_bgl)],
        immediate_size: 0,
    });
    // The pipelines this function builds itself (the cluster build, the page clear and the
    // translucent casters) belong to the device (ADR-220): a renderer built after the warm (the
    // stand-in a game-frame warm leaves, whose graph prewarm or level-load prewarm builds its
    // pipes on the host thread; a second renderer) takes them instead of compiling them again.
    // Bind group layouts with equal entries are one layout on a device, so this renderer's
    // groups bind to pipelines built against another renderer's layouts (ADR-195).
    let cluster_pipeline = shared
        .get_or_create(shared_pipes_key("lab cluster pipeline"), || {
            let module = shader_module(
                d,
                "cluster_build",
                include_str!("../../shaders/cluster_build.wgsl"),
                "cluster_build.wgsl",
                HashMap::new(),
            )
            .map_err(|e| e.to_string())?;
            let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("cluster layout"),
                bind_group_layouts: &[Some(&cluster_bgl)],
                immediate_size: 0,
            });
            Ok::<_, String>(d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("cluster pipeline"),
                layout: Some(&layout),
                module: &module,
                entry_point: Some("cs_main"),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(&d).as_deref(),
            }))
        })
        .map_err(RenderError::Shader)?;
    let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("lab sampler"),
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        // Trilinear and anisotropic over the material mip chains (`upload_material_texture`).
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        anisotropy_clamp: anisotropy,
        ..Default::default()
    });
    // Once per device and clamp: the probe submits on the raw queue, so no frame counter moves.
    let material_lod_offset = shared.get_or_create(
        crate::device_pipelines::pipeline_key(&("material aniso lod offset", anisotropy)),
        || crate::material_aniso_calibration::material_anisotropy_lod_offset(d, q.untracked(), anisotropy),
    );
    let (white_tex, white_view) = solid_rgba(d, q, [255, 255, 255, 255]);
    let white_array_view = white_tex.create_view(&wgpu::TextureViewDescriptor {
        label: Some("white array"),
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    let (_flat_tex, flat_view) = solid_rgba(d, q, [128, 128, 255, 255]);
    let (_mid_tex, mid_view) = solid_rgba(d, q, [128, 128, 128, 128]);
    let (_aux_tex, aux_view) = solid_rgba(d, q, [255, 128, 0, 255]);
    let translucent = shared
        .get_or_create(shared_pipes_key("lab translucent casters"), || {
            translucent_shadow::TranslucentPipes::new(d, &object_bgl, &mat_bgl, &graph_mat_bgl)
                .map_err(|e| e.to_string())
        })
        .map_err(RenderError::Shader)?;
    let page_clear = shared
        .get_or_create(shared_pipes_key("lab page clear"), || {
            let module = shader_module(
                d,
                "page clear",
                include_str!("../../shaders/page_clear.wgsl"),
                "page_clear.wgsl",
                HashMap::new(),
            )
            .map_err(|e| e.to_string())?;
            let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("page clear"),
                bind_group_layouts: &[],
                immediate_size: 0,
            });
            Ok::<_, String>(d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("page clear"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: None,
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: crate::pipeline_cache::cache_for(&d).as_deref(),
            }))
        })
        .map_err(RenderError::Shader)?;
    Ok(Pipes {
        forward_mod,
        shadow_mod,
        view_bgl,
        many_light_binding,
        many_light_stand_in,
        blue_noise_binding,
        forward_decals: forward_decal_binding.then(|| crate::forward_decals::ForwardDecalBindings::new(d)),
        cluster_bgl,
        object_bgl,
        mat_bgl,
        graph_mat_bgl,
        table_bgl,
        graph_forward_layout,
        material_ssbo,
        shadow_bgl,
        forward_layout,
        refract_bgl,
        refract_layout,
        refract_sampler,
        glass: glass_gpu::GlassPipes::default(),
        mask: mask_decals_gpu::MaskPipes::default(),
        lightmap: lightmap_forward::LightmapPipes::default(),
        film: thin_film_gpu::FilmPipes::default(),
        subsurface: subsurface_gpu::SubsurfacePipes::default(),
        oit: oit_gpu::OitPipes::default(),
        shadow_layout,
        cluster_pipeline,
        morph: morph_gpu::MorphPipes::default(),
        sampler,
        material_lod_offset,
        white_view,
        white_array_view,
        flat_view,
        mid_view,
        aux_view,
        page_clear,
        translucent,
        sets: HashMap::new(),
        lite: forward_lite_gpu::LiteCache::default(),
    })
}

/// Adds the ray-traced sun's forward def when `sun_rt` ([`PipelineKey::sun_rt`], ADR-2035). A key
/// without the bit composes the module it always did.
pub(super) fn add_sun_rt_def(defs: &mut HashMap<String, ShaderDefValue>, sun_rt: bool) {
    if sun_rt {
        crate::hwrt::SunShadowVariant::RayQuery.add_forward_defs(defs);
    }
}

/// The forward pipeline set of `key`: its colour format, encode, faces, sample count and forward
/// module (the ray-traced sun's, ADR-2035; the studio-off one, ADR-2308).
pub(super) fn build_pipeline_set(d: &wgpu::Device, pipes: &Pipes, key: PipelineKey) -> PipelineSet {
    // The after-warm-up report names which key axis this renderer had no set for (ADR-7885).
    let _axes = crate::frame_stats::key_axes_scope(new_key_axes(pipes, key));
    let PipelineKey {
        format,
        encode,
        double_sided,
        samples,
        sun_rt,
        studio_off,
        restir_di,
    } = key;
    let d = &crate::device::TrackedDevice::observe(d);
    if restir_di {
        // The plain twin's set with the many-light opaque colour pipeline (ADR-2652): the blended,
        // x-ray and transmission pipelines are the twin's own (deferred) objects, so a blended
        // draw that the twin already built draws under either key.
        let plain = pipes
            .sets
            .get(&key.plain())
            .cloned()
            .unwrap_or_else(|| build_pipeline_set(d, pipes, key.plain()));
        return PipelineSet {
            color: restir_color_pipeline(d, pipes, key),
            ..plain
        };
    }
    let multisample = wgpu::MultisampleState {
        count: samples,
        mask: u64::MAX,
        alpha_to_coverage_enabled: false,
    };
    let depth_state = |write: bool| wgpu::DepthStencilState {
        format: wgpu::TextureFormat::Depth32Float,
        depth_write_enabled: Some(write),
        depth_compare: Some(wgpu::CompareFunction::LessEqual),
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState::default(),
    };
    let primitive = wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        cull_mode: if double_sided {
            None
        } else {
            Some(wgpu::Face::Back)
        },
        ..Default::default()
    };
    let mut defs = HashMap::new();
    defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    if !encode {
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    }
    add_sun_rt_def(&mut defs, sun_rt);
    crate::forward_permutations::add_studio_off_def(&mut defs, studio_off);
    let module = if encode && !sun_rt && !studio_off {
        pipes.forward_mod.clone()
    } else {
        // A linear-output variant of the same shader (sRGB targets encode in hardware), the
        // ray-traced sun's (ADR-2035), or the studio-off one (ADR-2308).
        shader_module(
            d,
            "lab_forward_linear",
            include_str!("../../shaders/lab_forward.wgsl"),
            "lab_forward.wgsl",
            defs,
        )
        .expect("forward linear variant")
    };
    // The opaque colour pipeline is built now; the blend modes and the glass transmission
    // passes are deferred (`pipeline_warm::DeferredPipeline`): built where a frame first draws
    // them, or in the background after the first game frame. A cold boot compiles only the
    // variants its frames draw; the uber shader costs ~0.4–1 s per variant on a cold NVIDIA
    // driver.
    let deferred = |label: &'static str,
                    layout: &wgpu::PipelineLayout,
                    entry: &'static str,
                    blend: Option<wgpu::BlendState>,
                    depth_write: bool,
                    target: wgpu::TextureFormat| {
        let (device, layout, module) = (wgpu::Device::clone(d), layout.clone(), module.clone());
        let axes = crate::frame_stats::current_key_axes();
        crate::pipeline_warm::DeferredPipeline::new(label, move || {
            let _axes = crate::frame_stats::key_axes_scope(axes);
            lab_color_pipeline_entry(
                &device,
                &layout,
                &module,
                label,
                entry,
                blend,
                depth_write,
                target,
                double_sided,
                multisample,
            )
        })
    };
    let additive = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
    };
    let fl = &pipes.forward_layout;
    let blend_pipeline = deferred(
        "lab blend",
        fl,
        "fs_main",
        Some(wgpu::BlendState::ALPHA_BLENDING),
        false,
        format,
    );
    let premul_pipeline = deferred(
        "lab premul",
        fl,
        "fs_main",
        Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        false,
        format,
    );
    let add_pipeline = deferred("lab additive", fl, "fs_main", Some(additive), false, format);
    let xray_pipeline = {
        let (device, layout, module) = (wgpu::Device::clone(d), fl.clone(), module.clone());
        // Built on first use only: most frames never draw an x-ray override.
        crate::pipeline_warm::DeferredPipeline::on_demand("lab x-ray", move || {
            lab_color_pipeline_compare(
                &device,
                &layout,
                &module,
                "lab x-ray",
                "fs_main",
                Some(wgpu::BlendState::ALPHA_BLENDING),
                false,
                format,
                double_sided,
                multisample,
                wgpu::CompareFunction::Always,
            )
        })
    };
    // Tinted transmission is two blends of the same draw (no dual-source blending needed):
    // `dst ×= transmittance`, then `dst += reflection + remaining diffuse`. Both keep the
    // target's alpha (the HDR target carries the SSR roughness there).
    let keep_alpha = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    };
    let add_keep_alpha = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: keep_alpha,
    };
    let transmit_mul_pipeline = deferred(
        "lab transmit multiply",
        fl,
        "fs_transmit",
        Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Dst,
                dst_factor: wgpu::BlendFactor::Zero,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: keep_alpha,
        }),
        false,
        format,
    );
    let transmit_add_pipeline = deferred(
        "lab transmit add",
        fl,
        "fs_main",
        Some(add_keep_alpha),
        false,
        format,
    );
    let transmit_add_depth_pipeline = deferred(
        "lab transmit add (depth)",
        &pipes.refract_layout,
        "fs_transmit_add",
        Some(add_keep_alpha),
        false,
        format,
    );
    let transmit_front_pipeline = deferred(
        "lab transmit front layer",
        fl,
        "fs_transmit",
        None,
        true,
        FRONT_LAYER_FORMAT,
    );
    let transmit_refract_pipeline = deferred(
        "lab transmit refract",
        &pipes.refract_layout,
        "fs_refract",
        Some(add_keep_alpha),
        false,
        format,
    );
    let color = || {
        lab_color_pipeline(
            d,
            &pipes.forward_layout,
            &module,
            "lab forward",
            None,
            true,
            format,
            double_sided,
            multisample,
        )
    };
    // The opaque colour compile runs beside the depth, shadow and sky ones (`pipeline_warm::join`).
    let rest = || {
        let depth_pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lab depth"),
            layout: Some(&pipes.forward_layout),
            vertex: wgpu::VertexState {
                module: &pipes.forward_mod,
                entry_point: Some("vs_main"),
                buffers: &[Some(LabVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &pipes.forward_mod,
                entry_point: Some("fs_depth"),
                targets: &[],
                compilation_options: Default::default(),
            }),
            primitive,
            depth_stencil: Some(depth_state(true)),
            multisample,
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        let shadow_pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lab shadow"),
            layout: Some(&pipes.shadow_layout),
            vertex: wgpu::VertexState {
                module: &pipes.shadow_mod,
                entry_point: Some("vs_main"),
                buffers: &[Some(LabVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &pipes.shadow_mod,
                entry_point: Some("fs_main"),
                targets: &[],
                compilation_options: Default::default(),
            }),
            primitive,
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                // No pipeline depth bias: Vulkan's slope-scaled bias is in
                // implementation-dependent units, so the same scene self-shadowed on
                // llvmpipe but rendered clean on NVIDIA. All shadow biasing is done
                // receiver-side in WGSL (`biased_shadow_depth`), deterministically.
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        // The sky reads only the view group, so its layout is that one group: it can be
        // drawn without object/material bind groups, and group 0 stays bound across it.
        let sky_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lab sky layout"),
            bind_group_layouts: &[Some(&pipes.view_bgl)],
            immediate_size: 0,
        });
        let sky_pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lab sky"),
            layout: Some(&sky_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_sky"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_sky"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(depth_state(false)),
            multisample,
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        (depth_pipeline, shadow_pipeline, sky_pipeline)
    };
    let (color_pipeline, (depth_pipeline, shadow_pipeline, sky_pipeline)) =
        crate::pipeline_warm::join(color, rest);
    PipelineSet {
        depth: depth_pipeline,
        color: color_pipeline,
        blend: blend_pipeline,
        premul: premul_pipeline,
        add: add_pipeline,
        xray: xray_pipeline,
        transmit_mul: transmit_mul_pipeline,
        transmit_add: transmit_add_pipeline,
        transmit_add_depth: transmit_add_depth_pipeline,
        transmit_front: transmit_front_pipeline,
        transmit_refract: transmit_refract_pipeline,
        shadow: shadow_pipeline,
        sky: sky_pipeline,
    }
}

/// The axes of `key` that no set in `pipes` has yet ([`crate::KeyAxes`]; ADR-7885): a new
/// colour format or sample count is a target the warm never drew into.
pub(super) fn new_key_axes(pipes: &Pipes, key: PipelineKey) -> crate::KeyAxes {
    use crate::KeyAxes;
    if pipes.sets.is_empty() {
        return KeyAxes::FIRST;
    }
    type Same = fn(&PipelineKey, &PipelineKey) -> bool;
    let checks: [(KeyAxes, Same); 7] = [
        (KeyAxes::FORMAT, |a, b| a.format == b.format),
        (KeyAxes::SAMPLES, |a, b| a.samples == b.samples),
        (KeyAxes::ENCODE, |a, b| a.encode == b.encode),
        (KeyAxes::DOUBLE_SIDED, |a, b| a.double_sided == b.double_sided),
        (KeyAxes::SUN_RT, |a, b| a.sun_rt == b.sun_rt),
        (KeyAxes::STUDIO_OFF, |a, b| a.studio_off == b.studio_off),
        (KeyAxes::RESTIR_DI, |a, b| a.restir_di == b.restir_di),
    ];
    let mut axes = KeyAxes::NONE;
    for (axis, same) in checks {
        if !pipes.sets.keys().any(|known| same(known, &key)) {
            axes = axes.union(axis);
        }
    }
    axes
}

#[allow(clippy::too_many_arguments)] // one pipeline's whole state
pub(super) fn lab_color_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    label: &'static str,
    blend: Option<wgpu::BlendState>,
    depth_write: bool,
    format: wgpu::TextureFormat,
    double_sided: bool,
    multisample: wgpu::MultisampleState,
) -> wgpu::RenderPipeline {
    lab_color_pipeline_entry(
        device,
        layout,
        module,
        label,
        "fs_main",
        blend,
        depth_write,
        format,
        double_sided,
        multisample,
    )
}

/// [`lab_color_pipeline`] with a chosen fragment entry point.
#[allow(clippy::too_many_arguments)] // one pipeline's whole state
pub(super) fn lab_color_pipeline_entry(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    label: &'static str,
    entry: &'static str,
    blend: Option<wgpu::BlendState>,
    depth_write: bool,
    format: wgpu::TextureFormat,
    double_sided: bool,
    multisample: wgpu::MultisampleState,
) -> wgpu::RenderPipeline {
    lab_color_pipeline_compare(
        device,
        layout,
        module,
        label,
        entry,
        blend,
        depth_write,
        format,
        double_sided,
        multisample,
        wgpu::CompareFunction::LessEqual,
    )
}

/// [`lab_color_pipeline_entry`] with a chosen depth test (x-ray overrides draw with `Always`).
#[allow(clippy::too_many_arguments)] // one pipeline's whole state
pub(super) fn lab_color_pipeline_compare(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    label: &'static str,
    entry: &'static str,
    blend: Option<wgpu::BlendState>,
    depth_write: bool,
    format: wgpu::TextureFormat,
    double_sided: bool,
    multisample: wgpu::MultisampleState,
    depth_compare: wgpu::CompareFunction,
) -> wgpu::RenderPipeline {
    let device = &crate::device::TrackedDevice::observe(device);
    // The HDR target's alpha carries the opaque surface's reflection inputs (`sssr`): a blended
    // draw keeps it instead of blending its coverage in, which would corrupt the code.
    let blend = blend.map(|state| {
        if format == wgpu::TextureFormat::Rgba16Float {
            wgpu::BlendState {
                color: state.color,
                alpha: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::Zero,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
            }
        } else {
            state
        }
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            buffers: &[Some(LabVertex::layout())],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(entry),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: if double_sided {
                None
            } else {
                Some(wgpu::Face::Back)
            },
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(depth_write),
            depth_compare: Some(depth_compare),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample,
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    })
}

/// The opaque colour pipeline of a many-light key (ADR-2652): the forward module of `key`'s
/// plain twin with `RESTIR_DI`, which shades the pixel's reservoir light in place of the list's
/// points and spots.
fn restir_color_pipeline(
    d: &crate::device::TrackedDevice,
    pipes: &Pipes,
    key: PipelineKey,
) -> wgpu::RenderPipeline {
    let mut defs = HashMap::new();
    defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    if !key.encode {
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    }
    add_sun_rt_def(&mut defs, key.sun_rt);
    crate::forward_permutations::add_studio_off_def(&mut defs, key.studio_off);
    crate::forward_permutations::add_restir_di_def(&mut defs, true);
    let module = shader_module(
        d,
        "lab_forward_restir_di",
        include_str!("../../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        defs,
    )
    .expect("forward many-light variant");
    lab_color_pipeline(
        d,
        &pipes.forward_layout,
        &module,
        "lab forward (many lights)",
        None,
        true,
        key.format,
        key.double_sided,
        wgpu::MultisampleState {
            count: key.samples,
            mask: u64::MAX,
            alpha_to_coverage_enabled: false,
        },
    )
}
