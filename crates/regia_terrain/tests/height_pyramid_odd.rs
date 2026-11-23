// SPDX-License-Identifier: MPL-2.0
//! Odd-sized height pyramids agree between the CPU mirror, the shader and the uploaded mips,
//! and no level drops the map's last row or column (ADR-3367).
//!
//! Every real terrain map is `2^n + 1` samples on a side. The pyramid keeps the GPU mip size
//! (`max(1, n >> k)`) and averaged texels `2i` and `2i + 1` only, so on an odd side the last row
//! and column of the map reached no coarse level: a coarse vertex or distant normal at the far
//! border read the map's interior. The rule now (`height_mips::parent_taps`): the last texel of
//! an odd side averages the four parent texels centred on it, so the trailing texel is read and
//! every texel keeps its position.
//!
//! The check runs `terrain.wgsl`'s own `level_dims` and `height_level` (their text, cut out of
//! the shader) in a compute pass over the height texture `TerrainGpu` uploaded, at every level of
//! 257×129, 1025×513, 3×2 and 2×3 maps (3×1 is not a valid map: the minimum is 2×2):
//!
//! - at every texel centre, against the CPU level (the uploaded texel is the CPU's);
//! - at off-grid points inside and past the border, against `HeightMips::sample` at that integer
//!   level, so the shader's sizes, centre offset and clamp match the CPU mirror;
//! - on a map that is zero but for its last column, every level's last texel on the GPU must
//!   carry it (the pairwise pyramid gave 0 there: this part fails on the old rule).
//!
//! Lavapipe is enough; no goldens, no timing.

use std::sync::OnceLock;

use regia_render::RenderDevice;
use regia_terrain::gpu::TerrainGpu;
use regia_terrain::height_mips::level_dims;
use regia_terrain::{CdlodConfig, HeightMips, LayerDesc, TerrainLayers, TerrainMap};
use wgpu::util::DeviceExt;

const TERRAIN_WGSL: &str = include_str!("../src/gpu/terrain.wgsl");

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// The text of WGSL function `name` in `source` (from `fn name(` to its closing brace).
fn wgsl_fn(source: &str, name: &str) -> String {
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("terrain.wgsl has no fn {name}"));
    let mut depth = 0i32;
    for (i, c) in source[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[start..=start + i].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("fn {name} is not closed");
}

/// A compute shader around the shader's own pyramid read: `out[i] = height_level(coords[i], k)`.
fn probe_source() -> String {
    format!(
        "struct Probe {{
    sample_max: vec2<f32>,
    height_scale: f32,
    height_offset: f32,
    level: u32,
    count: u32,
    pad0: u32,
    pad1: u32,
}};
@group(0) @binding(0) var<uniform> terrain: Probe;
@group(0) @binding(1) var height_texture: texture_2d<f32>;
@group(0) @binding(2) var<storage, read> coords: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read_write> out: array<f32>;

{}

{}

@compute @workgroup_size(64, 1, 1)
fn probe(@builtin(global_invocation_id) gid: vec3<u32>) {{
    if (gid.x >= terrain.count) {{
        return;
    }}
    out[gid.x] = height_level(coords[gid.x], terrain.level);
}}
",
        wgsl_fn(TERRAIN_WGSL, "level_dims"),
        wgsl_fn(TERRAIN_WGSL, "height_level"),
    )
}

/// Heights with a strong last row and column (what the old pyramid dropped) over a hashed
/// field, so every texel's box mean is distinct.
fn odd_map(w: u32, h: u32) -> TerrainMap {
    let mut heights = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let hash = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663)) % 97;
            let edge = if x + 1 == w { 40.0 } else { 0.0 } + if y + 1 == h { 25.0 } else { 0.0 };
            heights.push(hash as f32 * 0.1 + edge);
        }
    }
    TerrainMap::from_heights(w, h, heights, 64.0, 1.0, 0.0).expect("odd map")
}

/// Zero everywhere but the last column (100 m) and the last row (40 m).
fn edge_map(w: u32, h: u32) -> TerrainMap {
    let mut heights = vec![0.0f32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let edge = if x + 1 == w { 100.0 } else { 0.0 } + if y + 1 == h { 40.0 } else { 0.0 };
            heights[(y * w + x) as usize] = edge;
        }
    }
    TerrainMap::from_heights(w, h, heights, 64.0, 1.0, 0.0).expect("edge map")
}

/// Level-0 coordinates of every texel centre of level `k` (row-major).
fn texel_centres(lw: u32, lh: u32, k: u32) -> Vec<[f32; 2]> {
    let scale = (1u32 << k) as f32;
    let centre = (scale - 1.0) * 0.5;
    let mut coords = Vec::with_capacity((lw * lh) as usize);
    for j in 0..lh {
        for i in 0..lw {
            coords.push([i as f32 * scale + centre, j as f32 * scale + centre]);
        }
    }
    coords
}

