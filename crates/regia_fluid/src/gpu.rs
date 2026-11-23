// SPDX-License-Identifier: MPL-2.0
//! The wgpu compute path (`gpu` feature, ADR-2441 § 7): the PIC/FLIP substep of [`crate::sim`] in
//! WGSL (`fluid_flip.wgsl`), one code path for every backend (lavapipe in CI).
//!
//! # 1. What runs on the GPU
//!
//! Per substep: clear, particles to grid (fixed-point sums in 1/65536 units, 64 bits wide as pairs
//! of `atomic<u32>` words, so no clump of particles on one face wraps them), one
//! extrapolation layer, gravity and the external acceleration, the weighted divergence, `iterations`
//! red-black SOR sweeps of the same variational Poisson system the CPU builds (open-face fractions
//! from the CPU's solid field, free surface `p = 0` in air cells), the pressure-gradient update, and
//! grid to particles (the PIC/FLIP blend) with RK2 advection and the closed domain walls.
//!
//! # 2. What stays on the CPU
//!
//! Emitters, sinks, reseeding, the position correction, the push out of solid colliders (solids
//! still act through the open-face fractions of the pressure solve), whitewater, the surface mesh
//! and the cache. [`GpuFluid`] steps an existing particle set; [`GpuFluid::sync_into`] hands it back
//! to a [`FluidSim`] for those.
//!
//! Moving solids (ADR-2577, at most [`MAX_GPU_SOLIDS`]) go up in the uniform with their pose at the
//! start of the next step; a `rasterize` kernel poses them at each substep's time (a counter the
//! `clear` kernel advances; [`GpuFluid::set_moving_solids`] resets it) and narrows each face's static
//! open fraction by their coverage, with the same 4 x 4 bilinear subsamples of the node distances as
//! the CPU. A face they close carries their velocity where they are at least as near as the static
//! colliders at the face centre (the CPU's rule; the static distances go up once), the divergence
//! counts the solid share at that velocity, and `g2p_advect` pushes particles out of them relative
//! to their velocity. A cell whose centre is inside a static collider or a moving solid and that
//! touches the liquid counts as liquid, as on the CPU. Where a moving solid overlaps a static
//! collider the GPU takes the smaller of the two open fractions (the CPU folds both distances
//! first). The pressure impulses on the solids (two-way coupling) are CPU only.
//!
//! # 3. Determinism
//!
//! Not bit-identical to the CPU: SOR instead of PCG, one extrapolation layer instead of three, and
//! fixed-point atomic sums (order-free, so a GPU run repeats itself on one device). The CPU
//! reference is the contract.
//!
//! The kernels call no WGSL builtin whose accuracy the spec leaves loosest (`sin`, `cos`, `sqrt`,
//! `inverseSqrt`, `length`): `fluid_flip.wgsl` carries software versions built on add, multiply,
//! divide, `round` and `bitcast`, and the `p2g` scatter saturates into the i32 fixed-point range
//! (ADR-4753). One input repeats bit for bit on one device. Two devices can still differ in the
//! last bits: WGSL allows f32 division 2.5 ULP and lets a driver fuse a multiply-add, and an
//! RTX 5090 reads back other bits than lavapipe after 40 steps of the spinning-paddle test
//! (ADR-5598, first seen on the kernels of 008db7177). Across devices the contract is the CPU
//! tolerance below.
//! The pinned readback in `tests/gpu.rs` is lavapipe's (the CI reference): it is exact on that
//! device only. The 2026-11-10 RTX 5090 run compared NVIDIA output with pins derived on NVIDIA on
//! another base, and a later 5090 run read other bits, so cross-device bit identity is not
//! claimed (rev-kimi-21 finding 1).
//! Uploads clamp particle speeds to [`MAX_FIXEDPOINT_SPEED`], the ceiling that saturation
//! implies.
//!
//! # 4. Tolerance against the CPU reference
//!
//! On the dam break (1.6 x 0.6 x 0.1 m, 2.5 cm cells, `dt` 1/120, one substep, position correction
//! and reseeding off on the CPU side, 40 steps), `tests/gpu.rs` holds the GPU run to the CPU run
//! within: total liquid volume 2 %, the mean velocity `|mean_gpu - mean_cpu| <= 0.05 |mean_cpu| +
//! 0.02 m/s`, and the front (largest x) within one cell. Measured on lavapipe (2026-11-03): volume
//! 0.012 %, mean velocity 0.0024 m/s apart (at about 1.06 m/s), front 4.7 mm apart.

