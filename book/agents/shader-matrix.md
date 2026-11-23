# Shader permutation matrix

`cargo xtask check-shaders --matrix` compiles every shader permutation a shipped preset can draw, on every backend a
shipped platform translates it for, and fails with the permutation named (ADR-2644). An agent that changes a shader,
a forward define or a render profile runs it before handing the work on; `--json` gives a machine-readable report.

```text
cargo xtask check-shaders --matrix                 # full matrix, text summary on stderr
cargo xtask check-shaders --matrix --json > m.json # the report on stdout
cargo xtask check-shaders --matrix --no-pipelines  # no device: naga, layout scan, spirv-val, HLSL, MSL
cargo xtask check-shaders --matrix --subset        # the fixed subset the default test runs
```

The default `check-shaders` (no flag) also translates every other crate's shaders to MSL now.

The texture-table modules build their pipelines only on a device with binding arrays: run the matrix a second time
with `REGIA_BINDLESS_LEVEL=3` (on a default device they are `skipped`, `device lacks TEXTURE_BINDING_ARRAY`).

## What it covers

- **Presets and platforms.** Linux (Desktop Low, High, Ultra, Deck), Windows and macOS (the three Desktop presets),
  Android (Mobile Low and High), as `permutation_policy::platform_presets` lists them for the cook.
- **Forward axes per preset** (`regia_render::forward_permutations_for`): the lite tier (`forward_mobile.wgsl`, Mobile
  Low and High) and the full shader; the CPU path, and with GPU-driven profiles the GPU-driven and instanced paths;
  display-encoded and linear targets; the studio axis (`STUDIO_IBL_OFF`); the material binding tiers (uniform arena,
  storage arena, texture table; Android keeps the uniform arena); glass, mask decals, thin film, lightmaps,
  subsurface and OIT; the traced sun (`SUN_SHADOW_RT`) on desktop; the Adreno workaround on Android.
- **Material graph passes.** Every pass of every set `enumerate_profile` lists for two sample graphs (surface only;
  vertex stage plus second UV set), single- and double-sided.
- **The engine list** of `check-shaders` (post, upscalers, GI, visibility buffer, sky, fog, clouds, ...).

Equal compositions run once: each composed module is written back to WGSL and hashed, so a define that a module
never tests does not cost a second check. The report lists every define set and origin under the module.

## Backends

| Backend | What passes |
|---|---|
| `compose` | naga_oil composes the source with the defines |
| `naga` | naga validates the module (and no image op goes through a texture parameter) |
| `layout_scan` | no Function or Private variable has an explicitly laid-out type (ADR-026); Workgroup hits are ADR-135 warnings |
| `spirv_val` | `spirv-val --target-env vulkan1.3` accepts the SPIR-V wgpu would emit |
| `vulkan_pipeline` | the module and a pipeline per entry point build on a validation-enabled device without a validation error; `skipped` with the feature named when the device lacks it |
| `hlsl_sm6` | naga writes every entry point as HLSL Shader Model 6.0 |
| `hlsl_sm51` | naga writes Shader Model 5.1 with no Shader Model 6 construct, within the Direct3D 12 limits and FXC's loop, addressing, barrier and assertion rules |
| `msl` | naga writes every entry point as MSL 4.0 with wgpu's Metal options, within Metal's per-stage argument limits |

A full run also renders each preset's frame (and the lab frame) under validation; those results are under `frames`.

## The JSON report

`format_version` 1. Top level: `scope` (`full` or `subset`), `inputs` (origins listed), `compositions` (distinct
file, source and defines), `modules`, `adapter`, `frames`, `unattributed` (validation messages no module owned: a
failure), `failed_modules`. Each module: `file`, `hash` (16 hex digits; `x…` when it did not compose or validate),
`ok`, `permutations` (`defines`, `origins`) and `backends` (`backend`, `status` `pass` / `fail` / `skipped`,
`detail`). An origin reads `<platform>/<preset> <axes>` (`linux/DesktopHigh gpu_instanced linear studio_off
texture_table`), `<platform>/<preset> graph <name> <pass>` or `engine`.

