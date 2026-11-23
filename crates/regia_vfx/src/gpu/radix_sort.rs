// SPDX-License-Identifier: MIT
//! GPU radix sort of `(key, payload)` pairs whose count lives on the GPU.
//!
//! Portions ported from FidelityFX SDK (<https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK>,
//! commit c6efa6b, tag v1.1.4 only; the SDK's later main branch is under a different licence),
//! FidelityFX Parallel Sort. Copyright (C) 2024 Advanced Micro Devices, Inc., MIT licence:
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy of this
//! software and associated documentation files (the "Software"), to deal in the Software
//! without restriction, including without limitation the rights to use, copy, modify, merge,
//! publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons
//! to whom the Software is furnished to do so, subject to the following conditions: The above
//! copyright notice and this permission notice shall be included in all copies or substantial
//! portions of the Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
//! EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
//! FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT
//! HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF
//! CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
//! USE OR OTHER DEALINGS IN THE SOFTWARE.
//!
//! Least-significant-digit radix sort, 4 bits per pass: per pass `count` (per-group digit
//! histograms), `reduce` (per-bin partial sums), `scan` (prefix of the partials, one group),
//! `scan_add` (prefix of each bin's group sums plus its partial) and `scatter` (a stable
//! split of each 128-key round in workgroup memory, then the global write). A `setup`
//! pass derives the dispatch sizes and constants from the count the simulation wrote, so
//! the sort costs what is alive, not the pool width. The sort is stable, so equal keys
//! keep the order they were gathered in, unless a [`RadixTieBreak`] first sorts by payload
//! bits (Regia Engine addition: least-significant-digit passes over the payload before the key
//! passes, so equal keys come out ordered by payload whatever their input order).

use wgpu::util::DeviceExt;

/// Keys per thread group per block (`FFX_PARALLELSORT_ELEMENTS_PER_THREAD` × group size).
const BLOCK: u32 = 512;
/// Digit bins per pass (4 bits).
const BINS: u32 = 16;
/// Upper bound on thread groups per pass (`FFX_PARALLELSORT_MAX_THREADGROUPS_TO_RUN`).
const MAX_TG: u32 = 800;
/// Dynamic-offset stride of the per-pass uniform blocks.
const PASS_STRIDE: u64 = 256;

/// The two WGSL modules (`setup`, sort passes) for `cargo xtask check-shaders`.
#[must_use]
pub fn shader_sources() -> Vec<(String, String)> {
    vec![
        (
            "vfx radix setup".to_string(),
            include_str!("radix_setup.wgsl").to_string(),
        ),
        (
            "vfx radix sort".to_string(),
            include_str!("radix_sort.wgsl").to_string(),
        ),
    ]
}

/// Where the pairs to sort live and how many there are.
pub struct RadixSortTarget<'a> {
    /// Buffer of `vec2<u32>` (key, payload) pairs; sorted in place.
    pub pairs: &'a wgpu::Buffer,
    /// First pair of the range, in pairs.
    pub base: u32,
    /// Most pairs the range can hold.
    pub max_keys: u32,
    /// `array<u32>` holding the live count.
    pub counters: &'a wgpu::Buffer,
    /// Index of the count in `counters`.
    pub count_index: u32,
    /// Key bits that matter (rounded up to whole 4-bit passes; an even pass count keeps
    /// the result in `pairs`). 32 sorts the full key.
    pub key_bits: u32,
}

/// Payload bits a [`GpuRadixSort`] sorts by before the key, so equal keys come out in payload
/// order instead of input order ([`GpuRadixSort::with_tie_break`]). The VFX alpha list breaks
/// ties on the pool-slot bits its key does not already hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RadixTieBreak {
    /// Lowest payload bit to sort by.
    pub shift: u32,
    /// Payload bits to sort by from `shift` (0: no tie-break). Bits past 31 are ignored.
    pub bits: u32,
}