/// Runs the shader's `height_level` at `coords` (level-0 sample coordinates) on level `k` of
/// `gpu`'s uploaded height texture.
fn shader_heights(gpu: &TerrainGpu, map: &TerrainMap, k: u32, coords: &[[f32; 2]]) -> Vec<f32> {
    let rd = device();
    let (d, q) = (&rd.device, &rd.queue);
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("height pyramid probe"),
        source: wgpu::ShaderSource::Wgsl(probe_source().into()),
    });
    let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    };
    let storage = |read_only| wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only },
        has_dynamic_offset: false,
        min_binding_size: None,
    };
    // The height texture is R32Float: bound unfilterable, as the terrain pass binds it.
    let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("height pyramid probe"),
        entries: &[
            entry(
                0,
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
            ),
            entry(
                1,
                wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
            ),
            entry(2, storage(true)),
            entry(3, storage(false)),
        ],
    });
    let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("height pyramid probe"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("height pyramid probe"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("probe"),
        compilation_options: Default::default(),
        cache: None,
    });
    let mut uniform = Vec::with_capacity(32);
    for v in [
        (map.width() - 1) as f32,
        (map.height() - 1) as f32,
        map.height_scale(),
        map.height_offset(),
    ] {
        uniform.extend_from_slice(&v.to_le_bytes());
    }
    for v in [k, coords.len() as u32, 0, 0] {
        uniform.extend_from_slice(&v.to_le_bytes());
    }
    let uniform = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("probe uniform"),
        contents: &uniform,
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let flat: Vec<f32> = coords.iter().flat_map(|c| [c[0], c[1]]).collect();
    let coord_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("probe coords"),
        contents: bytemuck::cast_slice(&flat),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let size = (coords.len() * 4) as u64;
    let out = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe out"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe readback"),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("probe bind"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(gpu.height_view()),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: coord_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: out.as_entire_binding(),
            },
        ],
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups((coords.len() as u32).div_ceil(64), 1, 1);
    }
    encoder.copy_buffer_to_buffer(&out, 0, &readback, 0, size);
    q.submit([encoder.finish()]);
    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map callback").expect("map");
    let data = slice.get_mapped_range().expect("mapped range");
    let values: Vec<f32> = bytemuck::cast_slice(&data).to_vec();
    drop(data);
    readback.unmap();
    values
}

fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc::ground()],
        ..TerrainLayers::default()
    }
}

#[test]
fn odd_pyramids_match_between_cpu_shader_and_uploaded_mips_at_every_level() {
    for (w, h) in [(257u32, 129u32), (1025, 513), (3, 2), (2, 3)] {
        let map = odd_map(w, h);
        let mips = HeightMips::build(&map);
        let gpu = TerrainGpu::new(
            device(),
            &map,
            &layers(),
            &CdlodConfig::with_grid_size(16),
            0,
            None,
            None,
        )
        .expect("terrain gpu");
        let levels = mips.level_count();
        assert_eq!(
            levels,
            regia_terrain::height_mips::mip_level_count(w, h),
            "{w}x{h}"
        );
        for k in 0..levels {
            let (lw, lh) = level_dims(w, h, k);
            // 1. Every texel centre: the shader reads the uploaded texel, which is the CPU's.
            let got = shader_heights(&gpu, &map, k, &texel_centres(lw, lh, k));
            for j in 0..lh {
                for i in 0..lw {
                    let want = if k == 0 {
                        map.heights()[(j * w + i) as usize]
                    } else {
                        let level = mips.level(k).expect("level");
                        assert_eq!((level.width, level.height), (lw, lh), "{w}x{h} level {k} size");
                        level.data[(j * lw + i) as usize]
                    };
                    let g = got[(j * lw + i) as usize];
                    assert!(
                        (g - want).abs() < 1e-4,
                        "{w}x{h} level {k} ({lw}x{lh}) texel ({i}, {j}): GPU {g}, CPU {want}"
                    );
                }
            }
            // 2. Off-grid points, inside and past the border: the shader's sizes, centre offset
            //    and clamp are the CPU mirror's.
            let mut probes = Vec::new();
            for a in 0..23u32 {
                for b in 0..17u32 {
                    probes.push([
                        -1.5 + (w as f32 + 2.0) * a as f32 / 22.0,
                        -1.5 + (h as f32 + 2.0) * b as f32 / 16.0,
                    ]);
                }
            }
            probes.push([(w - 1) as f32, (h - 1) as f32]);
            let got = shader_heights(&gpu, &map, k, &probes);
            for (p, g) in probes.iter().zip(&got) {
                let cpu = mips.sample(&map, *p, k as f32);
                assert!(
                    (cpu - g).abs() < 1e-3,
                    "{w}x{h} level {k} at {p:?}: shader {g}, CPU mirror {cpu}"
                );
            }
        }

        // 3. The last column and row reach every level on the GPU.
        let edges = edge_map(w, h);
        let gpu = TerrainGpu::new(
            device(),
            &edges,
            &layers(),
            &CdlodConfig::with_grid_size(16),
            0,
            None,
            None,
        )
        .expect("terrain gpu");
        for k in 1..levels {
            let (lw, lh) = level_dims(w, h, k);
            let got = shader_heights(&gpu, &edges, k, &texel_centres(lw, lh, k));
            for j in 0..lh {
                let last = got[(j * lw + lw - 1) as usize];
                assert!(
                    last > 0.0,
                    "{w}x{h} level {k} row {j}: the map's last column reached no texel ({last})"
                );
            }
            for i in 0..lw {
                let last = got[((lh - 1) * lw + i) as usize];
                assert!(
                    last > 0.0,
                    "{w}x{h} level {k} column {i}: the map's last row reached no texel ({last})"
                );
            }
        }
    }
}
