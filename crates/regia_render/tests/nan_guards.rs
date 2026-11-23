// SPDX-License-Identifier: MPL-2.0
//! The render shaders' NaN and infinity guards, run on the GPU (lavapipe in CI; ADR-869).
//!
//! Each case takes guard functions out of the real shader files (not copies), composes them
//! with a one-invocation compute kernel, feeds them degenerate inputs (zero vectors,
//! coincident points, NaN, ±∞) as raw `f32` bits through a storage buffer and reads the
//! results back. What they prove:
//!
//! - `ddgi_depth_normal` (DDGI and baked-GI screen lookups): a screen-border texel, whose
//!   clamped neighbor is itself, still gets the plane's normal; both differences zero, or
//!   NaN/∞ depths, give the view direction; an interior texel is unchanged.
//! - `ddgi_dir_to_probe`: a surface through a probe takes the surface normal, not
//!   `normalize(0)`.
//! - `ddgi_finite_or3` / `ddgi_finite_or2` (DDGI history blend) and `finite_or4` (cloud and
//!   froxel-fog history writes): every NaN and ±∞ is replaced, every finite value (including
//!   zero, negatives, denormal-free tiny values and `f32::MAX`) passes through bit for bit; the
//!   same holds on a computed value, which a driver compiler may assume finite (ADR-3150).
//! - `safe_dir` (gizmo lines and joints): a zero or NaN screen vector takes the fallback.

use std::borrow::Cow;
use std::sync::{Mutex, MutexGuard, PoisonError};

use regia_render::RenderDevice;

const DDGI_COMMON: &str = include_str!("../shaders/ddgi_common.wgsl");
const CLOUDS: &str = include_str!("../shaders/clouds.wgsl");
const FOG: &str = include_str!("../shaders/volumetric_fog.wgsl");
const GIZMOS: &str = include_str!("../shaders/gizmos.wgsl");

/// One test at a time. The default test harness runs the four cases on parallel threads, and
/// each creates and destroys its own Vulkan device; on a real NVIDIA driver (615.71) that
/// concurrent device churn live-locks inside the driver (a thread spinning in
/// `vkDestroyDevice` while others wait in `vkDestroyDescriptorPool`; ADR-1438).
static GPU: Mutex<()> = Mutex::new(());

/// Takes the process-wide GPU lock, then opens a headless device. Bind the guard before the
/// device (`let (_gpu, device) = device();`) so the device is dropped while the lock is held.
fn device() -> (MutexGuard<'static, ()>, RenderDevice) {
    let guard = GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless GPU (lavapipe in CI)");
    (guard, device)
}

/// The text of `fn name(` through its closing brace, from a shader file.
fn wgsl_fn(source: &str, name: &str) -> String {
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("`fn {name}` not found"));
    let open = start + source[start..].find('{').expect("function body");
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[start..=open + offset].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("`fn {name}` is not closed");
}