/// Per-pass digit plan: `(shift, sorts the payload)` per 4-bit pass, payload passes first.
/// The count is even, so the sorted pairs end in the target range: an odd plan gets one
/// more pass on a higher key digit, or (a full 32-bit key) one more payload digit below or
/// above the tie-break bits. Either extra digit only refines the order.
fn pass_plan(key_bits: u32, tie: RadixTieBreak) -> Vec<(u32, bool)> {
    let mut key_passes = key_bits.clamp(4, 32).div_ceil(4);
    let mut tie_lo = tie.shift.min(31);
    let mut tie_passes = tie.bits.min(32 - tie_lo).div_ceil(4);
    if !(key_passes + tie_passes).is_multiple_of(2) {
        if key_passes < 8 {
            key_passes += 1;
        } else if tie_lo >= 4 {
            tie_lo -= 4;
            tie_passes += 1;
        } else {
            // `tie_passes` is odd (at most 7) and `tie_lo` below 4: the digit on top starts
            // below bit 32.
            tie_passes += 1;
        }
    }
    (0..tie_passes)
        .map(|i| (tie_lo + 4 * i, true))
        .chain((0..key_passes).map(|i| (4 * i, false)))
        .collect()
}

/// A radix sort bound to one key range (see [`RadixSortTarget`]).
pub struct GpuRadixSort {
    pipe_setup: wgpu::ComputePipeline,
    pipe_count: wgpu::ComputePipeline,
    pipe_reduce: wgpu::ComputePipeline,
    pipe_scan: wgpu::ComputePipeline,
    pipe_scan_add: wgpu::ComputePipeline,
    pipe_scatter: wgpu::ComputePipeline,
    bg_setup: wgpu::BindGroup,
    /// Pass data: `[pairs → scratch, scratch → pairs]`.
    bg_data: [wgpu::BindGroup; 2],
    bg_pass: wgpu::BindGroup,
    indirect: wgpu::Buffer,
    passes: u32,
}

impl GpuRadixSort {
    /// Builds the pipelines and scratch buffers for `target`.
    #[must_use]
    pub fn new(device: &wgpu::Device, target: &RadixSortTarget<'_>) -> Self {
        Self::with_tie_break(device, target, RadixTieBreak::default())
    }