use glam::Vec3;
use wgpu::util::DeviceExt;

use crate::sim::FluidSim;
use crate::solid::{MovingSolid, SolidShape};

/// The shader.
pub const SHADER: &str = include_str!("fluid_flip.wgsl");

/// Default red-black SOR sweeps per substep.
pub const DEFAULT_ITERATIONS: u32 = 200;

/// Default over-relaxation factor of the sweeps.
pub const DEFAULT_OMEGA: f32 = 1.85;

/// Threads per workgroup of every kernel.
const WORKGROUP: u32 = 64;

/// Most moving solids the GPU path takes (the uniform holds a fixed array).
pub const MAX_GPU_SOLIDS: usize = 16;

/// The speed ceiling the GPU path sees, in m/s per component (ADR-4753).
///
/// The `p2g` scatter is fixed point `i32` with 1/65536 units: WGSL's float-to-i32 conversion is
/// indeterminate above `i32::MAX`, so the shader saturates the scaled product at this value,
/// the largest whole speed that keeps `1.0 * v * 65536` inside `i32` with room for the weight's
/// rounding. Uploads clamp to the same ceiling so the CPU reference and the GPU see the same
/// liquid (the CPU solver itself is pure f32 and needs no clamp).
pub const MAX_FIXEDPOINT_SPEED: f32 = 32767.0;

/// Why the GPU path could not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GpuFluidError {
    /// The read-back failed.
    #[error("GPU read-back failed: {0}")]
    ReadBack(String),
    /// The simulation is too big for one dispatch row.
    #[error("the simulation needs {0} workgroups in one dimension (most 65535)")]
    TooBig(u32),
    /// Android runs the CPU path only (ADR-2441: mobile gets the particle cap and the CPU path).
    #[error("the GPU fluid path is not available on mobile; use the CPU solver")]
    MobileCpuOnly,
    /// More moving solids than the GPU path holds.
    #[error("{0} moving solids (the GPU path takes at most {MAX_GPU_SOLIDS})")]
    TooManySolids(usize),
}

impl GpuFluidError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ReadBack(_) => "fluid_gpu_readback",
            Self::TooBig(_) => "fluid_gpu_too_big",
            Self::MobileCpuOnly => "fluid_gpu_mobile",
            Self::TooManySolids(_) => "fluid_gpu_too_many_solids",
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    n: [u32; 4],
    off: [u32; 4],
    origin_dx: [f32; 4],
    accel_dt: [f32; 4],
    misc: [f32; 4],
    solid_info: [u32; 4],
    solids: [GpuSolid; MAX_GPU_SOLIDS],
}

/// One moving solid in the uniform (WGSL `Solid`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuSolid {
    shape: [f32; 4],
    pos: [f32; 4],
    rot: [f32; 4],
    lin: [f32; 4],
    ang: [f32; 4],
}

impl GpuSolid {
    fn from_solid(s: &MovingSolid) -> Self {
        let shape = match s.shape {
            SolidShape::Box { half_extents } => [0.0, half_extents.x, half_extents.y, half_extents.z],
            SolidShape::Sphere { radius } => [1.0, radius, 0.0, 0.0],
            SolidShape::Capsule { half_height, radius } => [2.0, half_height, radius, 0.0],
        };
        let v4 = |v: Vec3| [v.x, v.y, v.z, 0.0];
        Self {
            shape,
            pos: v4(s.position),
            rot: s.rotation.to_array(),
            lin: v4(s.linear_velocity),
            ang: v4(s.angular_velocity),
        }
    }
}