/// Runs `body` once per output (`i` = output index) with `nan_in` and `nan_out` bound, after
/// `functions`, and returns `nan_out`.
fn run(
    device: &RenderDevice,
    functions: &str,
    body: &str,
    inputs: &[[f32; 4]],
    outputs: usize,
) -> Vec<[f32; 4]> {
    let source = format!(
        "@group(0) @binding(0) var<storage, read> nan_in: array<vec4<f32>>;\n\
         @group(0) @binding(1) var<storage, read_write> nan_out: array<vec4<f32>>;\n\
         {functions}\n\
         @compute @workgroup_size(1)\n\
         fn nan_kernel(@builtin(global_invocation_id) id: vec3<u32>) {{\n\
             let i = id.x;\n\
             {body}\n\
         }}\n"
    );
    let d = &device.device;
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("nan guards"),
        source: wgpu::ShaderSource::Wgsl(Cow::Owned(source)),
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("nan guards"),
        layout: None,
        module: &module,
        entry_point: Some("nan_kernel"),
        compilation_options: Default::default(),
        cache: None,
    });
    let in_bytes: Vec<u8> = inputs
        .iter()
        .flat_map(|v| v.iter().flat_map(|c| c.to_bits().to_le_bytes()))
        .collect();
    let out_size = (outputs.max(1) * 16) as u64;
    let input = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("nan guards in"),
        size: in_bytes.len().max(16) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    device.queue.write_buffer(&input, 0, &in_bytes);
    let output = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("nan guards out"),
        size: out_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("nan guards readback"),
        size: out_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("nan guards"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: input.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: output.as_entire_binding(),
            },
        ],
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("nan guards"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("nan guards"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(outputs as u32, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&output, 0, &readback, 0, out_size);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    d.poll(wgpu::PollType::wait_indefinitely())
        .expect("poll readback");
    rx.recv().expect("map callback").expect("map readback");
    let mapped = readback.slice(..).get_mapped_range().expect("mapped range");
    let (texels, _) = mapped.as_chunks::<16>();
    let values: Vec<[f32; 4]> = texels
        .iter()
        .take(outputs)
        .map(|texel| {
            let (words, _) = texel.as_chunks::<4>();
            let mut v = [0.0f32; 4];
            for (out, word) in v.iter_mut().zip(words) {
                *out = f32::from_bits(u32::from_le_bytes(*word));
            }
            v
        })
        .collect();
    drop(mapped);
    readback.unmap();
    values
}

fn finite(v: [f32; 4]) -> bool {
    v.iter().all(|c| c.is_finite())
}

fn close3(a: [f32; 4], b: [f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
}

const NAN: f32 = f32::NAN;
const INF: f32 = f32::INFINITY;

#[test]
fn depth_normal_is_finite_at_the_border_and_for_bad_depths() {
    let (_gpu, device) = device();
    let functions = wgsl_fn(DDGI_COMMON, "ddgi_depth_normal");
    let body = "let k = i * 5u;\n\
                nan_out[i] = vec4<f32>(ddgi_depth_normal(nan_in[k].xyz, nan_in[k + 1u].xyz, \
                nan_in[k + 2u].xyz, nan_in[k + 3u].xyz, nan_in[k + 4u].xyz), 0.0);";
    let up = [0.0, 1.0, 0.0, 0.0];
    let v = [0.6, 0.8, 0.0, 0.0];
    let zero = [0.0; 4];
    // Ground plane y = 0: screen x runs along world +x, screen y along world +z.
    let cases: Vec<([[f32; 4]; 5], [f32; 3])> = vec![
        // Interior texel: both differences on each axis, the smaller taken.
        (
            [
                [0.01, 0.0, 0.0, 0.0],
                [0.02, 0.0, 0.0, 0.0],
                [0.0, 0.0, 0.01, 0.0],
                [0.0, 0.0, 0.03, 0.0],
                up,
            ],
            [0.0, 1.0, 0.0],
        ),
        // Left and top border: the clamped neighbor gives l = 0 and b = 0.
        (
            [[0.01, 0.0, 0.0, 0.0], zero, [0.0, 0.0, 0.01, 0.0], zero, up],
            [0.0, 1.0, 0.0],
        ),
        // Right and bottom border: r = 0 and u = 0.
        (
            [zero, [0.01, 0.0, 0.0, 0.0], zero, [0.0, 0.0, 0.01, 0.0], up],
            [0.0, 1.0, 0.0],
        ),
        // A one-texel-wide target: both x differences zero. The view direction.
        ([zero, zero, [0.0, 0.0, 0.01, 0.0], zero, v], [0.6, 0.8, 0.0]),
        // Facing: a viewer below the plane gets −Y.
        (
            [
                [0.01, 0.0, 0.0, 0.0],
                zero,
                [0.0, 0.0, 0.01, 0.0],
                zero,
                [0.0, -1.0, 0.0, 0.0],
            ],
            [0.0, -1.0, 0.0],
        ),
        // A NaN neighbor is never taken while the other side is finite.
        (
            [
                [NAN, 0.0, 0.0, 0.0],
                [0.01, 0.0, 0.0, 0.0],
                [0.0, 0.0, 0.01, 0.0],
                zero,
                up,
            ],
            [0.0, 1.0, 0.0],
        ),
        // An infinite neighbor is the longer difference: the finite one is taken.
        (
            [
                [INF, 0.0, 0.0, 0.0],
                [0.01, 0.0, 0.0, 0.0],
                [0.0, 0.0, 0.01, 0.0],
                zero,
                up,
            ],
            [0.0, 1.0, 0.0],
        ),
        // Every depth NaN: the view direction.
        ([[NAN; 4], [NAN; 4], [NAN; 4], [NAN; 4], v], [0.6, 0.8, 0.0]),
        // Collinear differences (an edge-on sliver): the view direction.
        (
            [[0.01, 0.0, 0.0, 0.0], zero, [0.02, 0.0, 0.0, 0.0], zero, v],
            [0.6, 0.8, 0.0],
        ),
    ];
    let inputs: Vec<[f32; 4]> = cases
        .iter()
        .flat_map(|(input, _)| input.iter().copied())
        .collect();
    let out = run(&device, &functions, body, &inputs, cases.len());
    for (k, ((_, want), got)) in cases.iter().zip(&out).enumerate() {
        assert!(finite(*got), "case {k}: {got:?}");
        assert!(close3(*got, *want), "case {k}: got {got:?}, want {want:?}");
    }
}

#[test]
fn probe_direction_on_the_probe_is_the_normal() {
    let (_gpu, device) = device();
    let functions = wgsl_fn(DDGI_COMMON, "ddgi_dir_to_probe");
    let body = "let k = i * 3u;\n\
                nan_out[i] = vec4<f32>(ddgi_dir_to_probe(nan_in[k].xyz, nan_in[k + 1u].xyz, nan_in[k + 2u].xyz), 0.0);";
    let n = [0.0, 0.0, 1.0, 0.0];
    let cases: Vec<([[f32; 4]; 3], [f32; 3])> = vec![
        ([[1.0, 2.0, 3.0, 0.0], [1.0, 2.0, 3.0, 0.0], n], [0.0, 0.0, 1.0]),
        (
            [[1.0, 2.0, 3.0, 0.0], [1.0, 2.0 + 1e-7, 3.0, 0.0], n],
            [0.0, 0.0, 1.0],
        ),
        ([[4.0, 0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], n], [1.0, 0.0, 0.0]),
        ([[0.0, -2.0, 0.0, 0.0], [0.0, 0.0, 0.0, 0.0], n], [0.0, -1.0, 0.0]),
        ([[NAN, 0.0, 0.0, 0.0], [0.0, 0.0, 0.0, 0.0], n], [0.0, 0.0, 1.0]),
    ];
    let inputs: Vec<[f32; 4]> = cases
        .iter()
        .flat_map(|(input, _)| input.iter().copied())
        .collect();
    let out = run(&device, &functions, body, &inputs, cases.len());
    for (k, ((_, want), got)) in cases.iter().zip(&out).enumerate() {
        assert!(finite(*got), "case {k}: {got:?}");
        assert!(close3(*got, *want), "case {k}: got {got:?}, want {want:?}");
    }
}

/// Every NaN and ±∞ component replaced, every finite one passed through bit for bit.
fn check_finite_or(name: &str, out: &[[f32; 4]], inputs: &[[f32; 4]], fallback: [f32; 4]) {
    for (k, (got, input)) in out.iter().zip(inputs).enumerate() {
        for (c, ((g, i), f)) in got.iter().zip(input).zip(fallback).enumerate() {
            if i.is_finite() {
                assert_eq!(g.to_bits(), i.to_bits(), "{name} case {k} component {c}: {got:?}");
            } else {
                assert_eq!(*g, f, "{name} case {k} component {c}: {got:?}");
            }
        }
    }
}

const SAMPLES: [[f32; 4]; 6] = [
    [0.25, -3.0, 1.0e-30, f32::MAX],
    [NAN, 1.0, 2.0, 3.0],
    [1.0, INF, 2.0, 3.0],
    [1.0, 2.0, f32::NEG_INFINITY, 0.0],
    [-0.5, 2.0, 3.0, NAN],
    [NAN, INF, f32::NEG_INFINITY, NAN],
];

#[test]
fn history_guards_replace_every_non_finite_component() {
    let (_gpu, device) = device();
    // Clouds and froxel fog: per component, fallback (9, 8, 7, 6).
    for (file, name) in [(CLOUDS, "clouds finite_or4"), (FOG, "fog finite_or4")] {
        let functions = wgsl_fn(file, "finite_or4");
        let body = "nan_out[i] = finite_or4(nan_in[i], vec4<f32>(9.0, 8.0, 7.0, 6.0));";
        let out = run(&device, &functions, body, &SAMPLES, SAMPLES.len());
        check_finite_or(name, &out, &SAMPLES, [9.0, 8.0, 7.0, 6.0]);
    }
    // DDGI irradiance history: the whole rgb falls back when any component is bad.
    let functions = wgsl_fn(DDGI_COMMON, "ddgi_finite_or3");
    let body = "nan_out[i] = vec4<f32>(ddgi_finite_or3(nan_in[i].xyz, vec3<f32>(5.0)), nan_in[i].w);";
    let out = run(&device, &functions, body, &SAMPLES, SAMPLES.len());
    for (k, (got, input)) in out.iter().zip(&SAMPLES).enumerate() {
        let bad = input[..3].iter().any(|c| !c.is_finite());
        for (g, i) in got[..3].iter().zip(&input[..3]) {
            if bad {
                assert_eq!(*g, 5.0, "rgb case {k}: {got:?}");
            } else {
                assert_eq!(g.to_bits(), i.to_bits(), "rgb case {k}: {got:?}");
            }
        }
    }
    // DDGI distance moments: the pair falls back together.
    let functions = wgsl_fn(DDGI_COMMON, "ddgi_finite_or2");
    let body = "nan_out[i] = vec4<f32>(ddgi_finite_or2(nan_in[i].xy, vec2<f32>(4.0, 16.0)), 0.0, 0.0);";
    let out = run(&device, &functions, body, &SAMPLES, SAMPLES.len());
    for (k, (got, input)) in out.iter().zip(&SAMPLES).enumerate() {
        if input[..2].iter().all(|c| c.is_finite()) {
            assert_eq!(got[0].to_bits(), input[0].to_bits(), "moments case {k}: {got:?}");
            assert_eq!(got[1].to_bits(), input[1].to_bits(), "moments case {k}: {got:?}");
        } else {
            assert_eq!([got[0], got[1]], [4.0, 16.0], "moments case {k}: {got:?}");
        }
    }
}

/// The history guards on a value the shader computed (`x * 3 + 1`), not one it loaded: a driver
/// compiler may assume arithmetic results are finite and fold a guard on them (the NVIDIA 610.88
/// Windows Vulkan compiler does so for the masked bit test on a `dot` result, ADR-3150). NaN and ±∞
/// stay non-finite through `x * 3 + 1` (and `f32::MAX * 3` overflows), so those components take
/// the fallback; every finite one is the computed value.
#[test]
fn history_guards_hold_on_computed_values() {
    let (_gpu, device) = device();
    let samples: [[f32; 4]; 6] = [
        [0.25, -3.0, 1.0e-30, 1.0e30],
        [NAN, 1.0, 2.0, 3.0],
        [1.0, INF, 2.0, 3.0],
        [1.0, 2.0, f32::NEG_INFINITY, 0.0],
        [-0.5, 2.0, 3.0, NAN],
        [NAN, INF, f32::NEG_INFINITY, f32::MAX],
    ];
    let computed = |v: f32| v * 3.0 + 1.0;
    let near = |got: f32, want: f32| got.is_finite() && (got - want).abs() <= want.abs() * 1.0e-6;
    for (file, name) in [(CLOUDS, "clouds finite_or4"), (FOG, "fog finite_or4")] {
        let functions = wgsl_fn(file, "finite_or4");
        let body =
            "nan_out[i] = finite_or4(nan_in[i] * 3.0 + vec4<f32>(1.0), vec4<f32>(9.0, 8.0, 7.0, 6.0));";
        let out = run(&device, &functions, body, &samples, samples.len());
        for (k, (got, input)) in out.iter().zip(&samples).enumerate() {
            for (c, (g, fallback)) in got.iter().zip([9.0, 8.0, 7.0, 6.0]).enumerate() {
                let want = computed(input[c]);
                if want.is_finite() {
                    assert!(near(*g, want), "{name} case {k} component {c}: {got:?}");
                } else {
                    assert_eq!(*g, fallback, "{name} case {k} component {c}: {got:?}");
                }
            }
        }
    }
    let functions = wgsl_fn(DDGI_COMMON, "ddgi_finite_or3");
    let body =
        "nan_out[i] = vec4<f32>(ddgi_finite_or3(nan_in[i].xyz * 3.0 + vec3<f32>(1.0), vec3<f32>(5.0)), 0.0);";
    let out = run(&device, &functions, body, &samples, samples.len());
    for (k, (got, input)) in out.iter().zip(&samples).enumerate() {
        let bad = input[..3].iter().any(|c| !computed(*c).is_finite());
        for (g, i) in got[..3].iter().zip(&input[..3]) {
            if bad {
                assert_eq!(*g, 5.0, "rgb case {k}: {got:?}");
            } else {
                assert!(near(*g, computed(*i)), "rgb case {k}: {got:?}");
            }
        }
    }
    let functions = wgsl_fn(DDGI_COMMON, "ddgi_finite_or2");
    let body = "nan_out[i] = vec4<f32>(ddgi_finite_or2(nan_in[i].xy * 3.0 + vec2<f32>(1.0), vec2<f32>(4.0, 16.0)), 0.0, 0.0);";
    let out = run(&device, &functions, body, &samples, samples.len());
    for (k, (got, input)) in out.iter().zip(&samples).enumerate() {
        if input[..2].iter().all(|c| computed(*c).is_finite()) {
            assert!(near(got[0], computed(input[0])), "moments case {k}: {got:?}");
            assert!(near(got[1], computed(input[1])), "moments case {k}: {got:?}");
        } else {
            assert_eq!([got[0], got[1]], [4.0, 16.0], "moments case {k}: {got:?}");
        }
    }
}

#[test]
fn gizmo_screen_directions_never_normalize_zero() {
    let (_gpu, device) = device();
    let functions = wgsl_fn(GIZMOS, "safe_dir");
    let body = "nan_out[i] = vec4<f32>(safe_dir(nan_in[i].xy, vec2<f32>(0.0, 1.0)), 0.0, 0.0);";
    let inputs = [
        [3.0, 4.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 0.0],
        [1.0e-9, 0.0, 0.0, 0.0],
        [NAN, 1.0, 0.0, 0.0],
        [INF, 0.0, 0.0, 0.0],
    ];
    let want = [[0.6, 0.8], [0.0, 1.0], [0.0, 1.0], [0.0, 1.0], [0.0, 1.0]];
    let out = run(&device, &functions, body, &inputs, inputs.len());
    for (k, (got, want)) in out.iter().zip(want).enumerate() {
        assert!(finite(*got), "case {k}: {got:?}");
        assert!(
            (got[0] - want[0]).abs() < 1e-5 && (got[1] - want[1]).abs() < 1e-5,
            "case {k}: {got:?}"
        );
    }
}

/// The renderer's other vector NaN/∞ guards (`finite_or_zero`, the `finite*` predicates, the OIT
/// `bounded` clamp) on values the shader computed (`x * 3 + 1`), as
/// `history_guards_hold_on_computed_values` does for the history helpers: a driver that folds the
/// exponent test on arithmetic results (ADR-3150, ADR-8366) would let a NaN through any of them.
/// Every non-finite computed component must be caught and every finite one passed through.
#[test]
fn every_vector_guard_holds_on_computed_values() {
    const TAAU: &str = include_str!("../shaders/taau.wgsl");
    const SSSR: &str = include_str!("../shaders/sssr_common.wgsl");
    const UTILS: &str = include_str!("../shaders/utils.wgsl");
    const FSR3: &str = include_str!("../src/fsr3/fsr3_common.wgsl");
    const TSR: &str = include_str!("../src/tsr/tsr_resolve.wgsl");
    const MOTION_BLUR: &str = include_str!("../shaders/motion_blur.wgsl");
    const DOF: &str = include_str!("../shaders/dof.wgsl");
    const LENS: &str = include_str!("../shaders/lens_effects.wgsl");
    const OIT: &str = include_str!("../shaders/oit_composite.wgsl");
    let (_gpu, device) = device();
    let samples: [[f32; 4]; 6] = [
        [0.25, -3.0, 1.0e-30, 1.0e30],
        [NAN, 1.0, 2.0, 3.0],
        [1.0, INF, 2.0, 3.0],
        [1.0, 2.0, f32::NEG_INFINITY, 0.0],
        [-0.5, 2.0, 3.0, NAN],
        [NAN, INF, f32::NEG_INFINITY, f32::MAX],
    ];
    let computed = |v: f32| v * 3.0 + 1.0;
    let near = |got: f32, want: f32| got.is_finite() && (got - want).abs() <= want.abs() * 1.0e-6;
    let mut problems = Vec::new();
    // Per component: the computed value when finite, else 0.
    for (source, file) in [
        (TAAU, "taau"),
        (SSSR, "sssr_common"),
        (UTILS, "utils"),
        (FSR3, "fsr3_common"),
        (TSR, "tsr_resolve"),
    ] {
        let functions = wgsl_fn(source, "finite_or_zero");
        let body = "nan_out[i] = vec4<f32>(finite_or_zero(nan_in[i].xyz * 3.0 + vec3<f32>(1.0)), 0.0);";
        let out = run(&device, &functions, body, &samples, samples.len());
        for (k, (got, input)) in out.iter().zip(&samples).enumerate() {
            for (c, (g, i)) in got.iter().zip(input).take(3).enumerate() {
                let want = computed(*i);
                let ok = if want.is_finite() {
                    near(*g, want)
                } else {
                    *g == 0.0
                };
                if !ok {
                    problems.push(format!("{file} finite_or_zero case {k} component {c}: {got:?}"));
                }
            }
        }
    }
    // Predicates: 1 when every computed component of the width is finite, else 0.
    for (source, file, name, width, negated) in [
        (MOTION_BLUR, "motion_blur", "finite2", 2, false),
        (MOTION_BLUR, "motion_blur", "finite4", 4, false),
        (DOF, "dof", "finite3", 3, false),
        (LENS, "lens_effects", "finite3", 3, false),
        (SSSR, "sssr_common", "non_finite4", 4, true),
    ] {
        let functions = wgsl_fn(source, name);
        let swizzle = ["", "", "xy", "xyz", "xyzw"][width];
        let call = format!("{name}(nan_in[i].{swizzle} * 3.0 + vec{width}<f32>(1.0))");
        let body = if negated {
            format!("nan_out[i] = vec4<f32>(select(1.0, 0.0, {call}));")
        } else {
            format!("nan_out[i] = vec4<f32>(select(0.0, 1.0, {call}));")
        };
        let out = run(&device, &functions, &body, &samples, samples.len());
        for (k, (got, input)) in out.iter().zip(&samples).enumerate() {
            let want = if input[..width].iter().all(|v| computed(*v).is_finite()) {
                1.0
            } else {
                0.0
            };
            if got[0] != want {
                problems.push(format!("{file} {name} case {k}: {} for {input:?}", got[0]));
            }
        }
    }
    // OIT `bounded`: a non-finite component becomes MAX_HDR, every value is clamped to it.
    let functions = format!("const MAX_HDR: f32 = 6.0e4;\n{}", wgsl_fn(OIT, "bounded"));
    assert!(
        OIT.contains("const MAX_HDR: f32 = 6.0e4;"),
        "oit_composite's MAX_HDR changed"
    );
    let body = "nan_out[i] = bounded(nan_in[i] * 3.0 + vec4<f32>(1.0));";
    let out = run(&device, &functions, body, &samples, samples.len());
    for (k, (got, input)) in out.iter().zip(&samples).enumerate() {
        for (c, (g, i)) in got.iter().zip(input).enumerate() {
            let want = computed(*i);
            let ok = if want.is_finite() {
                near(*g, want.min(6.0e4))
            } else {
                *g == 6.0e4
            };
            if !ok {
                problems.push(format!("oit_composite bounded case {k} component {c}: {got:?}"));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "{} guard(s) let a computed NaN/∞ through or changed a value:\n{}",
        problems.len(),
        problems.join("\n")
    );
}