    /// [`Self::new`] that orders equal keys by the payload bits of `tie` (see
    /// [`RadixTieBreak`]); each 4 tie-break bits cost one more pass.
    #[must_use]
    pub fn with_tie_break(device: &wgpu::Device, target: &RadixSortTarget<'_>, tie: RadixTieBreak) -> Self {
        let d = device;
        let max_keys = target.max_keys.max(1);
        // An even number of 4-bit passes, so the sorted keys end where they started.
        let plan = pass_plan(target.key_bits, tie);
        let passes = plan.len() as u32;
        let blocks = max_keys.div_ceil(BLOCK);
        let groups = blocks.clamp(1, MAX_TG);
        let reduce_groups = BINS * groups.div_ceil(BLOCK);
        let storage = |label: &str, size: u64, extra: wgpu::BufferUsages| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(16),
                usage: wgpu::BufferUsages::STORAGE | extra,
                mapped_at_creation: false,
            })
        };
        let scratch = storage(
            "vfx radix scratch",
            u64::from(max_keys) * 8,
            wgpu::BufferUsages::empty(),
        );
        let sums = storage(
            "vfx radix sums",
            u64::from(BINS * groups) * 4,
            wgpu::BufferUsages::empty(),
        );
        let reduced = storage(
            "vfx radix reduce",
            u64::from(reduce_groups.max(BINS)) * 4,
            wgpu::BufferUsages::empty(),
        );
        let consts = storage("vfx radix constants", 32, wgpu::BufferUsages::empty());
        let indirect = storage("vfx radix dispatch", 24, wgpu::BufferUsages::INDIRECT);
        let setup_params = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vfx radix setup params"),
            contents: bytemuck::cast_slice(&[target.count_index, max_keys, 0u32, 0u32]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        // Pass blocks: shift, source base, destination base, payload digit (1) or key digit
        // (0). Even passes read the target.
        let mut blocks_bytes = vec![0u8; (PASS_STRIDE * u64::from(passes)) as usize];
        for (pass, &(shift, payload)) in plan.iter().enumerate() {
            let (src_base, dst_base) = if pass.is_multiple_of(2) {
                (target.base, 0)
            } else {
                (0, target.base)
            };
            let at = pass * PASS_STRIDE as usize;
            blocks_bytes[at..at + 16].copy_from_slice(bytemuck::cast_slice(&[
                shift,
                src_base,
                dst_base,
                u32::from(payload),
            ]));
        }
        let pass_params = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vfx radix pass params"),
            contents: &blocks_bytes,
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let cs = wgpu::ShaderStages::COMPUTE;
        let entry = |binding: u32, ty: wgpu::BufferBindingType, dynamic: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: cs,
            ty: wgpu::BindingType::Buffer {
                ty,
                has_dynamic_offset: dynamic,
                min_binding_size: None,
            },
            count: None,
        };
        let ro = wgpu::BufferBindingType::Storage { read_only: true };
        let rw = wgpu::BufferBindingType::Storage { read_only: false };
        let uniform = wgpu::BufferBindingType::Uniform;
        let bgl_setup = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx radix setup bgl"),
            entries: &[
                entry(0, ro, false),
                entry(1, rw, false),
                entry(2, rw, false),
                entry(3, uniform, false),
            ],
        });
        let bgl_data = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx radix data bgl"),
            entries: &[
                entry(0, ro, false),
                entry(1, rw, false),
                entry(2, rw, false),
                entry(3, rw, false),
                entry(4, ro, false),
            ],
        });
        let bgl_pass = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx radix pass bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: cs,
                ty: wgpu::BindingType::Buffer {
                    ty: uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(16),
                },
                count: None,
            }],
        });
        let bg_setup = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx radix setup bg"),
            layout: &bgl_setup,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: target.counters.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: consts.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: indirect.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: setup_params.as_entire_binding(),
                },
            ],
        });
        let data = |label: &str, src: &wgpu::Buffer, dst: &wgpu::Buffer| {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &bgl_data,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: src.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: dst.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: sums.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: reduced.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: consts.as_entire_binding(),
                    },
                ],
            })
        };
        let bg_data = [
            data("vfx radix data even", target.pairs, &scratch),
            data("vfx radix data odd", &scratch, target.pairs),
        ];
        let bg_pass = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx radix pass bg"),
            layout: &bgl_pass,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &pass_params,
                    offset: 0,
                    size: wgpu::BufferSize::new(16),
                }),
            }],
        });
        let module = |label: &str, source: &'static str| {
            d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            })
        };
        let setup_module = module("vfx radix setup", include_str!("radix_setup.wgsl"));
        let sort_module = module("vfx radix sort", include_str!("radix_sort.wgsl"));
        let layout = |label: &str, bgls: &[&wgpu::BindGroupLayout]| {
            let refs: Vec<Option<&wgpu::BindGroupLayout>> = bgls.iter().map(|b| Some(*b)).collect();
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &refs,
                immediate_size: 0,
            })
        };
        let setup_layout = layout("vfx radix setup layout", &[&bgl_setup]);
        let sort_layout = layout("vfx radix sort layout", &[&bgl_data, &bgl_pass]);
        let pipe = |label: &str, m: &wgpu::ShaderModule, entry_point: &str, l: &wgpu::PipelineLayout| {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(l),
                module: m,
                entry_point: Some(entry_point),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
            })
        };
        Self {
            pipe_setup: pipe("vfx radix setup", &setup_module, "setup", &setup_layout),
            pipe_count: pipe("vfx radix count", &sort_module, "count", &sort_layout),
            pipe_reduce: pipe("vfx radix reduce", &sort_module, "reduce", &sort_layout),
            pipe_scan: pipe("vfx radix scan", &sort_module, "scan", &sort_layout),
            pipe_scan_add: pipe("vfx radix scan add", &sort_module, "scan_add", &sort_layout),
            pipe_scatter: pipe("vfx radix scatter", &sort_module, "scatter", &sort_layout),
            bg_setup,
            bg_data,
            bg_pass,
            indirect,
            passes,
        }
    }

    /// 4-bit passes this sort runs (always even).
    #[must_use]
    pub fn passes(&self) -> u32 {
        self.passes
    }

    /// Records the sort into `pass`, after whatever wrote the keys and the count in the same
    /// compute pass (dispatches in one pass are ordered).
    pub fn record(&self, pass: &mut wgpu::ComputePass<'_>) {
        pass.set_pipeline(&self.pipe_setup);
        pass.set_bind_group(0, Some(&self.bg_setup), &[]);
        pass.dispatch_workgroups(1, 1, 1);
        for p in 0..self.passes {
            let offset = (u64::from(p) * PASS_STRIDE) as u32;
            pass.set_bind_group(0, Some(&self.bg_data[(p % 2) as usize]), &[]);
            pass.set_bind_group(1, Some(&self.bg_pass), &[offset]);
            pass.set_pipeline(&self.pipe_count);
            pass.dispatch_workgroups_indirect(&self.indirect, 0);
            pass.set_pipeline(&self.pipe_reduce);
            pass.dispatch_workgroups_indirect(&self.indirect, 12);
            pass.set_pipeline(&self.pipe_scan);
            pass.dispatch_workgroups(1, 1, 1);
            pass.set_pipeline(&self.pipe_scan_add);
            pass.dispatch_workgroups_indirect(&self.indirect, 12);
            pass.set_pipeline(&self.pipe_scatter);
            pass.dispatch_workgroups_indirect(&self.indirect, 0);
        }
    }
}