To find what broke: filter `modules` by `ok == false`, read the failing backend's `detail`, and take the first
permutation's `defines` to reproduce it with `regia_render::compose_source`.

## Rows outside the matrix (ADR-2664)

Two kinds of module are built at runtime from text the matrix's enumeration does not list. Both
now have a static row in the default `check-shaders`:

- `water_body.wgsl + PROBE_ENTRY`: the water shader with the compute probe appended
  (`water_body/gpu.rs`, `probe_ring_vertices`) joins the plain-WGSL list, so it takes the SPIR-V
  checks, DX12 and MSL like the other terrain shaders.
- Composed `pt.wgsl` and `lightmap_bake.wgsl` (the path tracer and the GPU lightmap baker) were
  checked for HLSL and MSL only. They now get the layout scan too
  (`regia_render::composed_function_layout_errors`), and **both fail it**: `pt.wgsl` has 15
  Function variables with explicit-layout types (ADR-026: the BVH traversal stacks
  `var stack: array<u32, ...>`, `var sh: Shading`, `var vtx: Vertex`, `var sh: array<vec4<f32>, 12>`
  and similar) and `lightmap_bake.wgsl` has 5 (`var stack: array<u32, STACK>`, `var hit = Hit(..)`,
  `var best = Hit(..)`, `var out = Direct(..)`). A driver that rejects such a module (Mesa does)
  would fail the bake or the preview, so the Vulkan step is not weakened to pass them: they are
  pinned in `xtask::dx12_shaders::COMPOSED_LAYOUT_FAILURES`, a test fails when one starts passing,
  and `regia_render::validate_composed_modules` (the Vulkan step for composed modules) runs over
  the rest of the list, which is empty today. Rewriting the two shaders (a separate job with a
  pixel/bake A/B proof) moves them out.
- ADR-7879 rewrote both: one variable per struct member (or a value built with its constructor),
  and the BVH traversal stacks in workgroup memory (`trace_stack` in `pt.wgsl`, 28 KiB per
  64-invocation group; `bvh_stack` in `lightmap_bake.wgsl`, 12 KiB per 32), outputs hashed
  identical before and after. `COMPOSED_LAYOUT_FAILURES` is empty and both take the Vulkan step;
  `regia_pt`'s `pt_and_lightmap_modules_pass_unrelaxed_spirv_val` runs `spirv-val` with no VUID
  allowed. A SPIRV-Tools release that rejects naga's `ArrayStride` on Workgroup storage (2026.4,
  ADR-135) still cites `VUID-StandaloneSpirv-None-10684` for the two stacks, as for every other
  engine shader with a workgroup array.

## Struct contract and binding limits (ADR-3002)

`cargo xtask check-shaders` also runs two ratchets over every engine shader. Both add to `--json`
(`contract` and `limits` beside `modules`; additive, `format_version` stays 1).

**Rust and WGSL struct agreement.** A uniform or storage binding reads host memory laid out by a Rust type.
`regia_render::shader_contract` asks naga's `Layouter` for the size and member offsets of the composed WGSL
struct and compares them with a registry row, a `ContractRow` built with `contract_row!("file.wgsl", "Struct",
RustType { member, other => rust_field })`. The macro takes the size from `size_of` and each offset from
`offset_of!`, so a row cannot drift from the type. The ratchet lists every struct a uniform or storage binding
reads (`bound_structs`) in every `regia_render`, `regia_terrain`, `regia_vfx` and `regia_pt` shader, and fails when
a struct has neither a row nor an `AllowEntry` (file, struct, reason). It also fails on a dead row, on an allowlist
line for a struct that is now covered or no longer bound, and on any size or offset mismatch. The allowlist can only
shrink. A row may be `partial` (WGSL members it does not list) or `host_may_extend` (a shader that reads a prefix of
a larger uniform).