fn solid_block(solids: &[MovingSolid]) -> Result<(u32, [GpuSolid; MAX_GPU_SOLIDS]), GpuFluidError> {
    if solids.len() > MAX_GPU_SOLIDS {
        return Err(GpuFluidError::TooManySolids(solids.len()));
    }
    let mut out = [GpuSolid::default(); MAX_GPU_SOLIDS];
    for (slot, s) in out.iter_mut().zip(solids) {
        *slot = GpuSolid::from_solid(s);
    }
    Ok((solids.len() as u32, out))
}

/// A simulation's particles and grid on the GPU.
pub struct GpuFluid {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipelines: Vec<(&'static str, wgpu::ComputePipeline)>,
    bind_group: wgpu::BindGroup,
    pos: wgpu::Buffer,
    vel: wgpu::Buffer,
    uniform_buf: wgpu::Buffer,
    uniform: Params,
    cellf: wgpu::Buffer,
    particles: u32,
    faces: u32,
    cells: u32,
    /// Red-black SOR sweeps per substep.
    pub iterations: u32,
    /// Substeps per [`GpuFluid::step`] (each `dt / substeps`).
    pub substeps: u32,
}

impl std::fmt::Debug for GpuFluid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuFluid")
            .field("particles", &self.particles)
            .field("faces", &self.faces)
            .field("cells", &self.cells)
            .field("iterations", &self.iterations)
            .field("substeps", &self.substeps)
            .finish()
    }
}

fn groups(n: u32) -> Result<u32, GpuFluidError> {
    let g = n.div_ceil(WORKGROUP).max(1);
    if g > 65_535 {
        return Err(GpuFluidError::TooBig(g));
    }
    Ok(g)
}

