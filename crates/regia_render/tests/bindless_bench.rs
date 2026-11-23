// SPDX-License-Identifier: MPL-2.0
//! Bindless step 4 (ADR-427): material-binding microbenchmarks on 1,000 and 10,000 materials.
//!
//! Six ways to hand N materials (a 32-byte constant record plus one texture each) to N draws.
//! They are built straight on wgpu, apart from the renderer, so the same file measures every
//! step of the bindless plan (ADR-316, ADR-356) on any build:
//!
//! | path | constants | which material a draw reads | textures |
//! |---|---|---|---|
//! | `classic` | one uniform buffer per material | one bind group per material | in the material group |
//! | `ssbo_offset` | one storage buffer (the arena), a record per slot | one bind group per material, bound at `slot × stride` (step 1) | in the material group |
//! | `ssbo_instance` | the arena, bound whole once | `first_instance` = slot | one group per texture set |
//! | `ssbo_immediate` | the arena, bound whole once | a 4-byte immediate = slot (step 2b) | one group per texture set |
//! | `texture_table` | the arena, bound whole once | a 4-byte immediate = slot | one binding array of every texture, bound once (step 3) |
//! | `table_offset` | the arena, one group | the group rebound at the draw's dynamic offset (`slot × stride`; the renderer's level 3, ADR-863) | one binding array of every texture, bound once |
//!
//! Materials share [`TEXTURES`] textures in contiguous runs (material `i` uses texture
//! `i × TEXTURES / N`), as real content shares maps between material instances, and the draws
//! run in material order. Each draw covers one pixel of a 128 × 80 target, so the GPU work is
//! negligible and the numbers are the CPU's.
//!
//! Per path and material count the bench prints:
//! - **setup**: creating every material's resources from nothing (a level load), with the
//!   buffers and bind groups that took;
//! - **steady** frames: recording one pass of N draws (`encode`, from `create_command_encoder`
//!   to `finish`) and `queue.submit`;
//! - **churn** frames: the same, after replacing 1 % of the material records (new constants)
//!   and swapping one texture's view (a streaming residency change). `churn` is the time of
//!   those resource updates; the buffers and bind groups they created are counted.
//!
//! The ignored bench asserts only that the work happened (the counts), never a time
//! (`docs/06-testing-and-ci.md` § 9). Compare numbers only between runs on one machine.
//!
//! ```text
//! cargo test -p regia_render --release --test bindless_bench -- --ignored --nocapture
//! ```
//!
//! `every_path_draws_the_same_pixels` is not ignored: it checks on a small scene that every path
//! the device supports binds the right record and texture for every draw, before and after a
//! churn step, so the bench never times a path that draws the wrong thing.

use std::num::NonZeroU32;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regia_render::RenderDevice;

/// Distinct textures the materials share (the texture table's length).
const TEXTURES: u32 = 256;
/// Bytes of one material record (`Material` in the shaders).
const RECORD_BYTES: u64 = 32;
/// Render target width; material `i` draws pixel `i` in row-major order.
const WIDTH: u32 = 128;
/// Render target height: room for 10,240 materials.
const HEIGHT: u32 = 80;
/// The render target's format (`COPY_BYTES_PER_ROW_ALIGNMENT` divides `WIDTH × 4`).
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Frames recorded before timing starts.
const WARM: usize = 10;
/// Timed frames per mode.
const FRAMES: usize = 120;
/// Material counts the bench runs.
const SCALES: [u32; 2] = [1_000, 10_000];

const _: () = assert!(
    TEXTURES == 256,
    "the texture table's WGSL length is a literal 256"
);
const _: () = assert!(SCALES[1] <= WIDTH * HEIGHT, "one pixel per material");

/// Shared WGSL: the material record and the one-pixel triangle.
const COMMON_WGSL: &str = "
struct Material {
    color: vec4<f32>,
    texture: u32,
    pixel: u32,
    pad0: u32,
    pad1: u32,
}

const WIDTH: u32 = 128u;
const HEIGHT: u32 = 80u;

// A triangle whose only covered pixel centre is `pixel`'s: corners at the pixel's top-left
// corner and 1.5 pixels right and down from it.
fn corner(pixel: u32, vertex: u32) -> vec4<f32> {
    let cell = vec2<f32>(f32(pixel % WIDTH), f32(pixel / WIDTH));
    var offset = vec2<f32>(0.0, 0.0);
    if vertex == 1u {
        offset = vec2<f32>(1.5, 0.0);
    } else if vertex == 2u {
        offset = vec2<f32>(0.0, 1.5);
    }
    let p = (cell + offset) / vec2<f32>(f32(WIDTH), f32(HEIGHT));
    return vec4<f32>(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
}
";

/// `classic`: the material group holds the record as a uniform buffer.
const CLASSIC_WGSL: &str = "
@group(0) @binding(0) var<uniform> material: Material;
@group(0) @binding(1) var tex: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32) -> @builtin(position) vec4<f32> {
    return corner(material.pixel, vertex);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return material.color * textureLoad(tex, vec2<i32>(0, 0), 0);
}
";