/// Bytes of GPU scratch a sort of `max_keys` pairs allocates (pairs, sums, partials).
#[must_use]
pub fn scratch_bytes(max_keys: u32) -> u64 {
    let blocks = max_keys.max(1).div_ceil(BLOCK);
    let groups = blocks.clamp(1, MAX_TG);
    u64::from(max_keys.max(1)) * 8
        + u64::from(BINS * groups) * 4
        + u64::from((BINS * groups.div_ceil(BLOCK)).max(BINS)) * 4
}

#[cfg(test)]
mod tests {
    use super::{RadixTieBreak, pass_plan};

    /// Without a tie-break the plan is the key digits, rounded up to an even count.
    #[test]
    fn plain_plans_round_the_key_up_to_even() {
        assert_eq!(pass_plan(32, RadixTieBreak::default()).len(), 8);
        assert_eq!(
            pass_plan(20, RadixTieBreak::default()),
            vec![
                (0, false),
                (4, false),
                (8, false),
                (12, false),
                (16, false),
                (20, false)
            ]
        );
    }

    /// The tie-break runs first on payload digits; an odd count gets a key digit when the key
    /// is short, else a payload digit below the tie-break bits.
    #[test]
    fn tie_break_plans_sort_payload_digits_first_and_stay_even() {
        // A 131 072-slot pool: slot bits 12..17 above the 12 the key holds.
        let plan = pass_plan(32, RadixTieBreak { shift: 12, bits: 5 });
        assert_eq!(&plan[..2], &[(12, true), (16, true)]);
        assert_eq!(plan.len(), 10);
        // An 8 192-slot pool: one tie digit, so one more below it.
        let plan = pass_plan(32, RadixTieBreak { shift: 12, bits: 1 });
        assert_eq!(&plan[..2], &[(8, true), (12, true)]);
        assert_eq!(plan.len(), 10);
        // A short key takes the extra digit itself.
        let plan = pass_plan(20, RadixTieBreak { shift: 12, bits: 5 });
        assert_eq!(plan.len(), 8);
        assert_eq!(&plan[..3], &[(12, true), (16, true), (0, false)]);
        assert_eq!(plan[7], (20, false));
        // A tie-break from bit 0 adds its extra digit on top, below bit 32.
        let plan = pass_plan(32, RadixTieBreak { shift: 0, bits: 28 });
        assert_eq!(plan.len(), 16);
        assert!(plan.iter().all(|&(shift, _)| shift < 32));
        for key_bits in [4, 13, 32] {
            for shift in [0, 3, 12, 30] {
                for bits in [0, 1, 5, 17, 32] {
                    let plan = pass_plan(key_bits, RadixTieBreak { shift, bits });
                    assert!(
                        plan.len().is_multiple_of(2),
                        "{key_bits} {shift} {bits}: odd plan"
                    );
                    assert!(
                        plan.iter().all(|&(s, _)| s < 32),
                        "{key_bits} {shift} {bits}: shift past 31"
                    );
                    assert!(
                        plan.windows(2).all(|w| w[0].1 || !w[1].1),
                        "payload digits must come before key digits"
                    );
                }
            }
        }
    }
}