impl GpuFluid {
    /// Uploads `sim`'s particles, its open-face fractions and its settings.
    ///
    /// On Android this returns [`GpuFluidError::MobileCpuOnly`]: mobile runs the CPU solver with
    /// its lower caps.
    ///
    /// A particle whose position or velocity is not finite is dropped, as the CPU solver drops it
    /// at the end of a step: `f32::clamp` keeps NaN and a fast-math shader compiler can fold a
    /// `v == v` guard, so the upload must not rely on the kernel's `finite3` alone (ADR-5730).
    /// [`GpuFluid::sync_into`] then hands the kept set back, so the dropped particle leaves `sim`
    /// as well.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, sim: &FluidSim) -> Result<Self, GpuFluidError> {
        Self::upload(device, queue, sim, true)
    }

    /// [`GpuFluid::new`] with the non-finite particle filter switchable. Only the kernel-guard
    /// test passes `drop_non_finite: false`, so the shader's `finite3` sees the NaN (ADR-5875).
    fn upload(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        sim: &FluidSim,
        drop_non_finite: bool,
    ) -> Result<Self, GpuFluidError> {
        if cfg!(target_os = "android") {
            return Err(GpuFluidError::MobileCpuOnly);
        }
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_gpu_upload").entered();
        let d = *sim.dims();
        let params = sim.params();
        let substeps = params.max_substeps.max(1);
        let dt = params.dt / substeps as f32;
        let sizes = [d.face_dims(0), d.face_dims(1), d.face_dims(2)];
        let count = |s: [usize; 3]| (s[0] * s[1] * s[2]) as u32;
        let (nu, nv, nw) = (count(sizes[0]), count(sizes[1]), count(sizes[2]));
        let faces = nu + nv + nw;
        let cells = d.cells() as u32;
        // The CPU solver drops a particle whose state is not finite at the end of a step; the
        // upload does the same so no backend's shader sees the NaN (ADR-5730).
        let mut kept_pos: Vec<Vec3> = Vec::with_capacity(sim.positions().len());
        let mut kept_vel: Vec<Vec3> = Vec::with_capacity(sim.positions().len());
        for (p, v) in sim.positions().iter().zip(sim.velocities()) {
            if !drop_non_finite || (p.is_finite() && v.is_finite()) {
                kept_pos.push(*p);
                kept_vel.push(*v);
            }
        }
        let particles = kept_pos.len() as u32;
        groups(particles.max(4 * faces).max(cells))?;
        let accel = params.gravity + params.external_acceleration;
        let (solid_count, solids) = solid_block(sim.moving_solids())?;
        let uniform = Params {
            n: [d.nx as u32, d.ny as u32, d.nz as u32, particles],
            off: [0, nu, nu + nv, faces],
            origin_dx: [d.origin.x, d.origin.y, d.origin.z, d.dx],
            accel_dt: [accel.x, accel.y, accel.z, dt],
            misc: [
                params.flip_ratio,
                dt / (params.density * d.dx),
                DEFAULT_OMEGA,
                0.0,
            ],
            solid_info: [solid_count, 0, 0, 0],
            solids,
        };
        let storage =
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST;
        let vec4s = |v: &[Vec3]| -> Vec<[f32; 4]> {
            let mut out: Vec<[f32; 4]> = v.iter().map(|p| [p.x, p.y, p.z, 0.0]).collect();
            if out.is_empty() {
                out.push([0.0; 4]);
            }
            out
        };
        // Speeds clamp to the fixed-point ceiling the p2g scatter saturates at, so the GPU and
        // the CPU reference integrate the same velocities (ADR-4753).
        let vec4s_clamped = |v: &[Vec3]| -> Vec<[f32; 4]> {
            let c = |x: f32| x.clamp(-MAX_FIXEDPOINT_SPEED, MAX_FIXEDPOINT_SPEED);
            let mut out: Vec<[f32; 4]> = v.iter().map(|p| [c(p.x), c(p.y), c(p.z), 0.0]).collect();
            if out.is_empty() {
                out.push([0.0; 4]);
            }
            out
        };
        let init = |label: &str, contents: &[u8], usage: wgpu::BufferUsages| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents,
                usage,
            })
        };
        let zeroed = |label: &str, bytes: u64| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes.max(16),
                usage: storage,
                mapped_at_creation: false,
            })
        };
        let uniform_buf = init(
            "fluid params",
            bytemuck::bytes_of(&uniform),
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let pos = init("fluid pos", bytemuck::cast_slice(&vec4s(&kept_pos)), storage);
        let vel = init(
            "fluid vel",
            bytemuck::cast_slice(&vec4s_clamped(&kept_vel)),
            storage,
        );
        // Per face: the velocity and weight sums, each a (low, high) pair of u32 words.
        let acc = zeroed("fluid acc", u64::from(faces) * 16);
        let grid = zeroed("fluid grid", u64::from(faces) * 8);
        // Per face: the static open fraction (the static colliders' field, without the moving
        // solids the CPU folded in), this substep's fraction, the solid velocity, and the static
        // colliders' distance at the face centre (a moving solid gives a closed face its velocity
        // only where it is at least as near, as on the CPU).
        let mut open: Vec<[f32; 4]> = Vec::with_capacity(faces as usize);
        for (axis, size) in sizes.iter().enumerate() {
            let fractions = sim.solid().static_open(axis);
            for k in 0..size[2] {
                for j in 0..size[1] {
                    for i in 0..size[0] {
                        let o = fractions[i + size[0] * (j + size[1] * k)];
                        let mut centre = Vec3::new(i as f32, j as f32, k as f32) + Vec3::splat(0.5);
                        centre[axis] -= 0.5;
                        let distance = sim.solid().static_distance(d.origin + centre * d.dx);
                        open.push([o, o, 0.0, distance]);
                    }
                }
            }
        }
        let open_buf = init("fluid face solids", bytemuck::cast_slice(&open), storage);
        let liquid = zeroed("fluid liquid", u64::from(cells) * 4);
        // Pressure, right-hand side, the substep counter of the moving solids' time, and the
        // static colliders' distance at every cell centre (the GPU counts a cell whose centre is
        // inside a solid and that touches the liquid as liquid, like the CPU).
        let mut cell_words = vec![0.0f32; 2 * cells as usize + 1];
        cell_words.reserve(cells as usize);
        for k in 0..d.nz {
            for j in 0..d.ny {
                for i in 0..d.nx {
                    cell_words.push(sim.solid().static_distance(d.cell_center(i, j, k)));
                }
            }
        }
        let cellf = init("fluid cells", bytemuck::cast_slice(&cell_words), storage);

        let cs = wgpu::ShaderStages::COMPUTE;
        let entry = |binding: u32, ty: wgpu::BufferBindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: cs,
            ty: wgpu::BindingType::Buffer {
                ty,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let rw = wgpu::BufferBindingType::Storage { read_only: false };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fluid bgl"),
            entries: &[
                entry(0, wgpu::BufferBindingType::Uniform),
                entry(1, rw),
                entry(2, rw),
                entry(3, rw),
                entry(4, rw),
                entry(5, rw),
                entry(6, rw),
                entry(7, rw),
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fluid bg"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: pos.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: vel.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: acc.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: grid.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: open_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: liquid.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: cellf.as_entire_binding(),
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fluid_flip.wgsl"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fluid layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipelines = [
            "clear",
            "rasterize",
            "p2g",
            "solid_liquid",
            "normalize",
            "forces",
            "divergence",
            "relax_red",
            "relax_black",
            "project",
            "g2p_advect",
        ]
        .into_iter()
        .map(|name| {
            let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(name),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(name),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            });
            (name, pipeline)
        })
        .collect();
        Ok(Self {
            device: device.clone(),
            queue: queue.clone(),
            pipelines,
            bind_group,
            pos,
            vel,
            uniform_buf,
            uniform,
            cellf,
            particles,
            faces,
            cells,
            iterations: DEFAULT_ITERATIONS,
            substeps,
        })
    }

    fn pipeline(&self, name: &str) -> Option<&wgpu::ComputePipeline> {
        self.pipelines.iter().find(|(n, _)| *n == name).map(|(_, p)| p)
    }

    /// Runs `steps` fixed steps (each `substeps` substeps) and submits them.
    pub fn step(&self, steps: u32) -> Result<(), GpuFluidError> {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_gpu_step").entered();
        let g_particles = groups(self.particles)?;
        let g_faces = groups(self.faces)?;
        let g_clear = groups((4 * self.faces).max(self.cells))?;
        let g_cells = groups(self.cells)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fluid steps"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("fluid substeps"),
                timestamp_writes: None,
            });
            pass.set_bind_group(0, Some(&self.bind_group), &[]);
            let mut run = |name: &str, groups: u32| {
                if let Some(p) = self.pipeline(name) {
                    pass.set_pipeline(p);
                    pass.dispatch_workgroups(groups, 1, 1);
                }
            };
            for _ in 0..steps.saturating_mul(self.substeps) {
                run("clear", g_clear);
                run("rasterize", g_faces);
                run("p2g", g_particles);
                run("solid_liquid", g_cells);
                run("normalize", g_faces);
                run("forces", g_faces);
                run("divergence", g_cells);
                for _ in 0..self.iterations {
                    run("relax_red", g_cells);
                    run("relax_black", g_cells);
                }
                run("project", g_faces);
                run("g2p_advect", g_particles);
            }
        }
        self.queue.submit([encoder.finish()]);
        Ok(())
    }

    /// Replaces the moving solids (their pose at the start of the next step; they move at their
    /// velocities through every following substep until set again). At most [`MAX_GPU_SOLIDS`].
    pub fn set_moving_solids(&mut self, solids: &[MovingSolid]) -> Result<(), GpuFluidError> {
        let (count, block) = solid_block(solids)?;
        self.uniform.solid_info = [count, 0, 0, 0];
        self.uniform.solids = block;
        self.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&self.uniform));
        self.queue
            .write_buffer(&self.cellf, u64::from(self.cells) * 8, &0.0f32.to_le_bytes());
        Ok(())
    }

    /// Reads the particles back (blocks until the GPU is done).
    pub fn read_back(&self) -> Result<(Vec<Vec3>, Vec<Vec3>), GpuFluidError> {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_gpu_readback").entered();
        let bytes = u64::from(self.particles.max(1)) * 16;
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fluid readback"),
            size: bytes * 2,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fluid readback"),
            });
        encoder.copy_buffer_to_buffer(&self.pos, 0, &staging, 0, bytes);
        encoder.copy_buffer_to_buffer(&self.vel, 0, &staging, bytes, bytes);
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let result = (|| {
            self.device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(|e| GpuFluidError::ReadBack(e.to_string()))?;
            rx.recv()
                .map_err(|e| GpuFluidError::ReadBack(e.to_string()))?
                .map_err(|e| GpuFluidError::ReadBack(e.to_string()))?;
            let data = slice
                .get_mapped_range()
                .map_err(|e| GpuFluidError::ReadBack(e.to_string()))?;
            let floats: &[[f32; 4]] = bytemuck::cast_slice(&data[..]);
            let n = self.particles as usize;
            let pos = floats[..n].iter().map(|v| Vec3::new(v[0], v[1], v[2])).collect();
            let vel = floats[(bytes / 16) as usize..(bytes / 16) as usize + n]
                .iter()
                .map(|v| Vec3::new(v[0], v[1], v[2]))
                .collect();
            Ok((pos, vel))
        })();
        staging.unmap();
        result
    }

    /// Hands the GPU particles back to `sim` and advances its step counter by `steps`.
    pub fn sync_into(&self, sim: &mut FluidSim, steps: u64) -> Result<(), GpuFluidError> {
        let (pos, vel) = self.read_back()?;
        sim.set_particles(pos, vel);
        sim.advance_step_counter(steps);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::GpuFluid;
    use crate::sim::{FluidParams, FluidSim};
    use glam::Vec3;
    use regia_render::RenderDevice;

    /// The kernel's own non-finite guard (`finite3` in `fluid_flip.wgsl`), with the upload filter
    /// off so the NaN and ∞ reach `p2g`. WGSL `clamp` turns a NaN into the low bound, so a missed
    /// guard scatters -32767 m/s onto every neighbour face: with `finite3` forced to `true`,
    /// lavapipe read the rest neighbour back at 1997.3 m/s after one step (ADR-5875).
    #[test]
    fn kernel_guard_skips_a_non_finite_particle_the_upload_kept() {
        let device = match RenderDevice::shared_headless() {
            Ok(device) => device,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        };
        let mut params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.4),
            cell_size: 0.1,
            dt: 1.0 / 60.0,
            max_substeps: 1,
            gravity: Vec3::ZERO,
            separation: 0.0,
            reseed: false,
            ..FluidParams::default()
        };
        params.whitewater.enabled = false;
        let mut sim = FluidSim::new(params).expect("sim");
        sim.set_particles(
            vec![
                Vec3::new(0.15, 0.15, 0.15),
                Vec3::new(0.16, 0.15, 0.15),
                Vec3::new(f32::NAN, 0.15, 0.15),
                Vec3::new(0.15, f32::INFINITY, 0.15),
            ],
            vec![Vec3::ZERO, Vec3::new(f32::NAN, 0.0, 0.0), Vec3::ZERO, Vec3::ZERO],
        );
        let gpu = GpuFluid::upload(&device.device, &device.queue, &sim, false).expect("upload");
        assert_eq!(
            gpu.particles, 4,
            "the unfiltered upload must keep the non-finite particles"
        );
        gpu.step(1).expect("gpu step");
        let (pos, vel) = gpu.read_back().expect("read back");
        let speed = vel[0].length();
        assert!(
            speed < 1.0 && pos[0].is_finite(),
            "the kernel guard let a non-finite particle launch its neighbour to vel {:?} pos {:?}",
            vel[0],
            pos[0]
        );
    }
}