/// `ssbo_offset`: the material group binds one record of the arena (step 1).
const SSBO_OFFSET_WGSL: &str = "
@group(0) @binding(0) var<storage, read> material: Material;
@group(0) @binding(1) var tex: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32) -> @builtin(position) vec4<f32> {
    return corner(material.pixel, vertex);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return material.color * textureLoad(tex, vec2<i32>(0, 0), 0);
}
";

/// `ssbo_instance`: the whole arena, indexed by the draw's first instance.
const SSBO_INSTANCE_WGSL: &str = "
@group(0) @binding(0) var<storage, read> materials: array<Material>;
@group(1) @binding(0) var tex: texture_2d<f32>;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) slot: u32,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32, @builtin(instance_index) slot: u32) -> VsOut {
    var out: VsOut;
    out.position = corner(materials[slot].pixel, vertex);
    out.slot = slot;
    return out;
}

@fragment
fn fs_main(input: VsOut) -> @location(0) vec4<f32> {
    return materials[input.slot].color * textureLoad(tex, vec2<i32>(0, 0), 0);
}
";

/// `ssbo_immediate`: the whole arena, indexed by a 4-byte immediate (ADR-356 step 2b).
const SSBO_IMMEDIATE_WGSL: &str = "
struct Draw {
    slot: u32,
}

var<immediate> current: Draw;
@group(0) @binding(0) var<storage, read> materials: array<Material>;
@group(1) @binding(0) var tex: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32) -> @builtin(position) vec4<f32> {
    return corner(materials[current.slot].pixel, vertex);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return materials[current.slot].color * textureLoad(tex, vec2<i32>(0, 0), 0);
}
";

/// `texture_table`: the arena and one array of every texture, both bound once (ADR-356 step 3).
const TEXTURE_TABLE_WGSL: &str = "
struct Draw {
    slot: u32,
}

var<immediate> current: Draw;
@group(0) @binding(0) var<storage, read> materials: array<Material>;
@group(1) @binding(0) var textures: binding_array<texture_2d<f32>, 256>;

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32) -> @builtin(position) vec4<f32> {
    return corner(materials[current.slot].pixel, vertex);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    let material = materials[current.slot];
    return material.color * textureLoad(textures[material.texture], vec2<i32>(0, 0), 0);
}
";

/// `table_offset`: one record of the arena at the draw's dynamic offset, and the texture table
/// (the renderer's bindless level 3, ADR-863).
const TABLE_OFFSET_WGSL: &str = "
@group(0) @binding(0) var<storage, read> material: Material;
@group(1) @binding(0) var textures: binding_array<texture_2d<f32>, 256>;

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32) -> @builtin(position) vec4<f32> {
    return corner(material.pixel, vertex);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return material.color * textureLoad(textures[material.texture], vec2<i32>(0, 0), 0);
}
";

/// One way of binding materials (the module docs' table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    Classic,
    SsboOffset,
    SsboInstance,
    SsboImmediate,
    TextureTable,
    TableOffset,
}

impl Path {
    const ALL: [Self; 6] = [
        Self::Classic,
        Self::SsboOffset,
        Self::SsboInstance,
        Self::SsboImmediate,
        Self::TextureTable,
        Self::TableOffset,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::SsboOffset => "ssbo_offset",
            Self::SsboInstance => "ssbo_instance",
            Self::SsboImmediate => "ssbo_immediate",
            Self::TextureTable => "texture_table",
            Self::TableOffset => "table_offset",
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::Classic => CLASSIC_WGSL,
            Self::SsboOffset => SSBO_OFFSET_WGSL,
            Self::SsboInstance => SSBO_INSTANCE_WGSL,
            Self::SsboImmediate => SSBO_IMMEDIATE_WGSL,
            Self::TextureTable => TEXTURE_TABLE_WGSL,
            Self::TableOffset => TABLE_OFFSET_WGSL,
        }
    }

    /// Device features the path needs beyond WebGPU's core.
    fn features(self) -> wgpu::Features {
        match self {
            Self::Classic | Self::SsboOffset | Self::SsboInstance => wgpu::Features::empty(),
            Self::SsboImmediate => wgpu::Features::IMMEDIATES,
            Self::TextureTable => {
                wgpu::Features::IMMEDIATES
                    | wgpu::Features::TEXTURE_BINDING_ARRAY
                    | wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING
            }
            Self::TableOffset => {
                wgpu::Features::TEXTURE_BINDING_ARRAY
                    | wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING
            }
        }
    }

    /// Every texture in one binding array, bound once per pass.
    fn table(self) -> bool {
        matches!(self, Self::TextureTable | Self::TableOffset)
    }

    /// The arena's group is rebound per draw at the material's dynamic offset.
    fn dynamic_offset(self) -> bool {
        self == Self::TableOffset
    }

    /// One bind group per material (and so one bind per draw).
    fn per_material_groups(self) -> bool {
        matches!(self, Self::Classic | Self::SsboOffset)
    }

    /// One bind group per texture (bound when the texture changes between draws).
    fn per_texture_groups(self) -> bool {
        matches!(self, Self::SsboInstance | Self::SsboImmediate)
    }

    /// The draw's slot travels in an immediate.
    fn immediates(self) -> bool {
        matches!(self, Self::SsboImmediate | Self::TextureTable)
    }

    /// Why `gpu` cannot run this path, if it cannot.
    fn unsupported(self, gpu: &Gpu) -> Option<String> {
        let missing = self.features() - gpu.device.features();
        if !missing.is_empty() {
            return Some(format!("missing {missing:?}"));
        }
        let limits = gpu.device.limits();
        if self != Self::Classic && limits.max_storage_buffers_per_shader_stage == 0 {
            return Some("no storage buffers in shaders".to_owned());
        }
        if self.immediates() && limits.max_immediate_size < 4 {
            return Some(format!("max_immediate_size {}", limits.max_immediate_size));
        }
        if self.table() && limits.max_binding_array_elements_per_shader_stage < TEXTURES {
            return Some(format!(
                "max_binding_array_elements_per_shader_stage {}",
                limits.max_binding_array_elements_per_shader_stage
            ));
        }
        None
    }
}