```json
"contract": { "bound": 120, "covered": 40, "allowed": 80, "checks": 55, "problems": [] }
```

**Binding limits per device class.** `regia_render::shader_limits` counts, per entry point of every composed
permutation, the uniform buffers, storage buffers, storage textures, sampled textures and samplers the stage reads,
the bind groups, and the largest uniform and storage binding. Each shipped preset is held to a class:

| Class | Source | Effect |
|---|---|---|
| `mobile` | the smallest of the engine's Mobile tier floor (`MinspecProfile::Mobile`), a Pixel 6 (Mali-G78) and a Fairphone 5 (Adreno 643); the two phones' numbers are `maxBoundDescriptorSets`, `maxPerStageDescriptor*` and `maxUniformBufferRange` / `maxStorageBufferRange` read with `adb shell cmd gpu vkjson` on 2026-11-05 | a Mobile preset over a number fails |
| `downlevel` | `wgpu::Limits::downlevel_defaults()` | reported per preset (`over_downlevel`) |
| `desktop` | the Pascal tier (`MinspecProfile::Pascal`) and wgpu's defaults | headroom only |

```json
"limits": { "format_version": 1,
  "classes": [ { "name": "mobile", "gate": true, "limits": { "bind_groups": { "value": 4, "source": "Fairphone 5, ...", "verified": true } } } ],
  "presets": [ { "preset": "android/MobileHigh", "class": "mobile", "modules": 40,
                 "worst": [ { "metric": "sampled_textures", "used": 30, "limit": 32, "headroom": 2, "module": "forward_mobile.wgsl", "entry": "fs_main (fragment)" } ],
                 "over": [], "over_downlevel": [ "..." ] } ],
  "engine": { "...": "modules outside any preset's permutation list, against the mobile class" },
  "failures": [], "unverified": [] }
```

To confirm a number on a device: `adb -s <serial> shell cmd gpu vkjson` prints the Vulkan properties JSON
(`devices[0].properties.limits`). A row whose number nothing backs is listed under `unverified`.

## The FXC assertion rule

`hlsl_sm51` also fails a module that holds the shape FXC's optimizer does not come back from (ADR-5971): it recurses
until the stack overflows, at any stack size, and the process that creates the pipeline dies. The shape is a comparison
of two values that an enclosing `if` holds strictly apart, written with its operands the other way round, while an
enclosing `if` holds a third value below one of the two:

```wgsl
fn lookup(params: vec4<f32>) -> f32 {
    if (params.w < 0.5) { return 1.0; }        // the caller's test the other way round
    return 0.25;
}
// ...
if (unlit < 0.5) {                              // a third value below the same constant
    if (frame.cloud.w > 0.5) {                  // holds `0.5 < w`
        sun *= lookup(frame.cloud);
    }
}
```

Write the inner test as the outer one negated, `if (!(params.w > 0.5))`, or drop it. `w <= 0.5` compiles too; `w >= 0.5`
does not. `sign(x)`, `step(edge, x)` and `faceForward` are comparisons to FXC, and a vector comparison is one per
component. The check follows values through function calls, a `bool` stored once and a callee's only `return`. It is a
heuristic measured on `d3dcompiler_47.dll`: it also flags a few shapes FXC compiles (a float read from a storage
buffer), where the same rewrite applies. The message names the function with the inner test and the functions whose
`if`s hold the two facts.

## Exit and errors

Exit code 1 when any module, frame or unattributed line fails, after the other `check-shaders` checks. There are no
agent error codes: this is a repository tool, not a runtime method. The Metal compiler and the FXC and DXC compilers
do not run on Linux; `docs/06-testing-and-ci.md` § 17 has the Mac and Windows commands (`REGIA_MSL_DUMP_DIR`,
`REGIA_HLSL_DUMP_DIR`).