/// The bench's own device on the renderer's adapter pick (`REGIA_GPU_INDEX` applies), with
/// every feature a path above can use.
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Adapter name and backend, for the printed header.
    adapter: String,
    /// The renderer's device: keeps the instance and adapter it came from alive.
    _base: RenderDevice,
}

impl Gpu {
    /// One bench device per process, like [`RenderDevice::shared_headless`]: test threads that
    /// each open a device can livelock the NVIDIA driver.
    fn shared() -> &'static Self {
        static GPU: OnceLock<Gpu> = OnceLock::new();
        GPU.get_or_init(Self::new)
    }

    fn new() -> Self {
        let base = match RenderDevice::shared_headless() {
            Ok(device) => device,
            Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
        };
        let wanted = Path::ALL
            .iter()
            .fold(wgpu::Features::empty(), |all, path| all | path.features());
        let features = base.adapter.features() & wanted;
        let mut limits = base.adapter.limits();
        if !features.contains(wgpu::Features::IMMEDIATES) {
            // Asking for immediate space without the feature fails device creation.
            limits.max_immediate_size = 0;
        }
        let (device, queue) =
            regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("bindless bench"),
                required_features: features,
                required_limits: limits,
                ..Default::default()
            }))
            .expect("bench device on the renderer's adapter");
        let info = base.adapter.get_info();
        Self {
            device,
            queue,
            adapter: format!(
                "{} ({:?}, driver {} {})",
                info.name, info.backend, info.driver, info.driver_info
            ),
            _base: base,
        }
    }

    fn wait(&self) {
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("wait for the bench frame");
    }
}

/// Two 1 × 1 textures per texture slot: the resident view and the one a streaming swap moves to.
struct Textures {
    views: [Vec<wgpu::TextureView>; 2],
}

impl Textures {
    fn new(gpu: &Gpu) -> Self {
        let views = [0, 1].map(|variant| {
            (0..TEXTURES)
                .map(|slot| {
                    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("bindless bench texture"),
                        size: wgpu::Extent3d {
                            width: 1,
                            height: 1,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    gpu.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &texel(slot, variant),
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(4),
                            rows_per_image: Some(1),
                        },
                        wgpu::Extent3d {
                            width: 1,
                            height: 1,
                            depth_or_array_layers: 1,
                        },
                    );
                    texture.create_view(&wgpu::TextureViewDescriptor::default())
                })
                .collect::<Vec<_>>()
        });
        Self { views }
    }
}

/// The texel of texture `slot`'s `variant` (0 resident, 1 after a swap).
fn texel(slot: u32, variant: u32) -> [u8; 4] {
    let s = slot + variant * 101;
    [
        (s * 13 % 200 + 55) as u8,
        (s * 29 % 200 + 55) as u8,
        (s * 53 % 200 + 55) as u8,
        255,
    ]
}

/// Texture slot of material `i` of `n`: contiguous runs of materials share a texture.
fn texture_of(i: u32, n: u32) -> u32 {
    (u64::from(i) * u64::from(TEXTURES) / u64::from(n)) as u32
}

/// Material `i`'s record at `generation` (a churn step bumps it, so the constants change).
fn record(i: u32, n: u32, generation: u32) -> [u8; RECORD_BYTES as usize] {
    let k = i * 7 + generation * 3;
    let color = [
        (k * 37 % 251 + 4) as f32 / 255.0,
        (k * 61 % 251 + 4) as f32 / 255.0,
        (k * 97 % 251 + 4) as f32 / 255.0,
        1.0,
    ];
    let mut bytes = [0u8; RECORD_BYTES as usize];
    for (c, value) in color.iter().enumerate() {
        bytes[c * 4..c * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[16..20].copy_from_slice(&texture_of(i, n).to_le_bytes());
    bytes[20..24].copy_from_slice(&i.to_le_bytes());
    bytes
}

/// The RGBA8 pixel material `i` should draw: its colour times its texture's texel.
fn expected_pixel(i: u32, n: u32, generation: u32, variant: u32) -> [u8; 4] {
    let bytes = record(i, n, generation);
    let tex = texel(texture_of(i, n), variant);
    std::array::from_fn(|c| {
        let color = f32::from_le_bytes([bytes[c * 4], bytes[c * 4 + 1], bytes[c * 4 + 2], bytes[c * 4 + 3]]);
        (color * (f32::from(tex[c]) / 255.0) * 255.0).round() as u8
    })
}

/// Work counted by construction: what each phase created and what each frame recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    buffers: u64,
    bind_groups: u64,
    writes: u64,
    set_bind_group: u64,
    set_immediates: u64,
    draws: u64,
}

impl Counts {
    fn since(self, earlier: Self) -> Self {
        Self {
            buffers: self.buffers - earlier.buffers,
            bind_groups: self.bind_groups - earlier.bind_groups,
            writes: self.writes - earlier.writes,
            set_bind_group: self.set_bind_group - earlier.set_bind_group,
            set_immediates: self.set_immediates - earlier.set_immediates,
            draws: self.draws - earlier.draws,
        }
    }
}

/// One path's N materials and the pipeline that draws them.
struct Scene {
    path: Path,
    n: u32,
    pipeline: wgpu::RenderPipeline,
    /// Group 0 of `classic` / `ssbo_offset` (per material); unused otherwise.
    material_layout: wgpu::BindGroupLayout,
    /// Group 1 of the texture-set and table paths (per texture, or the whole table).
    texture_layout: wgpu::BindGroupLayout,
    /// `classic`'s uniform buffer per material.
    uniforms: Vec<wgpu::Buffer>,
    /// The record arena of every other path.
    arena: Option<wgpu::Buffer>,
    /// Bytes between two records in `arena`.
    stride: u64,
    /// Group 0 of the arena paths (the whole arena).
    arena_group: Option<wgpu::BindGroup>,
    /// Per material (`classic`, `ssbo_offset`), per texture (texture sets) or one (the table).
    groups: Vec<wgpu::BindGroup>,
    /// Materials using each texture (rebuilt with it on the per-material paths).
    members: Vec<Vec<u32>>,
    /// Each material's record generation.
    generations: Vec<u32>,
    /// Which variant of each texture is resident.
    resident: Vec<u32>,
    counts: Counts,
}

impl Scene {
    fn new(gpu: &Gpu, textures: &Textures, path: Path, n: u32) -> Self {
        let device = &gpu.device;
        let record_size = wgpu::BufferSize::new(RECORD_BYTES);
        let texture_entry = |binding: u32, count: Option<NonZeroU32>| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count,
        };
        let record_entry = |ty: wgpu::BufferBindingType, min_binding_size| wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty,
                has_dynamic_offset: false,
                min_binding_size,
            },
            count: None,
        };
        let storage = wgpu::BufferBindingType::Storage { read_only: true };
        let material_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bindless bench material"),
            entries: &[
                record_entry(
                    if path == Path::Classic {
                        wgpu::BufferBindingType::Uniform
                    } else {
                        storage
                    },
                    record_size,
                ),
                texture_entry(1, None),
            ],
        });
        let mut arena_entry = record_entry(storage, record_size);
        if path.dynamic_offset() {
            arena_entry.ty = wgpu::BindingType::Buffer {
                ty: storage,
                has_dynamic_offset: true,
                min_binding_size: record_size,
            };
        }
        let arena_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bindless bench arena"),
            entries: &[arena_entry],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bindless bench textures"),
            entries: &[texture_entry(
                0,
                if path.table() {
                    NonZeroU32::new(TEXTURES)
                } else {
                    None
                },
            )],
        });
        let per_material = [Some(&material_layout)];
        let arena_and_textures = [Some(&arena_layout), Some(&texture_layout)];
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bindless bench"),
            bind_group_layouts: if path.per_material_groups() {
                &per_material
            } else {
                &arena_and_textures
            },
            immediate_size: if path.immediates() { 4 } else { 0 },
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(path.name()),
            // The table paths declare a `binding_array` (naga needs the extension enabled; a device
            // without binding arrays refuses the directive, and runs no table path).
            source: wgpu::ShaderSource::Wgsl(
                format!(
                    "{}{COMMON_WGSL}{}",
                    if path.table() {
                        "enable wgpu_binding_array;\n"
                    } else {
                        ""
                    },
                    path.source()
                )
                .into(),
            ),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(path.name()),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let stride = if matches!(path, Path::SsboOffset | Path::TableOffset) {
            RECORD_BYTES.next_multiple_of(u64::from(device.limits().min_storage_buffer_offset_alignment))
        } else {
            RECORD_BYTES
        };
        let mut members = vec![Vec::new(); TEXTURES as usize];
        for i in 0..n {
            members[texture_of(i, n) as usize].push(i);
        }
        let mut scene = Self {
            path,
            n,
            pipeline,
            material_layout,
            texture_layout,
            uniforms: Vec::new(),
            arena: None,
            stride,
            arena_group: None,
            groups: Vec::new(),
            members,
            generations: vec![0; n as usize],
            resident: vec![0; TEXTURES as usize],
            counts: Counts::default(),
        };
        scene.load(gpu, textures, &arena_layout);
        scene
    }

    /// Creates every material's resources: what a level load does on this path.
    fn load(&mut self, gpu: &Gpu, textures: &Textures, arena_layout: &wgpu::BindGroupLayout) {
        let n = self.n;
        if self.path == Path::Classic {
            self.uniforms = (0..n).map(|i| self.uniform(gpu, i)).collect();
        } else {
            let size = u64::from(n) * self.stride;
            let arena = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("bindless bench arena"),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.counts.buffers += 1;
            let mut bytes = vec![0u8; size as usize];
            for i in 0..n {
                let at = (u64::from(i) * self.stride) as usize;
                bytes[at..at + RECORD_BYTES as usize].copy_from_slice(&record(i, n, 0));
            }
            gpu.queue.write_buffer(&arena, 0, &bytes);
            self.counts.writes += 1;
            if !self.path.per_material_groups() {
                // A dynamic-offset binding is one record long; the draw picks the record.
                let resource = if self.path.dynamic_offset() {
                    wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &arena,
                        offset: 0,
                        size: wgpu::BufferSize::new(RECORD_BYTES),
                    })
                } else {
                    arena.as_entire_binding()
                };
                self.arena_group = Some(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("bindless bench arena"),
                    layout: arena_layout,
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource }],
                }));
                self.counts.bind_groups += 1;
            }
            self.arena = Some(arena);
        }
        self.groups = if self.path.per_material_groups() {
            (0..n).map(|i| self.material_group(gpu, textures, i)).collect()
        } else if self.path.per_texture_groups() {
            (0..TEXTURES).map(|t| self.set_group(gpu, textures, t)).collect()
        } else {
            vec![self.table_group(gpu, textures)]
        };
    }

    /// `classic`: a new uniform buffer holding material `i`'s current record.
    fn uniform(&mut self, gpu: &Gpu, i: u32) -> wgpu::Buffer {
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bindless bench material"),
            size: RECORD_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue
            .write_buffer(&buffer, 0, &record(i, self.n, self.generations[i as usize]));
        self.counts.buffers += 1;
        self.counts.writes += 1;
        buffer
    }

    /// `classic` / `ssbo_offset`: material `i`'s group (its record and its resident texture).
    fn material_group(&mut self, gpu: &Gpu, textures: &Textures, i: u32) -> wgpu::BindGroup {
        let t = texture_of(i, self.n) as usize;
        let buffer = match &self.arena {
            Some(arena) => wgpu::BufferBinding {
                buffer: arena,
                offset: u64::from(i) * self.stride,
                size: wgpu::BufferSize::new(RECORD_BYTES),
            },
            None => wgpu::BufferBinding {
                buffer: &self.uniforms[i as usize],
                offset: 0,
                size: None,
            },
        };
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bindless bench material"),
            layout: &self.material_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(buffer),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(
                        &textures.views[self.resident[t] as usize][t],
                    ),
                },
            ],
        });
        self.counts.bind_groups += 1;
        group
    }

    /// Texture-set paths: texture `t`'s group.
    fn set_group(&mut self, gpu: &Gpu, textures: &Textures, t: u32) -> wgpu::BindGroup {
        let view = &textures.views[self.resident[t as usize] as usize][t as usize];
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bindless bench texture set"),
            layout: &self.texture_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            }],
        });
        self.counts.bind_groups += 1;
        group
    }

    /// `texture_table`: the table of every resident view (ADR-356: rebuilt at most once a frame).
    fn table_group(&mut self, gpu: &Gpu, textures: &Textures) -> wgpu::BindGroup {
        let views: Vec<&wgpu::TextureView> = (0..TEXTURES as usize)
            .map(|t| &textures.views[self.resident[t] as usize][t])
            .collect();
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bindless bench texture table"),
            layout: &self.texture_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureViewArray(&views),
            }],
        });
        self.counts.bind_groups += 1;
        group
    }

    /// Materials one churn step replaces (1 %, at least one).
    fn churn_count(&self) -> u32 {
        (self.n / 100).max(1)
    }

    /// One churn step: `churn_count` materials get new constants (a new material on the
    /// classic path: a new buffer and group), then texture `step % TEXTURES` swaps its view.
    fn churn(&mut self, gpu: &Gpu, textures: &Textures, step: u32) {
        let n = self.n;
        let count = self.churn_count();
        for k in 0..count {
            let i = (step * count + k) % n;
            self.generations[i as usize] += 1;
            match self.path {
                Path::Classic => {
                    self.uniforms[i as usize] = self.uniform(gpu, i);
                    self.groups[i as usize] = self.material_group(gpu, textures, i);
                }
                Path::SsboOffset => {
                    self.write_record(gpu, i);
                    // Step 1 binds the record at a fixed offset, so a new material is a new group.
                    self.groups[i as usize] = self.material_group(gpu, textures, i);
                }
                Path::SsboInstance | Path::SsboImmediate | Path::TextureTable | Path::TableOffset => {
                    self.write_record(gpu, i);
                }
            }
        }
        let t = step % TEXTURES;
        self.resident[t as usize] ^= 1;
        if self.path.per_material_groups() {
            for m in std::mem::take(&mut self.members[t as usize]) {
                self.groups[m as usize] = self.material_group(gpu, textures, m);
                self.members[t as usize].push(m);
            }
        } else if self.path.per_texture_groups() {
            self.groups[t as usize] = self.set_group(gpu, textures, t);
        } else {
            self.groups[0] = self.table_group(gpu, textures);
        }
    }

    fn write_record(&mut self, gpu: &Gpu, i: u32) {
        let Some(arena) = &self.arena else {
            return;
        };
        gpu.queue.write_buffer(
            arena,
            u64::from(i) * self.stride,
            &record(i, self.n, self.generations[i as usize]),
        );
        self.counts.writes += 1;
    }

    /// Records one pass of N draws, one per material in material order.
    fn encode(&mut self, gpu: &Gpu, target: &wgpu::TextureView) -> wgpu::CommandBuffer {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("bindless bench"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("bindless bench"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            let counts = &mut self.counts;
            if let Some(arena) = &self.arena_group
                && !self.path.dynamic_offset()
            {
                pass.set_bind_group(0, arena, &[]);
                counts.set_bind_group += 1;
            }
            if self.path.table() {
                pass.set_bind_group(1, &self.groups[0], &[]);
                counts.set_bind_group += 1;
            }
            let mut bound_texture = None;
            for i in 0..self.n {
                if self.path.per_material_groups() {
                    pass.set_bind_group(0, &self.groups[i as usize], &[]);
                    counts.set_bind_group += 1;
                } else if self.path.dynamic_offset()
                    && let Some(arena) = &self.arena_group
                {
                    let offset = u32::try_from(u64::from(i) * self.stride).expect("arena offset fits u32");
                    pass.set_bind_group(0, arena, &[offset]);
                    counts.set_bind_group += 1;
                } else if self.path.per_texture_groups() {
                    let t = texture_of(i, self.n);
                    if bound_texture != Some(t) {
                        pass.set_bind_group(1, &self.groups[t as usize], &[]);
                        counts.set_bind_group += 1;
                        bound_texture = Some(t);
                    }
                }
                if self.path.immediates() {
                    pass.set_immediates(0, &i.to_le_bytes());
                    counts.set_immediates += 1;
                }
                if self.path == Path::SsboInstance {
                    pass.draw(0..3, i..i + 1);
                } else {
                    pass.draw(0..3, 0..1);
                }
                counts.draws += 1;
            }
        }
        encoder.finish()
    }

    /// The colour every drawn pixel should hold now (`n` pixels, row-major).
    fn expected(&self) -> Vec<[u8; 4]> {
        (0..self.n)
            .map(|i| {
                let t = texture_of(i, self.n) as usize;
                expected_pixel(i, self.n, self.generations[i as usize], self.resident[t])
            })
            .collect()
    }
}

/// The render target and its readback buffer.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl Target {
    fn new(gpu: &Gpu) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bindless bench target"),
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self { texture, view }
    }

    /// Every pixel, row-major (`WIDTH × 4` bytes is a whole copy row, so no padding).
    fn read(&self, gpu: &Gpu) -> Vec<[u8; 4]> {
        let bytes_per_row = WIDTH * 4;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bindless bench readback"),
            size: u64::from(bytes_per_row) * u64::from(HEIGHT),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("bindless bench readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(HEIGHT),
                },
            },
            wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        gpu.wait();
        rx.recv().expect("map callback").expect("map readback");
        let mapped = buffer.slice(..).get_mapped_range().expect("mapped range");
        let pixels = mapped.as_chunks::<4>().0.to_vec();
        drop(mapped);
        buffer.unmap();
        pixels
    }
}

/// p50 / p99 / max of `samples`.
fn summary(mut samples: Vec<Duration>) -> (Duration, Duration, Duration) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    (at(0.5), at(0.99), samples[samples.len() - 1])
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// One mode's timed frames.
struct Run {
    churn: Vec<Duration>,
    encode: Vec<Duration>,
    submit: Vec<Duration>,
    /// Work per timed frame (the totals over the timed frames divided by `FRAMES`).
    per_frame: Counts,
}

fn run_frames(gpu: &Gpu, textures: &Textures, target: &Target, scene: &mut Scene, churn: bool) -> Run {
    let mut run = Run {
        churn: Vec::with_capacity(FRAMES),
        encode: Vec::with_capacity(FRAMES),
        submit: Vec::with_capacity(FRAMES),
        per_frame: Counts::default(),
    };
    let mut start_counts = scene.counts;
    for frame in 0..WARM + FRAMES {
        if frame == WARM {
            start_counts = scene.counts;
        }
        let t0 = Instant::now();
        if churn {
            scene.churn(gpu, textures, frame as u32);
        }
        let t1 = Instant::now();
        let commands = scene.encode(gpu, &target.view);
        let t2 = Instant::now();
        gpu.queue.submit([commands]);
        let t3 = Instant::now();
        gpu.wait();
        if frame >= WARM {
            run.churn.push(t1 - t0);
            run.encode.push(t2 - t1);
            run.submit.push(t3 - t2);
        }
    }
    let total = scene.counts.since(start_counts);
    let frames = FRAMES as u64;
    run.per_frame = Counts {
        buffers: total.buffers / frames,
        bind_groups: total.bind_groups / frames,
        writes: total.writes / frames,
        set_bind_group: total.set_bind_group / frames,
        set_immediates: total.set_immediates / frames,
        draws: total.draws / frames,
    };
    run
}

/// Distinct textures the materials of a scene of `n` use.
fn textures_used(n: u32) -> u32 {
    let mut used: Vec<u32> = (0..n).map(|i| texture_of(i, n)).collect();
    used.dedup();
    used.len() as u32
}

/// The per-frame work the module docs promise for `path` at `n` materials.
fn expected_frame(path: Path, n: u32, churn: bool) -> Counts {
    let churned = if churn { (n / 100).max(1) } else { 0 };
    // Materials on the swapped texture: the swaps go round every texture, so the per-frame
    // mean over whole cycles is n / TEXTURES; the bench checks the per-frame floor below.
    let set_binds = match path {
        Path::Classic | Path::SsboOffset => u64::from(n),
        Path::SsboInstance | Path::SsboImmediate => 1 + u64::from(textures_used(n)),
        Path::TextureTable => 2,
        // The table once, the arena group once per draw (each at its own offset).
        Path::TableOffset => 1 + u64::from(n),
    };
    Counts {
        buffers: if path == Path::Classic {
            u64::from(churned)
        } else {
            0
        },
        bind_groups: match (path, churn) {
            (_, false) => 0,
            (Path::Classic | Path::SsboOffset, true) => u64::from(churned),
            (_, true) => 1,
        },
        writes: u64::from(churned),
        set_bind_group: set_binds,
        set_immediates: if path.immediates() { u64::from(n) } else { 0 },
        draws: u64::from(n),
    }
}

/// RGBA8 pixels of one read-back target, row-major.
type Rgba8Image = Vec<[u8; 4]>;

#[test]
fn every_path_draws_the_same_pixels() {
    let gpu = Gpu::shared();
    let textures = Textures::new(gpu);
    let target = Target::new(gpu);
    // Fewer materials than textures: every material has its own texture.
    let n = 48;
    let mut ran = Vec::new();
    let mut reference: Option<(Rgba8Image, Rgba8Image)> = None;
    for path in Path::ALL {
        if let Some(why) = path.unsupported(gpu) {
            eprintln!("{}: skipped on {} ({why})", path.name(), gpu.adapter);
            continue;
        }
        let mut scene = Scene::new(gpu, &textures, path, n);
        let mut frames = Vec::new();
        for step in 0..2 {
            if step == 1 {
                // Churn step 0 replaces material 0 and swaps texture 0 (material 0's).
                scene.churn(gpu, &textures, 0);
            }
            let before = scene.counts;
            let commands = scene.encode(gpu, &target.view);
            gpu.queue.submit([commands]);
            gpu.wait();
            let drawn = scene.counts.since(before);
            let expected = expected_frame(path, n, false);
            assert_eq!(
                (drawn.draws, drawn.set_bind_group, drawn.set_immediates),
                (expected.draws, expected.set_bind_group, expected.set_immediates),
                "{}: draw-loop counts",
                path.name()
            );
            let pixels = target.read(gpu);
            let want = scene.expected();
            for (i, (got, want)) in pixels.iter().zip(&want).enumerate() {
                assert!(
                    got.iter().zip(want).all(|(g, w)| g.abs_diff(*w) <= 1),
                    "{} step {step}: pixel {i} is {got:?}, expected {want:?}",
                    path.name()
                );
            }
            assert!(
                pixels[n as usize..].iter().all(|p| *p == [0; 4]),
                "{} step {step}: a draw landed outside its pixel",
                path.name()
            );
            frames.push(pixels);
        }
        assert_ne!(
            frames[0][0],
            frames[1][0],
            "{}: the churn step changed material 0",
            path.name()
        );
        // Every path computes the same products in the same order: identical bytes.
        let pair = (frames.remove(0), frames.remove(0));
        let classic = reference.get_or_insert_with(|| pair.clone());
        assert!(*classic == pair, "{} draws other bytes than classic", path.name());
        ran.push(path);
    }
    assert!(
        ran.starts_with(&[Path::Classic, Path::SsboOffset, Path::SsboInstance]),
        "the storage-buffer paths need no optional feature: ran {ran:?} on {}",
        gpu.adapter
    );
}

#[test]
fn churn_work_is_counted_per_path() {
    let gpu = Gpu::shared();
    let textures = Textures::new(gpu);
    // 512 materials: two per texture, five churned per step.
    let n = 512;
    for path in Path::ALL {
        if path.unsupported(gpu).is_some() {
            continue;
        }
        let mut scene = Scene::new(gpu, &textures, path, n);
        let loaded = scene.counts;
        let (buffers, groups) = match path {
            Path::Classic => (u64::from(n), u64::from(n)),
            Path::SsboOffset => (1, u64::from(n)),
            Path::SsboInstance | Path::SsboImmediate => (1, 1 + u64::from(TEXTURES)),
            Path::TextureTable | Path::TableOffset => (1, 2),
        };
        assert_eq!(
            (loaded.buffers, loaded.bind_groups),
            (buffers, groups),
            "{} load",
            path.name()
        );
        scene.churn(gpu, &textures, 3);
        let step = scene.counts.since(loaded);
        let expected = expected_frame(path, n, true);
        // The per-material paths also rebuild the groups of both materials on the swapped texture.
        let swap_groups = if path.per_material_groups() { 2 } else { 0 };
        assert_eq!(step.buffers, expected.buffers, "{} churn buffers", path.name());
        assert_eq!(
            step.bind_groups,
            expected.bind_groups + swap_groups,
            "{} churn bind groups",
            path.name()
        );
        assert_eq!(step.writes, expected.writes, "{} churn writes", path.name());
    }
}

#[test]
#[ignore = "bench: prints numbers"]
fn bindless_bench() {
    let gpu = Gpu::shared();
    let textures = Textures::new(gpu);
    let target = Target::new(gpu);
    println!("bindless bench on {}", gpu.adapter);
    if cfg!(debug_assertions) {
        println!("WARNING: a debug build; run with --release for numbers worth recording");
    }
    let mut rows = Vec::new();
    for n in SCALES {
        for path in Path::ALL {
            if let Some(why) = path.unsupported(gpu) {
                println!("{} n={n}: skipped ({why})", path.name());
                rows.push(format!(
                    "| `{}` | {n} | skipped: {why} | | | | | | | |",
                    path.name()
                ));
                continue;
            }
            let t0 = Instant::now();
            let mut scene = Scene::new(gpu, &textures, path, n);
            let setup = t0.elapsed();
            let loaded = scene.counts;
            gpu.wait();
            let steady = run_frames(gpu, &textures, &target, &mut scene, false);
            let churn = run_frames(gpu, &textures, &target, &mut scene, true);
            assert_eq!(
                steady.per_frame,
                expected_frame(path, n, false),
                "{} n={n} steady",
                path.name()
            );
            let want = expected_frame(path, n, true);
            assert_eq!(
                (
                    churn.per_frame.buffers,
                    churn.per_frame.writes,
                    churn.per_frame.draws
                ),
                (want.buffers, want.writes, want.draws),
                "{} n={n} churn",
                path.name()
            );
            assert!(
                churn.per_frame.bind_groups >= want.bind_groups,
                "{} n={n} churn groups",
                path.name()
            );
            let name = path.name();
            for (mode, run) in [("steady", &steady), ("churn", &churn)] {
                for (part, samples) in [
                    ("churn", &run.churn),
                    ("encode", &run.encode),
                    ("submit", &run.submit),
                ] {
                    if mode == "steady" && part == "churn" {
                        continue;
                    }
                    let (p50, p99, max) = summary(samples.clone());
                    println!(
                        "perf-micro bindless_{name}_{n}_{mode}_{part}: n={FRAMES} p50={p50:?} p99={p99:?} max={max:?}"
                    );
                }
            }
            let (steady_encode, steady_encode_p99, _) = summary(steady.encode.clone());
            let (steady_submit, _, _) = summary(steady.submit.clone());
            let (churn_ops, churn_ops_p99, _) = summary(churn.churn.clone());
            let (churn_encode, _, _) = summary(churn.encode.clone());
            println!(
                "{name} n={n}: setup {:.3} ms ({} buffers, {} bind groups); steady frame: {} binds, {} immediates, {} draws; churn frame: {} buffers, {} bind groups, {} writes",
                ms(setup),
                loaded.buffers,
                loaded.bind_groups,
                steady.per_frame.set_bind_group,
                steady.per_frame.set_immediates,
                steady.per_frame.draws,
                churn.per_frame.buffers,
                churn.per_frame.bind_groups,
                churn.per_frame.writes,
            );
            rows.push(format!(
                "| `{name}` | {n} | {:.2} | {} / {} | {:.3} / {:.3} | {:.3} | {:.3} / {:.3} | {:.3} | {} / {} | {} |",
                ms(setup),
                loaded.buffers,
                loaded.bind_groups,
                ms(steady_encode),
                ms(steady_encode_p99),
                ms(steady_submit),
                ms(churn_ops),
                ms(churn_ops_p99),
                ms(churn_encode),
                churn.per_frame.buffers,
                churn.per_frame.bind_groups,
                steady.per_frame.set_bind_group,
            ));
        }
    }
    // The table `docs/evidence/bindless/README.md` records (milliseconds).
    println!();
    println!(
        "| path | materials | setup | setup buffers / groups | steady encode p50 / p99 | steady submit p50 | churn updates p50 / p99 | churn encode p50 | churn buffers / groups per frame | binds per frame |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for row in rows {
        println!("{row}");
    }
}
