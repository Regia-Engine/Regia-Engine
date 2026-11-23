// SPDX-License-Identifier: MPL-2.0
//! DX12 shader checks that run on any host (ADR-367).
//!
//! wgpu's DX12 backend turns each naga module into HLSL and compiles it with FXC (Shader Model
//! 5.1) or, with the `static-dxc` feature, DXC (Shader Model 6). A Linux host cannot run either
//! compiler, so [`dx12_module_problems`] runs the part it can:
//!
//! - naga's HLSL writer for Shader Model 5.1 and 6.0, the way wgpu-hal calls it (overrides
//!   substituted first, one write per entry point, the special-constants buffer bound);
//! - the fixed Direct3D 12 compute limits, which naga does not check and the HLSL compilers
//!   enforce: [`D3D12_GROUPSHARED_BYTES`] of `groupshared` memory per thread group,
//!   [`D3D12_MAX_THREADS_PER_GROUP`] threads per group, and [`D3D12_MAX_GROUP_SIZE`] per axis.
//!   Desktop Vulkan drivers allow more workgroup memory (48 or 64 KiB), so a shader can pass
//!   every Vulkan test and still fail to compile on DX12;
//! - FXC's loop rule (ADR-201): a fragment shader may not reach an implicit-derivative sample
//!   (`textureSample`, `textureSampleBias`, `textureSampleCompare`) or a `dpdx` / `dpdy` /
//!   `fwidth` inside a loop. FXC must unroll such a loop, and naga writes every loop as a
//!   bounded `while (true)`. The loops ADR-201 reworked in `lab_forward.wgsl` already sample
//!   with explicit levels; this applies the rule to every module, callees included. DXC has no
//!   such rule.
//! - FXC's addressing and barrier rules (ADR-1583), the two its survey on Windows hit:
//!   - a store at a dynamic index into a vector component, a matrix column or an array that is a
//!     struct member is not "natively addressable" for FXC (X3550): it unrolls every enclosing
//!     loop to make the index constant, and fails on a loop whose trip count is not a
//!     compile-time constant (X3511). A plain local or private array, an array of arrays and an
//!     array of structs are addressable. So such a store, callees included, may not sit inside
//!     a loop that does not count a local variable against a constant by a constant step;
//!   - a control barrier (`workgroupBarrier`, `storageBarrier`, `workgroupUniformLoad`) after a
//!     `return` under a condition FXC cannot prove uniform, in the same function: FXC follows
//!     constants, uniform buffers, the workgroup id and a `workgroupUniformLoad` result read
//!     directly, but not a value merged from two paths (a function with two `return`s), so it
//!     rejects the barrier (X4026) where WGSL's uniformity analysis accepts it.
//!
//!   Both are heuristics on naga's IR, not FXC: a loop FXC unrolls by other means can be flagged,
//!   and FXC rejects more than this sees.
//! - FXC's assertion rule (ADR-5971): FXC's optimizer keeps what the enclosing `if`s hold about
//!   each value and checks every comparison against it. It does not come back (it recurses until
//!   the stack overflows, at any stack size: the compile kills the process) from a comparison of
//!   two values that an enclosing `if` holds strictly apart, written with its operands the other
//!   way round (`w < k` or `w >= k` under `if (w > k)`), while an enclosing `if` holds a third
//!   value below one of the two (`u < k`). The same test written as the outer one negated
//!   (`!(w > k)`), or `w <= k`, compiles. `sign` and `step` are comparisons to FXC. The shape
//!   comes from a callee re-testing a switch its caller tested (`lab_forward.wgsl` called the
//!   cloud shadow lookup that way and no DX12 host without DXC could build the forward
//!   pipelines), so the rule follows values through calls. A heuristic like the two above:
//!   measured on `d3dcompiler_47.dll` with minimal HLSL, not derived from FXC.
//!
//! [`wgsl_dx12_problems`] runs the same checks on plain WGSL sources: the shaders of the
//! other engine crates, through `cargo xtask check-shaders`. Passing proves naga can write the
//! shader as HLSL inside the Direct3D 12 limits. It does not prove FXC or DXC accepts the text,
//! and it cannot see a driver bug.

use naga::back::hlsl;

/// Direct3D 12 `groupshared` memory per compute thread group, in bytes
/// (`D3D12_CS_TGSM_REGISTER_COUNT` registers of 4 bytes).
pub const D3D12_GROUPSHARED_BYTES: u32 = 32 << 10;

/// Direct3D 12 threads per compute thread group (`D3D12_CS_THREAD_GROUP_MAX_THREADS_PER_GROUP`).
pub const D3D12_MAX_THREADS_PER_GROUP: u32 = 1024;

/// Direct3D 12 compute thread group size per axis, x, y, z (`D3D12_CS_THREAD_GROUP_MAX_X`, `_Y`,
/// `_Z`).
pub const D3D12_MAX_GROUP_SIZE: [u32; 3] = [1024, 1024, 64];

/// The shader models wgpu's DX12 backend writes: 5.1 for FXC, 6.0 (the lowest) for DXC.
const SHADER_MODELS: [hlsl::ShaderModel; 2] = [hlsl::ShaderModel::V5_1, hlsl::ShaderModel::V6_0];

/// What DX12 would reject in `module` (named `name` in each message): naga validation and HLSL
/// writer errors for Shader Model 5.1 and 6.0, compute entry points over the Direct3D 12 limits,
/// fragment entry points that break FXC's loop rule, and entry points that break FXC's
/// addressing, barrier or assertion rule. Empty when the module passes.
#[must_use]
pub fn dx12_module_problems(name: &str, module: &naga::Module) -> Vec<String> {
    let info = match naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(module)
    {
        Ok(info) => info,
        Err(err) => return vec![format!("{name}: naga validation: {err}")],
    };
    let constants = naga::back::PipelineConstants::new();
    let (module, info) =
        match naga::back::pipeline_constants::process_overrides(module, &info, None, &constants) {
            Ok(pair) => pair,
            Err(err) => return vec![format!("{name}: pipeline overrides: {err}")],
        };
    let mut problems = compute_limit_problems(name, &module);
    problems.extend(gradient_loop_problems(name, &module));
    problems.extend(fxc_flow_problems(name, &module, &info));
    problems.extend(fxc_assertion_problems(name, &module, &info));
    for shader_model in SHADER_MODELS {
        if let Err(err) = write_hlsl(name, &module, &info, shader_model) {
            problems.push(err);
        }
    }
    problems
}

/// Parses each `(name, wgsl)` source and runs [`dx12_module_problems`] on the module as
/// `ShaderSource::Wgsl` hands it to wgpu and, when that differs, as [`crate::wgsl_source`] hands
/// it (texture parameters specialized away, the `(wgsl_source)` label). Returns every parse
/// failure and DX12 problem, one per entry; empty when all pass.
#[must_use]
pub fn wgsl_dx12_problems(sources: &[(&str, &str)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, source) in sources {
        match naga::front::wgsl::parse_str(source) {
            Ok(mut module) => {
                problems.extend(dx12_module_problems(name, &module));
                if crate::specialize_handle_arguments(&mut module) {
                    problems.extend(dx12_module_problems(&format!("{name} (wgsl_source)"), &module));
                }
            }
            Err(err) => problems.push(format!("{name}: wgsl parse: {err}")),
        }
    }
    problems
}

/// Writes every entry point of `module` as HLSL for `shader_model`; the first naga error.
fn write_hlsl(
    name: &str,
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
    shader_model: hlsl::ShaderModel,
) -> Result<(), String> {
    let options = hlsl::Options {
        shader_model,
        // wgpu-core always asks for the first-vertex, first-instance and workgroup-count
        // constants (`PipelineLayoutFlags`), so wgpu-hal always binds them, and naga then writes
        // `vertex_index`, `instance_index` and `num_workgroups` through them. The register is
        // arbitrary here: nothing compiles this text.
        special_constants_binding: Some(hlsl::BindTarget {
            space: u8::MAX,
            ..hlsl::BindTarget::default()
        }),
        ..hlsl::Options::default()
    };
    for entry in &module.entry_points {
        let pipeline = hlsl::PipelineOptions {
            entry_point: Some((entry.stage, entry.name.clone())),
        };
        let mut text = String::new();
        let mut writer = hlsl::Writer::new(&mut text, &options, &pipeline);
        let reflection = writer
            .write(module, info, None)
            .map_err(|err| format!("{name} ({}, {shader_model:?}): hlsl: {err}", entry.name))?;
        let mut written_name = None;
        for written in reflection.entry_point_names {
            let written = written.map_err(|err| {
                format!(
                    "{name} ({}, {shader_model:?}): hlsl entry point: {err}",
                    entry.name
                )
            })?;
            written_name.get_or_insert(written);
        }
        if shader_model == hlsl::ShaderModel::V5_1 {
            // naga renames some entry points (`build_level0` is written `build_level0_`); wgpu
            // compiles the written name.
            dump_hlsl(name, entry, written_name.as_deref().unwrap_or(&entry.name), &text);
        }
    }
    Ok(())
}

/// With `REGIA_HLSL_DUMP_DIR` set, writes the Shader Model 5.1 text of `entry` there as
/// `<name>__<entry>.hlsl` and appends `file|written entry name|profile` to `manifest.txt`, so a
/// Windows host can run `fxc.exe` over every engine shader (the text wgpu hands to `D3DCompile`).
/// Each manifest line is one `write` to a file opened for appending, so the parallel test
/// threads do not interleave lines. Best effort: a write error is ignored, the check itself does
/// not depend on it.
fn dump_hlsl(name: &str, entry: &naga::EntryPoint, written_name: &str, text: &str) {
    use std::io::Write as _;
    let Some(dir) = std::env::var_os("REGIA_HLSL_DUMP_DIR") else {
        return;
    };
    let dir = std::path::Path::new(&dir);
    let _ = std::fs::create_dir_all(dir);
    let clean: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let file = format!("{clean}__{}.hlsl", entry.name);
    if std::fs::write(dir.join(&file), text).is_err() {
        return;
    }
    let profile = match entry.stage {
        naga::ShaderStage::Vertex => "vs_5_1",
        naga::ShaderStage::Fragment => "ps_5_1",
        _ => "cs_5_1",
    };
    if let Ok(mut manifest) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("manifest.txt"))
    {
        let line = format!("{file}|{written_name}|{profile}\n");
        let _ = manifest.write_all(line.as_bytes());
    }
}

/// Compute entry points of `module` whose thread group is over the Direct3D 12 caps, and the
/// module's `groupshared` memory when it is over the Direct3D 12 budget.
fn compute_limit_problems(name: &str, module: &naga::Module) -> Vec<String> {
    let mut problems = Vec::new();
    let mut has_compute = false;
    for entry in &module.entry_points {
        if entry.stage != naga::ShaderStage::Compute {
            continue;
        }
        has_compute = true;
        let [x, y, z] = entry.workgroup_size;
        let threads = u64::from(x) * u64::from(y) * u64::from(z);
        let [max_x, max_y, max_z] = D3D12_MAX_GROUP_SIZE;
        if threads > u64::from(D3D12_MAX_THREADS_PER_GROUP) || x > max_x || y > max_y || z > max_z {
            problems.push(format!(
                "{name} ({}): workgroup size {x}x{y}x{z} is over Direct3D 12's {max_x}x{max_y}x{max_z}, \
                 {D3D12_MAX_THREADS_PER_GROUP} threads per group",
                entry.name
            ));
        }
    }
    let bytes = groupshared_bytes(module);
    if has_compute && bytes > D3D12_GROUPSHARED_BYTES {
        problems.push(format!(
            "{name}: {bytes} bytes of workgroup memory, over Direct3D 12's {D3D12_GROUPSHARED_BYTES}"
        ));
    }
    problems
}

/// Bytes of workgroup memory `module` declares, by naga's WGSL layout. naga writes every
/// module-scope variable into each entry point's HLSL, so this is the whole module, not what
/// one entry point touches (whether FXC or DXC drop an unused `groupshared` array before they
/// count is not relied on). WGSL pads at least as much as HLSL packs `groupshared` data (a
/// `vec3<f32>` array element takes 16 bytes, not 12), so this never reads below what the
/// compilers count.
fn groupshared_bytes(module: &naga::Module) -> u32 {
    module
        .global_variables
        .iter()
        .filter(|(_, var)| var.space == naga::AddressSpace::WorkGroup)
        .map(|(_, var)| {
            module.types[var.ty]
                .inner
                .try_size(module.to_ctx())
                .unwrap_or(u32::MAX)
        })
        .fold(0, u32::saturating_add)
}

/// Fragment entry points of `module` that reach implicit-derivative work inside a loop.
fn gradient_loop_problems(name: &str, module: &naga::Module) -> Vec<String> {
    let mut scan = GradientScan {
        module,
        functions: vec![None; module.functions.len()],
    };
    let mut problems = Vec::new();
    for entry in &module.entry_points {
        if entry.stage != naga::ShaderStage::Fragment {
            continue;
        }
        let mut found = Gradients::default();
        scan.block(&entry.function, &entry.function.body, false, &mut found);
        if found.in_loop {
            problems.push(format!(
                "{name} ({}): a sample with an implicit level of detail (textureSample, textureSampleBias, \
                 textureSampleCompare) or a dpdx/dpdy/fwidth inside a loop: FXC has to unroll that loop \
                 (X3570) and may fail to (X3511), since naga writes every loop as a bounded `while (true)`. \
                 Use an explicit level (textureSampleLevel, textureSampleCompareLevel, textureSampleGrad) \
                 or move the derivative out of the loop",
                entry.name
            ));
        }
    }
    problems
}

/// Implicit-derivative work of a function (callees included): anywhere in it, and inside a loop.
#[derive(Clone, Copy, Default)]
struct Gradients {
    anywhere: bool,
    in_loop: bool,
}

/// Walks function bodies for `Gradients`, once per function (WGSL has no recursion).
struct GradientScan<'a> {
    module: &'a naga::Module,
    functions: Vec<Option<Gradients>>,
}

impl GradientScan<'_> {
    fn function(&mut self, handle: naga::Handle<naga::Function>) -> Gradients {
        if let Some(found) = self.functions[handle.index()] {
            return found;
        }
        let module = self.module;
        let function = &module.functions[handle];
        let mut found = Gradients::default();
        self.block(function, &function.body, false, &mut found);
        self.functions[handle.index()] = Some(found);
        found
    }

    fn block(
        &mut self,
        function: &naga::Function,
        block: &naga::Block,
        in_loop: bool,
        found: &mut Gradients,
    ) {
        for statement in block {
            match statement {
                naga::Statement::Emit(range) => {
                    if range
                        .clone()
                        .any(|handle| is_implicit_derivative(&function.expressions[handle]))
                    {
                        found.anywhere = true;
                        found.in_loop |= in_loop;
                    }
                }
                naga::Statement::Call { function: callee, .. } => {
                    let inner = self.function(*callee);
                    found.anywhere |= inner.anywhere;
                    found.in_loop |= inner.in_loop || (in_loop && inner.anywhere);
                }
                naga::Statement::Block(inner) => self.block(function, inner, in_loop, found),
                naga::Statement::If { accept, reject, .. } => {
                    self.block(function, accept, in_loop, found);
                    self.block(function, reject, in_loop, found);
                }
                naga::Statement::Switch { cases, .. } => {
                    for case in cases {
                        self.block(function, &case.body, in_loop, found);
                    }
                }
                naga::Statement::Loop { body, continuing, .. } => {
                    self.block(function, body, true, found);
                    self.block(function, continuing, true, found);
                }
                _ => {}
            }
        }
    }
}

/// Sampling with an automatic or biased level of detail, or a screen-space derivative: the HLSL
/// gradient instructions (`Sample`, `SampleBias`, `SampleCmp`, `ddx`, `ddy`, `fwidth`).
fn is_implicit_derivative(expression: &naga::Expression) -> bool {
    matches!(
        expression,
        naga::Expression::ImageSample {
            level: naga::SampleLevel::Auto | naga::SampleLevel::Bias(_),
            gather: None,
            ..
        } | naga::Expression::Derivative { .. }
    )
}

/// Entry points of `module` that break FXC's addressing rule (a store FXC cannot address at a
/// dynamic index inside a loop with a runtime trip count) or its barrier rule (a control
/// barrier after a divergent `return`), callees included. `info` is `module`'s validation.
fn fxc_flow_problems(name: &str, module: &naga::Module, info: &naga::valid::ModuleInfo) -> Vec<String> {
    let mut scan = FlowScan {
        module,
        info,
        functions: vec![None; module.functions.len()],
        results: vec![None; module.functions.len()],
    };
    let mut problems = Vec::new();
    for (index, entry) in module.entry_points.iter().enumerate() {
        let found = scan.walk(&entry.function, info.get_entry_point(index), &entry.name);
        if let Some(store) = found.store_in_runtime_loop {
            problems.push(format!(
                "{name} ({}): a store to {store} inside a loop whose trip count is not a compile-time \
                 constant: FXC cannot address that store (X3550), unrolls every enclosing loop to make \
                 the index constant and fails on this one (X3511). Accumulate in a plain local array, \
                 or build the vector or struct whole",
                entry.name
            ));
        }
        if let Some(barrier) = found.barrier_after_return {
            problems.push(format!(
                "{name} ({}): {barrier}: FXC cannot prove the barrier uniform (X4026). Reach every \
                 barrier first and return after the last one",
                entry.name
            ));
        }
    }
    problems
}

/// What `FlowScan` finds in a function, callees included. The strings name the first case.
#[derive(Clone, Default)]
struct Flow {
    /// A store FXC cannot address at a dynamic index, anywhere.
    store: Option<String>,
    /// Such a store inside a loop with a runtime trip count.
    store_in_runtime_loop: Option<String>,
    /// A control barrier, anywhere.
    barrier: bool,
    /// A control barrier after a divergent `return` of the same function.
    barrier_after_return: Option<String>,
}

/// Walks function bodies for `Flow`, once per function (WGSL has no recursion).
struct FlowScan<'a> {
    module: &'a naga::Module,
    info: &'a naga::valid::ModuleInfo,
    functions: Vec<Option<Flow>>,
    /// Per function: whether FXC sees its result as uniform (`FlowScan::result_uniform`).
    results: Vec<Option<bool>>,
}

/// One function's walk: the function, its validation, its name for messages, whether a
/// divergent `return` came before the statement being walked, what was found, and which
/// expressions FXC sees as uniform (`FlowScan::uniform`, filled as asked).
struct FlowWalk<'f> {
    function: &'f naga::Function,
    info: &'f naga::valid::FunctionInfo,
    label: &'f str,
    returned: bool,
    found: Flow,
    uniform: Vec<Option<bool>>,
}

/// Where `FlowScan::block` is: inside a loop with a runtime trip count; under a branch whose
/// condition FXC cannot prove uniform, or inside a loop, where a `return` diverges.
#[derive(Clone, Copy)]
struct FlowAt {
    runtime_loop: bool,
    divergent: bool,
}

impl FlowScan<'_> {
    fn function(&mut self, handle: naga::Handle<naga::Function>) -> Flow {
        if let Some(found) = &self.functions[handle.index()] {
            return found.clone();
        }
        let module = self.module;
        let info = self.info;
        let function = &module.functions[handle];
        let label = function.name.as_deref().unwrap_or("a function");
        let found = self.walk(function, &info[handle], label);
        self.functions[handle.index()] = Some(found.clone());
        found
    }

    fn walk(&mut self, function: &naga::Function, info: &naga::valid::FunctionInfo, label: &str) -> Flow {
        let mut walk = FlowWalk {
            function,
            info,
            label,
            returned: false,
            found: Flow::default(),
            uniform: vec![None; function.expressions.len()],
        };
        let at = FlowAt {
            runtime_loop: false,
            divergent: false,
        };
        self.block(&mut walk, &function.body, at);
        walk.found
    }

    /// Whether FXC sees the result of `handle` as uniform: its only `return` is the last
    /// top-level statement and returns a value `FlowScan::uniform` accepts (every argument taken
    /// as varying). Two returns make the result a merge FXC does not follow (`ddgi_blend.wgsl`'s
    /// old `load_rays`, ADR-1583).
    fn result_uniform(&mut self, handle: naga::Handle<naga::Function>) -> bool {
        if let Some(known) = self.results[handle.index()] {
            return known;
        }
        let module = self.module;
        let function = &module.functions[handle];
        let value = match function.body.last() {
            Some(naga::Statement::Return { value: Some(value) }) if count_returns(&function.body) == 1 => {
                Some(*value)
            }
            _ => None,
        };
        let uniform = value.is_some_and(|value| {
            let mut memo = vec![None; function.expressions.len()];
            self.uniform(function, &mut memo, value)
        });
        self.results[handle.index()] = Some(uniform);
        uniform
    }

    /// Whether FXC can prove `handle` uniform across the thread group: constants, uniform
    /// buffer loads, a `workgroupUniformLoad` result read directly (it sits between two
    /// barriers), the workgroup id and count, a callee's uniform result, and arithmetic on
    /// those. Everything else (a local variable, a storage or texture read, an argument) is
    /// taken as varying. `memo` caches per expression.
    fn uniform(
        &mut self,
        function: &naga::Function,
        memo: &mut Vec<Option<bool>>,
        handle: naga::Handle<naga::Expression>,
    ) -> bool {
        use naga::Expression as E;
        if let Some(known) = memo[handle.index()] {
            return known;
        }
        let operands: Vec<naga::Handle<naga::Expression>> = match function.expressions[handle] {
            E::Literal(_) | E::Constant(_) | E::Override(_) | E::ZeroValue(_) => Vec::new(),
            E::WorkGroupUniformLoadResult { .. } => Vec::new(),
            E::GlobalVariable(global) => {
                let space = self.module.global_variables[global].space;
                let uniform = matches!(space, naga::AddressSpace::Uniform | naga::AddressSpace::Immediate);
                memo[handle.index()] = Some(uniform);
                return uniform;
            }
            E::FunctionArgument(index) => {
                let binding = usize::try_from(index)
                    .ok()
                    .and_then(|index| function.arguments.get(index))
                    .and_then(|argument| argument.binding.as_ref());
                let uniform = matches!(
                    binding,
                    Some(naga::Binding::BuiltIn(
                        naga::BuiltIn::WorkGroupId | naga::BuiltIn::NumWorkGroups
                    ))
                );
                memo[handle.index()] = Some(uniform);
                return uniform;
            }
            E::CallResult(callee) => {
                let uniform = self.result_uniform(callee);
                memo[handle.index()] = Some(uniform);
                return uniform;
            }
            E::Load { pointer } => vec![pointer],
            E::AccessIndex { base, .. } => vec![base],
            E::Access { base, index } => vec![base, index],
            E::Splat { value, .. } => vec![value],
            E::Swizzle { vector, .. } => vec![vector],
            E::Unary { expr, .. } | E::As { expr, .. } => vec![expr],
            E::Binary { left, right, .. } => vec![left, right],
            E::Select {
                condition,
                accept,
                reject,
            } => vec![condition, accept, reject],
            E::Relational { argument, .. } => vec![argument],
            E::Math {
                arg,
                arg1,
                arg2,
                arg3,
                ..
            } => [Some(arg), arg1, arg2, arg3].into_iter().flatten().collect(),
            E::Compose { ref components, .. } => components.clone(),
            _ => {
                memo[handle.index()] = Some(false);
                return false;
            }
        };
        let uniform = operands
            .into_iter()
            .all(|operand| self.uniform(function, memo, operand));
        memo[handle.index()] = Some(uniform);
        uniform
    }

    /// `at` under a branch on `condition`: divergent when it was, or when FXC cannot prove
    /// `condition` uniform.
    fn branch(
        &mut self,
        walk: &mut FlowWalk<'_>,
        at: FlowAt,
        condition: naga::Handle<naga::Expression>,
    ) -> FlowAt {
        let function = walk.function;
        let divergent = at.divergent || !self.uniform(function, &mut walk.uniform, condition);
        FlowAt { divergent, ..at }
    }

    fn block(&mut self, walk: &mut FlowWalk<'_>, block: &naga::Block, at: FlowAt) {
        for statement in block {
            match statement {
                naga::Statement::Store { pointer, .. } => {
                    let store = unaddressable_store(self.module, walk.function, walk.info, *pointer);
                    if let Some(what) = store {
                        let what = format!("{what} in `{}`", walk.label);
                        let found = &mut walk.found;
                        if at.runtime_loop && found.store_in_runtime_loop.is_none() {
                            found.store_in_runtime_loop = Some(what.clone());
                        }
                        found.store.get_or_insert(what);
                    }
                }
                naga::Statement::ControlBarrier(_) | naga::Statement::WorkGroupUniformLoad { .. } => {
                    walk.found.barrier = true;
                    if walk.returned && walk.found.barrier_after_return.is_none() {
                        walk.found.barrier_after_return = Some(format!(
                            "a workgroup barrier after a divergent `return` in `{}`",
                            walk.label
                        ));
                    }
                }
                naga::Statement::Call { function: callee, .. } => {
                    let inner = self.function(*callee);
                    let found = &mut walk.found;
                    if at.runtime_loop && found.store_in_runtime_loop.is_none() {
                        found.store_in_runtime_loop.clone_from(&inner.store);
                    }
                    if found.store_in_runtime_loop.is_none() {
                        found.store_in_runtime_loop = inner.store_in_runtime_loop;
                    }
                    if found.store.is_none() {
                        found.store = inner.store;
                    }
                    if found.barrier_after_return.is_none() {
                        found.barrier_after_return = inner.barrier_after_return;
                    }
                    if inner.barrier {
                        found.barrier = true;
                        if walk.returned && found.barrier_after_return.is_none() {
                            let callee = self.module.functions[*callee]
                                .name
                                .as_deref()
                                .unwrap_or("a function");
                            found.barrier_after_return = Some(format!(
                                "a workgroup barrier (in `{callee}`) after a divergent `return` in `{}`",
                                walk.label
                            ));
                        }
                    }
                }
                naga::Statement::Return { .. } => walk.returned |= at.divergent,
                naga::Statement::Block(inner) => self.block(walk, inner, at),
                naga::Statement::If {
                    condition,
                    accept,
                    reject,
                } => {
                    let inside = self.branch(walk, at, *condition);
                    self.block(walk, accept, inside);
                    self.block(walk, reject, inside);
                }
                naga::Statement::Switch { selector, cases } => {
                    let inside = self.branch(walk, at, *selector);
                    for case in cases {
                        self.block(walk, &case.body, inside);
                    }
                }
                naga::Statement::Loop {
                    body,
                    continuing,
                    break_if,
                } => {
                    let runtime =
                        break_if.is_some() || !counts_to_a_constant(walk.function, body, continuing);
                    let inside = FlowAt {
                        runtime_loop: at.runtime_loop || runtime,
                        divergent: true,
                    };
                    // A barrier earlier in the body than a `return` in it still follows that
                    // return on the next trip.
                    let returned_before = walk.returned;
                    let barrier_before = std::mem::take(&mut walk.found.barrier);
                    self.block(walk, body, inside);
                    self.block(walk, continuing, inside);
                    if walk.returned
                        && !returned_before
                        && walk.found.barrier
                        && walk.found.barrier_after_return.is_none()
                    {
                        walk.found.barrier_after_return = Some(format!(
                            "a workgroup barrier in a loop that holds a `return` in `{}`",
                            walk.label
                        ));
                    }
                    walk.found.barrier |= barrier_before;
                }
                _ => {}
            }
        }
    }
}

/// The `return` statements in `block`, nested ones included.
fn count_returns(block: &naga::Block) -> usize {
    block
        .iter()
        .map(|statement| match statement {
            naga::Statement::Return { .. } => 1,
            naga::Statement::Block(inner) => count_returns(inner),
            naga::Statement::If { accept, reject, .. } => count_returns(accept) + count_returns(reject),
            naga::Statement::Switch { cases, .. } => cases.iter().map(|case| count_returns(&case.body)).sum(),
            naga::Statement::Loop { body, continuing, .. } => count_returns(body) + count_returns(continuing),
            _ => 0,
        })
        .sum()
}

/// What a store through `pointer` writes when FXC cannot address it: a vector component, a
/// matrix column or an element of an array that is a struct member, at an index that is not a
/// constant, in function or private memory. `None` for every other store.
fn unaddressable_store(
    module: &naga::Module,
    function: &naga::Function,
    info: &naga::valid::FunctionInfo,
    pointer: naga::Handle<naga::Expression>,
) -> Option<&'static str> {
    let mut what = None;
    let mut at = pointer;
    loop {
        match function.expressions[at] {
            naga::Expression::Access { base, index } => {
                if !is_constant(&function.expressions[index]) {
                    match pointee(module, info, base) {
                        Pointee::Vector => what = Some("a vector component at a dynamic index"),
                        Pointee::Matrix => what = Some("a matrix column at a dynamic index"),
                        Pointee::Array if is_struct_member(module, function, info, base) => {
                            what = Some("an array inside a struct at a dynamic index");
                        }
                        _ => {}
                    }
                }
                at = base;
            }
            naga::Expression::AccessIndex { base, .. } => at = base,
            naga::Expression::LocalVariable(_) => return what,
            naga::Expression::GlobalVariable(global) => {
                let private = module.global_variables[global].space == naga::AddressSpace::Private;
                return what.filter(|_| private);
            }
            _ => return None,
        }
    }
}

/// The kind of value a pointer points at, as far as FXC's addressing cares.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pointee {
    Vector,
    Matrix,
    Array,
    Struct,
    Other,
}

/// What `pointer` points at (a `ValuePointer` with a size points at a vector).
fn pointee(
    module: &naga::Module,
    info: &naga::valid::FunctionInfo,
    pointer: naga::Handle<naga::Expression>,
) -> Pointee {
    match info[pointer].ty.inner_with(&module.types) {
        naga::TypeInner::Pointer { base, .. } => match module.types[*base].inner {
            naga::TypeInner::Vector { .. } => Pointee::Vector,
            naga::TypeInner::Matrix { .. } => Pointee::Matrix,
            naga::TypeInner::Array { .. } => Pointee::Array,
            naga::TypeInner::Struct { .. } => Pointee::Struct,
            _ => Pointee::Other,
        },
        naga::TypeInner::ValuePointer { size: Some(_), .. } => Pointee::Vector,
        _ => Pointee::Other,
    }
}

/// Whether `pointer` is a struct member's address (`s.member`).
fn is_struct_member(
    module: &naga::Module,
    function: &naga::Function,
    info: &naga::valid::FunctionInfo,
    pointer: naga::Handle<naga::Expression>,
) -> bool {
    match function.expressions[pointer] {
        naga::Expression::AccessIndex { base, .. } => pointee(module, info, base) == Pointee::Struct,
        _ => false,
    }
}

/// A literal, a named or overridden constant, or a zero value.
fn is_constant(expression: &naga::Expression) -> bool {
    matches!(
        expression,
        naga::Expression::Literal(_)
            | naga::Expression::Constant(_)
            | naga::Expression::Override(_)
            | naga::Expression::ZeroValue(_)
    )
}

/// Whether a loop has the shape naga gives `for (var i = a; i < N; i = i + k)` with constants
/// `N` and `k`: its body starts with `if (i op N) {} else { break; }` (or the reverse) and its
/// continuing block stores `i op k` to the same local. FXC can count and unroll such a loop
/// (once the loops around it are unrolled, if `a` depends on them).
fn counts_to_a_constant(function: &naga::Function, body: &naga::Block, continuing: &naga::Block) -> bool {
    let local_of = |handle: naga::Handle<naga::Expression>| match function.expressions[handle] {
        naga::Expression::LocalVariable(local) => Some(local),
        _ => None,
    };
    let loaded = |handle: naga::Handle<naga::Expression>| match function.expressions[handle] {
        naga::Expression::Load { pointer } => local_of(pointer),
        _ => None,
    };
    // `local op constant` or `constant op local`.
    let against_constant = |handle: naga::Handle<naga::Expression>| match function.expressions[handle] {
        naga::Expression::Binary { left, right, .. } => {
            if is_constant(&function.expressions[right]) {
                loaded(left)
            } else if is_constant(&function.expressions[left]) {
                loaded(right)
            } else {
                None
            }
        }
        _ => None,
    };
    let is_break =
        |block: &naga::Block| block.len() == 1 && matches!(block.iter().next(), Some(naga::Statement::Break));
    let Some(header) = body
        .iter()
        .find(|statement| !matches!(statement, naga::Statement::Emit(_)))
    else {
        return false;
    };
    let counter = match header {
        naga::Statement::If {
            condition,
            accept,
            reject,
        } if (accept.is_empty() && is_break(reject)) || (reject.is_empty() && is_break(accept)) => {
            against_constant(*condition)
        }
        _ => None,
    };
    let Some(counter) = counter else {
        return false;
    };
    continuing.iter().any(|statement| match statement {
        naga::Statement::Store { pointer, value } => {
            local_of(*pointer) == Some(counter) && against_constant(*value) == Some(counter)
        }
        _ => false,
    })
}

/// Calls the assertion rule walks per entry point before it gives up. It walks a callee once per
/// call, with that call's arguments, so the count is the entry point's call paths. An entry point
/// over the budget is reported as a problem, so every engine shader is known to stay under it.
const ASSERTION_CALL_BUDGET: u32 = 400_000;

/// Entry points of `module` that hold the shape FXC's optimizer does not come back from
/// (ADR-5971): a comparison of two values that an enclosing `if` holds strictly apart, written
/// with its operands the other way round, while an enclosing `if` holds a third value below one
/// of the two. Callees are walked with their callers' values. `info` is `module`'s validation.
///
/// What was measured on `d3dcompiler_47.dll` (10.0.26100, the System32 and the SDK build, wgpu's
/// release flags), in FXC's own instructions (`a < b` is `lt(a, b)`, `a > b` is `lt(b, a)`,
/// `a <= b` is `ge(b, a)`, `a >= b` is `ge(a, b)`):
///
/// - an `if` holds `lo < hi` strictly (`lt(lo, hi)` taken, or `ge(lo, hi)` not taken), and a
///   comparison under it has the operands `(hi, lo)`: `lt(hi, lo)` or `ge(hi, lo)`. The same
///   pair in the same order (`ge(lo, hi)`, the outer test negated) compiles;
/// - and an `if` around that comparison holds another value at or below `lo` or `hi`
///   (`u < hi`, `u <= lo`, taken or as an else branch). Without one FXC compiles the pair;
/// - `sign(x)` is `lt(0, x)` and `lt(x, 0)`, `step(e, x)` is `ge(x, e)`, and `faceForward(a,
///   b, c)` is `sign(dot(b, c))`; `==`, `!=`, `min`, `max`, `clamp`, `saturate`, `smoothstep`,
///   `abs`, `atan2`, `pow`, `floor` and `fract` are not comparisons here;
/// - a vector comparison, `sign` or `step` is one comparison per component, and `all(a > b)`
///   taken (or `any` not taken) holds every component's;
/// - a fact reaches FXC through a `bool` stored once or returned by a callee, not through a
///   merge of two paths (naga's `&&` and `||`), an early `return` or a loop's `break`.
///
/// FXC also lets some values off (a `f32` read from a storage buffer, a sum or product as the
/// third value). The rule does not, so it can flag a shader FXC compiles today: the binding's
/// address space is not what should keep a shader alive, and the rewrite is one line.
fn fxc_assertion_problems(name: &str, module: &naga::Module, info: &naga::valid::ModuleInfo) -> Vec<String> {
    let mut problems = Vec::new();
    for (index, entry) in module.entry_points.iter().enumerate() {
        let mut scan = AssertionScan::new(module, info);
        scan.entry(&entry.function, info.get_entry_point(index), &entry.name);
        for found in &scan.found {
            problems.push(format!(
                "{name} ({}): {found}: FXC's optimizer recurses on that until the stack overflows, at \
                 any stack size (ADR-5971). Write the inner test as the outer one negated (`!(a > b)`, \
                 not `a < b`; `sign` and `step` count as comparisons), or drop it",
                entry.name
            ));
        }
        if scan.calls > ASSERTION_CALL_BUDGET {
            problems.push(format!(
                "{name} ({}): over {ASSERTION_CALL_BUDGET} call paths: the FXC assertion rule (ADR-5971) \
                 did not finish",
                entry.name
            ));
        }
    }
    problems
}

/// A value's number in an `AssertionScan`: two expressions with one number hold one value.
type ValueId = u32;

/// A value as the assertion rule follows it, built from its operands' numbers, so equal
/// expressions of equal operands get one number in any function.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Value {
    /// A scalar literal or a scalar zero: a tag for its type, and its bits.
    Literal(u8, u64),
    /// A module-scope constant expression that is not a scalar literal.
    Constant(u32),
    /// The zero value of a type that is not a scalar.
    Zero(u32),
    /// A global variable.
    Global(u32),
    /// The address of a local variable: the call it belongs to, and the variable.
    Local(u32, u32),
    /// An entry point's argument.
    Argument(u32),
    /// What an address holds: a global's contents, or a local never stored to.
    Load(ValueId),
    /// A member or component at a constant index, of a value or of an address.
    AccessIndex(ValueId, u32),
    /// An element at a dynamic index, of a value or of an address.
    Access(ValueId, ValueId),
    /// A unary operation.
    Unary(naga::UnaryOperator, ValueId),
    /// A binary operation; an ordering comparison only when its operands are scalars.
    Binary(naga::BinaryOperator, ValueId, ValueId),
    /// A vector by its components (a `vec3(..)`, a splat, a swizzle).
    Vector(Vec<ValueId>),
    /// A value with its member or component at a constant index replaced: what a local holds
    /// after a store to that member (ADR-8930).
    Stored(ValueId, u32, ValueId),
    /// An ordering comparison of two vectors with this many components.
    VectorCompare(naga::BinaryOperator, ValueId, ValueId, u32),
    /// `all(..)` of a vector of `bool`.
    All(ValueId),
    /// `any(..)` of a vector of `bool`.
    Any(ValueId),
    /// Any other pure operation: its name with its fixed parameters, and its operands.
    Other(String, Vec<ValueId>),
    /// A value the rule does not follow (a texture read, a merge of two paths): unique.
    Opaque(u32),
}

/// What an enclosing `if` holds: `low < high` (`strict`) or `low <= high`, and the function
/// the `if` is in.
#[derive(Clone, Copy)]
struct Held<'m> {
    low: ValueId,
    high: ValueId,
    strict: bool,
    label: &'m str,
}

/// One call of a function on the walk: its expressions' values under that call's arguments.
struct Activation<'m> {
    function: &'m naga::Function,
    info: &'m naga::valid::FunctionInfo,
    label: &'m str,
    id: u32,
    arguments: Vec<ValueId>,
    values: Vec<Option<ValueId>>,
    /// The value a `return` returned, once walked.
    returned: Option<ValueId>,
}

/// Walks an entry point the way FXC sees it, every call inlined, numbering values and keeping
/// what the enclosing `if`s hold; `found` names each comparison of the shape
/// `fxc_assertion_problems` describes, once per set of functions.
struct AssertionScan<'m> {
    module: &'m naga::Module,
    info: &'m naga::valid::ModuleInfo,
    numbers: std::collections::HashMap<Value, ValueId>,
    values: Vec<Value>,
    /// The current value of each local stored to (or initialized), by call and variable.
    locals: std::collections::HashMap<(u32, u32), ValueId>,
    held: Vec<Held<'m>>,
    calls: u32,
    opaque: u32,
    found: Vec<String>,
}

/// A handle's index as the `u32` a `Value` keeps.
fn index32<T>(handle: naga::Handle<T>) -> u32 {
    u32::try_from(handle.index()).unwrap_or(u32::MAX)
}

/// A scalar literal as a `Value`; `None` for the widths the engine's shaders do not use.
fn literal_value(literal: naga::Literal) -> Option<Value> {
    Some(match literal {
        naga::Literal::F32(value) => Value::Literal(0, u64::from(value.to_bits())),
        naga::Literal::F64(value) => Value::Literal(1, value.to_bits()),
        naga::Literal::U32(value) => Value::Literal(2, u64::from(value)),
        naga::Literal::I32(value) => Value::Literal(3, u64::from(value.cast_unsigned())),
        naga::Literal::Bool(value) => Value::Literal(4, u64::from(value)),
        naga::Literal::AbstractFloat(value) => Value::Literal(5, value.to_bits()),
        naga::Literal::AbstractInt(value) => Value::Literal(6, value.cast_unsigned()),
        _ => return None,
    })
}

/// The zero of a scalar type as the `Value` its literal has.
fn zero_value(scalar: naga::Scalar) -> Option<Value> {
    use naga::ScalarKind as K;
    match (scalar.kind, scalar.width) {
        (K::Float, 4) => Some(Value::Literal(0, 0)),
        (K::Float, 8) => Some(Value::Literal(1, 0)),
        (K::Uint, 4) => Some(Value::Literal(2, 0)),
        (K::Sint, 4) => Some(Value::Literal(3, 0)),
        (K::Bool, _) => Some(Value::Literal(4, 0)),
        _ => None,
    }
}

/// A function's name as its WGSL wrote it: without the module suffix `naga_oil` appends to an
/// imported item.
fn plain_name(name: &str) -> &str {
    name.split_once("X_naga_oil_mod_X")
        .map_or(name, |(plain, _)| plain)
}

/// A comparison as FXC's instruction, which has no `gt` or `le`: `lt(first, second)` (`true`)
/// or `ge(first, second)` (`false`). `None` for any other operator.
fn fxc_instruction<T>(op: naga::BinaryOperator, left: T, right: T) -> Option<(bool, T, T)> {
    use naga::BinaryOperator as B;
    match op {
        B::Less => Some((true, left, right)),
        B::Greater => Some((true, right, left)),
        B::LessEqual => Some((false, right, left)),
        B::GreaterEqual => Some((false, left, right)),
        _ => None,
    }
}

impl<'m> AssertionScan<'m> {
    fn new(module: &'m naga::Module, info: &'m naga::valid::ModuleInfo) -> Self {
        Self {
            module,
            info,
            numbers: std::collections::HashMap::new(),
            values: Vec::new(),
            locals: std::collections::HashMap::new(),
            held: Vec::new(),
            calls: 0,
            opaque: 0,
            found: Vec::new(),
        }
    }

    /// The number of `value`, a new one the first time.
    fn number(&mut self, value: Value) -> ValueId {
        if let Some(&known) = self.numbers.get(&value) {
            return known;
        }
        let id = u32::try_from(self.values.len()).unwrap_or(u32::MAX);
        self.values.push(value.clone());
        self.numbers.insert(value, id);
        id
    }

    /// A value equal to no other.
    fn fresh(&mut self) -> ValueId {
        self.opaque += 1;
        self.number(Value::Opaque(self.opaque))
    }

    /// Walks an entry point.
    fn entry(&mut self, function: &'m naga::Function, info: &'m naga::valid::FunctionInfo, label: &'m str) {
        let arguments = (0..function.arguments.len())
            .map(|index| self.number(Value::Argument(u32::try_from(index).unwrap_or(u32::MAX))))
            .collect();
        self.walk(function, info, label, arguments);
    }

    /// Walks one call of `function` with `arguments`; the value its only `return` returns.
    fn walk(
        &mut self,
        function: &'m naga::Function,
        info: &'m naga::valid::FunctionInfo,
        label: &'m str,
        arguments: Vec<ValueId>,
    ) -> Option<ValueId> {
        self.calls += 1;
        if self.calls > ASSERTION_CALL_BUDGET {
            return None;
        }
        let mut call = Activation {
            function,
            info,
            label,
            id: self.calls,
            arguments,
            values: vec![None; function.expressions.len()],
            returned: None,
        };
        for (handle, local) in function.local_variables.iter() {
            if let Some(init) = local.init {
                let value = self.value(&mut call, init);
                self.locals.insert((call.id, index32(handle)), value);
            }
        }
        self.block(&mut call, &function.body);
        call.returned.filter(|_| count_returns(&function.body) == 1)
    }

    /// The value of a module-scope constant expression.
    fn constant(&mut self, init: naga::Handle<naga::Expression>) -> ValueId {
        let value = match self.module.global_expressions[init] {
            naga::Expression::Literal(literal) => literal_value(literal),
            naga::Expression::ZeroValue(ty) => return self.zero(ty),
            _ => None,
        };
        self.number(value.unwrap_or(Value::Constant(index32(init))))
    }

    /// The zero value of `ty`: a scalar's is its literal zero.
    fn zero(&mut self, ty: naga::Handle<naga::Type>) -> ValueId {
        let value = match self.module.types[ty].inner {
            naga::TypeInner::Scalar(scalar) => zero_value(scalar),
            _ => None,
        };
        self.number(value.unwrap_or(Value::Zero(index32(ty))))
    }

    /// The scalar type of `handle` and how many of them it holds: 1 for a scalar, the size for a
    /// vector, `None` for anything else.
    fn shape(
        &self,
        call: &Activation<'m>,
        handle: naga::Handle<naga::Expression>,
    ) -> Option<(naga::Scalar, u32)> {
        match *call.info[handle].ty.inner_with(&self.module.types) {
            naga::TypeInner::Scalar(scalar) => Some((scalar, 1)),
            naga::TypeInner::Vector { size, scalar } => Some((scalar, size as u32)),
            _ => None,
        }
    }

    /// Component `index` of `base`: the component itself when `base` is a vector built here or
    /// a value with that member stored; under stores to other members, the member of the value
    /// they were stored into.
    fn component(&mut self, mut base: ValueId, index: u32) -> ValueId {
        loop {
            match &self.values[base as usize] {
                Value::Vector(components) => {
                    if let Some(&component) = components.get(index as usize) {
                        return component;
                    }
                    break;
                }
                Value::Stored(_, at, member) if *at == index => return *member,
                Value::Stored(inner, ..) => base = *inner,
                _ => break,
            }
        }
        self.number(Value::AccessIndex(base, index))
    }

    /// `parent` with its member or component `index` replaced by `member`.
    fn replace(&mut self, parent: ValueId, index: u32, member: ValueId) -> ValueId {
        let value = match &self.values[parent as usize] {
            Value::Vector(components) if (index as usize) < components.len() => {
                let mut components = components.clone();
                components[index as usize] = member;
                Value::Vector(components)
            }
            // A second store to one member: the first is gone.
            Value::Stored(inner, at, _) if *at == index => Value::Stored(*inner, index, member),
            _ => Value::Stored(parent, index, member),
        };
        self.number(value)
    }

    /// Stores `value` through the address `pointer`, member by member (FXC keeps a struct's
    /// members and a vector's components apart, ADR-8930): a store to a member at a constant
    /// index replaces that member in its parent and keeps the others; a store at a dynamic index
    /// gives the whole array or vector a value equal to no other. An address that is not in a
    /// local is not followed.
    fn assign(&mut self, pointer: ValueId, value: ValueId) {
        match self.values[pointer as usize] {
            Value::Local(call, local) => {
                self.locals.insert((call, local), value);
            }
            Value::AccessIndex(base, index) => {
                let parent = self.load(base);
                let replaced = self.replace(parent, index, value);
                self.assign(base, replaced);
            }
            Value::Access(base, _) => {
                let unknown = self.fresh();
                self.assign(base, unknown);
            }
            _ => {}
        }
    }

    /// The components of `handle`'s value: itself for a scalar, empty for anything but a scalar
    /// or a vector.
    fn components(
        &mut self,
        call: &mut Activation<'m>,
        handle: naga::Handle<naga::Expression>,
    ) -> Vec<ValueId> {
        let Some((_, count)) = self.shape(call, handle) else {
            return Vec::new();
        };
        let value = self.value(call, handle);
        if count == 1 {
            return vec![value];
        }
        (0..count).map(|index| self.component(value, index)).collect()
    }

    /// The value of `handle` in `call`. A load is numbered where it is emitted (`emitted`),
    /// since the local it reads can be stored to later; every other expression on first use,
    /// which gives the same number.
    fn value(&mut self, call: &mut Activation<'m>, handle: naga::Handle<naga::Expression>) -> ValueId {
        use naga::Expression as E;
        if let Some(known) = call.values[handle.index()] {
            return known;
        }
        let function = call.function;
        let other =
            |scan: &mut Self, name: String, operands: Vec<ValueId>| scan.number(Value::Other(name, operands));
        let value = match function.expressions[handle] {
            E::Literal(literal) => match literal_value(literal) {
                Some(value) => self.number(value),
                None => self.fresh(),
            },
            E::Constant(constant) => {
                let init = self.module.constants[constant].init;
                self.constant(init)
            }
            E::ZeroValue(ty) => self.zero(ty),
            E::GlobalVariable(global) => self.number(Value::Global(index32(global))),
            E::LocalVariable(local) => self.number(Value::Local(call.id, index32(local))),
            E::FunctionArgument(index) => match call.arguments.get(index as usize) {
                Some(&argument) => argument,
                None => self.fresh(),
            },
            E::Load { pointer } => {
                let pointer = self.value(call, pointer);
                self.load(pointer)
            }
            E::AccessIndex { base, index } => {
                let base = self.value(call, base);
                self.component(base, index)
            }
            E::Access { base, index } => {
                let base = self.value(call, base);
                let index = self.value(call, index);
                self.number(Value::Access(base, index))
            }
            E::Unary { op, expr } => {
                let expr = self.value(call, expr);
                self.number(Value::Unary(op, expr))
            }
            E::Binary { op, left, right } => {
                let ordering = fxc_instruction(op, left, right).is_some();
                let count = self.shape(call, left).map_or(0, |(_, count)| count);
                let left = self.value(call, left);
                let right = self.value(call, right);
                if ordering && count != 1 {
                    self.number(Value::VectorCompare(op, left, right, count))
                } else {
                    self.number(Value::Binary(op, left, right))
                }
            }
            E::Select {
                condition,
                accept,
                reject,
            } => {
                let operands = vec![
                    self.value(call, condition),
                    self.value(call, accept),
                    self.value(call, reject),
                ];
                other(self, "select".to_string(), operands)
            }
            E::Splat { size, value } => {
                let value = self.value(call, value);
                self.number(Value::Vector(vec![value; size as usize]))
            }
            E::Swizzle {
                size,
                vector,
                pattern,
            } => {
                let vector = self.value(call, vector);
                let components = pattern[..size as usize]
                    .iter()
                    .map(|component| self.component(vector, *component as u32))
                    .collect();
                self.number(Value::Vector(components))
            }
            E::Compose { ty, ref components } => {
                // `vec4(v.xy, a, b)` by its scalar components, so `vec4(..).z` is `a`.
                let mut scalars = Vec::new();
                if let naga::TypeInner::Vector { size, .. } = self.module.types[ty].inner {
                    for component in components {
                        scalars.extend(self.components(call, *component));
                    }
                    if scalars.len() != size as usize {
                        scalars.clear();
                    }
                }
                if scalars.is_empty() {
                    let operands = components
                        .iter()
                        .map(|component| self.value(call, *component))
                        .collect();
                    other(self, format!("compose {}", ty.index()), operands)
                } else {
                    self.number(Value::Vector(scalars))
                }
            }
            E::Relational { fun, argument } => {
                let argument = self.value(call, argument);
                match fun {
                    naga::RelationalFunction::All => self.number(Value::All(argument)),
                    naga::RelationalFunction::Any => self.number(Value::Any(argument)),
                    _ => other(self, format!("{fun:?}"), vec![argument]),
                }
            }
            E::Math {
                fun,
                arg,
                arg1,
                arg2,
                arg3,
            } => {
                let operands = [Some(arg), arg1, arg2, arg3]
                    .into_iter()
                    .flatten()
                    .map(|operand| self.value(call, operand))
                    .collect();
                other(self, format!("{fun:?}"), operands)
            }
            E::As { expr, kind, convert } => {
                let expr = self.value(call, expr);
                other(self, format!("as {kind:?} {convert:?}"), vec![expr])
            }
            E::ArrayLength(pointer) => {
                let pointer = self.value(call, pointer);
                other(self, "arrayLength".to_string(), vec![pointer])
            }
            // A texture read, a derivative, a call's or an atomic's result: one value per
            // expression, equal to no other.
            _ => self.fresh(),
        };
        call.values[handle.index()] = Some(value);
        value
    }

    /// What `pointer` holds. The load is pushed down to the variable, so `(*p).x` and `*(&p.x)`
    /// get one number.
    fn load(&mut self, pointer: ValueId) -> ValueId {
        match self.values[pointer as usize] {
            Value::Local(call, local) => match self.locals.get(&(call, local)) {
                Some(&value) => value,
                None => self.number(Value::Load(pointer)),
            },
            Value::AccessIndex(base, index) => {
                let base = self.load(base);
                self.component(base, index)
            }
            Value::Access(base, index) => {
                let base = self.load(base);
                self.number(Value::Access(base, index))
            }
            _ => self.number(Value::Load(pointer)),
        }
    }

    /// The local variable the address `pointer` points into.
    fn root(&self, mut pointer: ValueId) -> Option<(u32, u32)> {
        loop {
            match self.values[pointer as usize] {
                Value::Local(call, local) => return Some((call, local)),
                Value::AccessIndex(base, _) | Value::Access(base, _) => pointer = base,
                _ => return None,
            }
        }
    }

    /// The part of a local the pointer expression `handle` may store to, read off the
    /// expression without walking it (a dynamic index may not be emitted yet): the member at its
    /// constant indices, or the whole array or vector a dynamic index picks from. `None` for an
    /// address that is not in a local.
    fn pointer_address(
        &mut self,
        call: &Activation<'m>,
        handle: naga::Handle<naga::Expression>,
    ) -> Option<ValueId> {
        self.pointer_part(call, handle).map(|(address, _)| address)
    }

    /// `pointer_address`, and whether the address is exactly the expression's (no dynamic index
    /// on the way).
    fn pointer_part(
        &mut self,
        call: &Activation<'m>,
        handle: naga::Handle<naga::Expression>,
    ) -> Option<(ValueId, bool)> {
        match call.function.expressions[handle] {
            naga::Expression::AccessIndex { base, index } => {
                let (base, exact) = self.pointer_part(call, base)?;
                if exact {
                    Some((self.number(Value::AccessIndex(base, index)), true))
                } else {
                    Some((base, false))
                }
            }
            naga::Expression::Access { base, .. } => {
                let (base, _) = self.pointer_part(call, base)?;
                Some((base, false))
            }
            naga::Expression::LocalVariable(local) => {
                Some((self.number(Value::Local(call.id, index32(local))), true))
            }
            naga::Expression::FunctionArgument(index) => call
                .arguments
                .get(index as usize)
                .copied()
                .filter(|&argument| self.root(argument).is_some())
                .map(|argument| (argument, true)),
            _ => None,
        }
    }

    /// The parts of locals `block` may store to (`pointer_address`): its nested blocks and the
    /// pointer arguments of its calls included.
    fn stored(&mut self, call: &Activation<'m>, block: &naga::Block, out: &mut Vec<ValueId>) {
        for statement in block {
            match statement {
                naga::Statement::Store { pointer, .. } => out.extend(self.pointer_address(call, *pointer)),
                naga::Statement::Call { arguments, .. } => {
                    for argument in arguments {
                        out.extend(self.pointer_address(call, *argument));
                    }
                }
                naga::Statement::Block(inner) => self.stored(call, inner, out),
                naga::Statement::If { accept, reject, .. } => {
                    self.stored(call, accept, out);
                    self.stored(call, reject, out);
                }
                naga::Statement::Switch { cases, .. } => {
                    for case in cases {
                        self.stored(call, &case.body, out);
                    }
                }
                naga::Statement::Loop { body, continuing, .. } => {
                    self.stored(call, body, out);
                    self.stored(call, continuing, out);
                }
                _ => {}
            }
        }
    }

    /// Puts back the value `local` had (`None`: it was never stored to).
    fn restore(&mut self, local: (u32, u32), value: Option<ValueId>) {
        match value {
            Some(value) => {
                self.locals.insert(local, value);
            }
            None => {
                self.locals.remove(&local);
            }
        }
    }

    /// Where paths that may have stored to the parts `stored` meet, each part holds a value the
    /// rule does not follow (FXC does not carry a fact through such a merge either); the other
    /// members of those locals keep theirs.
    fn merge(&mut self, stored: &[ValueId]) {
        for &address in stored {
            let value = self.fresh();
            self.assign(address, value);
        }
    }

    /// The locals the parts `stored` are in, with the value each holds now.
    fn snapshot(&self, stored: &[ValueId]) -> Vec<((u32, u32), Option<ValueId>)> {
        stored
            .iter()
            .filter_map(|&address| self.root(address))
            .map(|local| (local, self.locals.get(&local).copied()))
            .collect()
    }

    /// Pushes what `condition` being `holds` says: through `!`, both sides of a true `&` and of
    /// a false `|`, every component of a true `all` and of a false `any`, down to comparisons.
    fn hold(&mut self, condition: ValueId, holds: bool, label: &'m str) {
        use naga::BinaryOperator as B;
        match self.values[condition as usize] {
            Value::Unary(naga::UnaryOperator::LogicalNot, inner) => self.hold(inner, !holds, label),
            Value::All(inner) if holds => self.hold(inner, true, label),
            Value::Any(inner) if !holds => self.hold(inner, false, label),
            Value::VectorCompare(op, left, right, count) => {
                for index in 0..count {
                    let left = self.component(left, index);
                    let right = self.component(right, index);
                    self.hold_comparison(op, left, right, holds, label);
                }
            }
            Value::Binary(B::LogicalAnd | B::And, left, right) if holds => {
                self.hold(left, true, label);
                self.hold(right, true, label);
            }
            Value::Binary(B::LogicalOr | B::InclusiveOr, left, right) if !holds => {
                self.hold(left, false, label);
                self.hold(right, false, label);
            }
            Value::Binary(op, left, right) => self.hold_comparison(op, left, right, holds, label),
            _ => {}
        }
    }

    /// Pushes what the comparison `left op right` being `holds` says; nothing for another
    /// operator.
    fn hold_comparison(
        &mut self,
        op: naga::BinaryOperator,
        left: ValueId,
        right: ValueId,
        holds: bool,
        label: &'m str,
    ) {
        if let Some((less, first, second)) = fxc_instruction(op, left, right) {
            // `lt(a, b)` taken and `ge(a, b)` not taken: `a < b`. The other two: `b <= a`.
            let held = if less == holds {
                Held {
                    low: first,
                    high: second,
                    strict: true,
                    label,
                }
            } else {
                Held {
                    low: second,
                    high: first,
                    strict: false,
                    label,
                }
            };
            self.held.push(held);
        }
    }

    /// A comparison instruction on `(first, second)` in the function `label`: found when an
    /// enclosing `if` holds `second < first` strictly (the same pair the other way round) and
    /// one holds a third value below `first` or `second`.
    fn compare(&mut self, label: &str, first: ValueId, second: ValueId) {
        let apart = self
            .held
            .iter()
            .find(|held| held.strict && held.low == second && held.high == first);
        let third = self.held.iter().find(|held| {
            (held.high == first || held.high == second) && held.low != first && held.low != second
        });
        if let (Some(apart), Some(third)) = (apart, third) {
            let found = format!(
                "`{label}` compares two values that an enclosing `if` in `{}` holds strictly apart, \
                 with the operands the other way round, while an `if` in `{}` holds a third value \
                 below one of them",
                apart.label, third.label
            );
            if !self.found.contains(&found) {
                self.found.push(found);
            }
        }
    }

    /// An emitted expression: a load is numbered here, and a comparison (`sign`, `step` and
    /// `faceForward` are comparisons to FXC) is checked, component by component, against what
    /// the enclosing `if`s hold.
    fn emitted(&mut self, call: &mut Activation<'m>, handle: naga::Handle<naga::Expression>) {
        use naga::Expression as E;
        use naga::MathFunction as M;
        let function = call.function;
        match function.expressions[handle] {
            E::Load { .. } => {
                self.value(call, handle);
            }
            E::Binary { op, left, right } => {
                if let Some((_, first, second)) = fxc_instruction(op, left, right) {
                    self.compare_each(call, first, second);
                }
            }
            // `sign(x)` is `lt(0, x)` and `lt(x, 0)`.
            E::Math {
                fun: M::Sign, arg, ..
            } => {
                if let Some(zero) = self.shape(call, arg).and_then(|(scalar, _)| zero_value(scalar)) {
                    let zero = self.number(zero);
                    for value in self.components(call, arg) {
                        self.compare(call.label, zero, value);
                        self.compare(call.label, value, zero);
                    }
                }
            }
            // `step(edge, x)` is `ge(x, edge)`.
            E::Math {
                fun: M::Step,
                arg,
                arg1: Some(value),
                ..
            } => self.compare_each(call, value, arg),
            // `faceForward(a, b, c)` is `-a * sign(dot(b, c))`.
            E::Math {
                fun: M::FaceForward,
                arg1: Some(incident),
                arg2: Some(reference),
                ..
            } => {
                if let Some(zero) = self
                    .shape(call, incident)
                    .and_then(|(scalar, _)| zero_value(scalar))
                {
                    let zero = self.number(zero);
                    let operands = vec![self.value(call, incident), self.value(call, reference)];
                    let dot = self.number(Value::Other(format!("{:?}", M::Dot), operands));
                    self.compare(call.label, zero, dot);
                    self.compare(call.label, dot, zero);
                }
            }
            _ => {}
        }
    }

    /// A comparison instruction on `(first, second)`, one per component for vectors.
    fn compare_each(
        &mut self,
        call: &mut Activation<'m>,
        first: naga::Handle<naga::Expression>,
        second: naga::Handle<naga::Expression>,
    ) {
        let first = self.components(call, first);
        let second = self.components(call, second);
        for (first, second) in first.into_iter().zip(second) {
            self.compare(call.label, first, second);
        }
    }

    fn block(&mut self, call: &mut Activation<'m>, block: &'m naga::Block) {
        for statement in block {
            if self.calls > ASSERTION_CALL_BUDGET {
                return;
            }
            match statement {
                naga::Statement::Emit(range) => {
                    for handle in range.clone() {
                        self.emitted(call, handle);
                    }
                }
                naga::Statement::Block(inner) => self.block(call, inner),
                naga::Statement::If {
                    condition,
                    accept,
                    reject,
                } => {
                    let condition = self.value(call, *condition);
                    let mut stored = Vec::new();
                    self.stored(call, accept, &mut stored);
                    let in_accept = stored.len();
                    self.stored(call, reject, &mut stored);
                    let before = self.snapshot(&stored[..in_accept]);
                    let depth = self.held.len();
                    self.hold(condition, true, call.label);
                    self.block(call, accept);
                    self.held.truncate(depth);
                    for (local, value) in before {
                        self.restore(local, value);
                    }
                    self.hold(condition, false, call.label);
                    self.block(call, reject);
                    self.held.truncate(depth);
                    self.merge(&stored);
                }
                naga::Statement::Switch { cases, .. } => {
                    let mut stored = Vec::new();
                    for case in cases {
                        self.stored(call, &case.body, &mut stored);
                    }
                    let before = self.snapshot(&stored);
                    for case in cases {
                        for &(local, value) in &before {
                            self.restore(local, value);
                        }
                        self.block(call, &case.body);
                    }
                    self.merge(&stored);
                }
                naga::Statement::Loop { body, continuing, .. } => {
                    // A local the loop stores to holds another value on each trip.
                    let mut stored = Vec::new();
                    self.stored(call, body, &mut stored);
                    self.stored(call, continuing, &mut stored);
                    self.merge(&stored);
                    self.block(call, body);
                    self.block(call, continuing);
                    self.merge(&stored);
                }
                naga::Statement::Store { pointer, value } => {
                    let pointer = self.value(call, *pointer);
                    let value = self.value(call, *value);
                    self.assign(pointer, value);
                }
                naga::Statement::Call {
                    function,
                    arguments,
                    result,
                } => {
                    let arguments = arguments
                        .iter()
                        .map(|argument| self.value(call, *argument))
                        .collect();
                    let module = self.module;
                    let info = self.info;
                    let callee = &module.functions[*function];
                    let label = plain_name(callee.name.as_deref().unwrap_or("a function"));
                    let returned = self.walk(callee, &info[*function], label, arguments);
                    if let Some(result) = result {
                        let value = match returned {
                            Some(value) => value,
                            None => self.fresh(),
                        };
                        call.values[result.index()] = Some(value);
                    }
                }
                naga::Statement::Return { value: Some(value) } => {
                    let value = self.value(call, *value);
                    call.returned = Some(value);
                }
                _ => {}
            }
        }
    }
}

/// One engine module that wgpu creates but the SPIR-V gate's list
/// (`shader_check::shader_sources`) leaves out, with the shader defs the renderer composes it
/// with.
pub(crate) struct ExtraModule {
    /// The `.wgsl` file that holds the entry points.
    pub(crate) file: &'static str,
    /// The text the renderer hands wgpu (or the composer).
    pub(crate) source: String,
    /// `Some`: composed with these defs (`compose_source`, a `ShaderSource::Naga` module);
    /// `None`: handed to wgpu as plain WGSL (`ShaderSource::Wgsl`).
    pub(crate) defs: Option<&'static [&'static str]>,
}

#[cfg(test)]
impl ExtraModule {
    /// A label with the file and its defs.
    pub(crate) fn label(&self) -> String {
        match self.defs {
            Some(defs) => format!("{} [{}]", self.file, defs.join(",")),
            None => format!("{} (plain WGSL)", self.file),
        }
    }

    /// The naga module wgpu receives.
    pub(crate) fn module(&self) -> Result<naga::Module, String> {
        match self.defs {
            Some(defs) => {
                let defs = defs
                    .iter()
                    .map(|def| ((*def).to_string(), naga_oil::compose::ShaderDefValue::Bool(true)))
                    .collect();
                crate::shader_lib::compose_source(&self.source, self.file, defs)
                    .map_err(|err| err.to_string())
            }
            None => naga::front::wgsl::parse_str(&self.source).map_err(|err| err.to_string()),
        }
    }
}

/// The engine modules (and `lab_forward.wgsl` def sets) the renderer creates beyond
/// `shader_check::shader_sources`: DDGI, baked GI, the boot splash, the display stage, local
/// exposure, compute skinning, the decal receiver, and the GPU-driven and alpha-tested forward
/// variants.
pub(crate) fn extra_engine_modules() -> Vec<ExtraModule> {
    let composed = |file: &'static str, source: &str, defs: &'static [&'static str]| ExtraModule {
        file,
        source: source.to_string(),
        defs: Some(defs),
    };
    let plain = |file: &'static str, source: String| ExtraModule {
        file,
        source,
        defs: None,
    };
    let forward = include_str!("../shaders/lab_forward.wgsl");
    let mut out = vec![
        composed("display.wgsl", include_str!("../shaders/display.wgsl"), &[]),
        composed(
            "local_exposure.wgsl",
            include_str!("../shaders/local_exposure.wgsl"),
            &[],
        ),
        composed(
            "skin_compute.wgsl",
            include_str!("../shaders/skin_compute.wgsl"),
            &[],
        ),
        plain(
            "boot-splash.wgsl",
            include_str!("../shaders/boot-splash.wgsl").to_string(),
        ),
        plain(
            "fade_overlay.wgsl",
            include_str!("../shaders/fade_overlay.wgsl").to_string(),
        ),
        // The look-review reference overlay (ADR-2463).
        plain(
            "reference_overlay.wgsl",
            include_str!("../shaders/reference_overlay.wgsl").to_string(),
        ),
        // The fog of war world overlay (`fog_overlay.rs`, audit gap FW-1).
        plain(
            "fog_of_war_overlay.wgsl",
            include_str!("../shaders/fog_of_war_overlay.wgsl").to_string(),
        ),
        plain("ddgi_trace.wgsl", crate::ddgi_gpu::trace_source()),
        plain("ddgi_blend.wgsl", crate::ddgi_gpu::blend_source()),
        plain("ddgi_screen.wgsl", crate::ddgi_gpu::screen_source()),
        plain("ddgi_maxmip.wgsl", crate::ddgi_gpu::maxmip_source().to_string()),
        plain("baked_gi_screen.wgsl", crate::baked_gi::screen_source()),
        plain("baked_gi_points.wgsl", crate::baked_gi::points::points_source()),
        plain(
            "radiance_cache_update.wgsl",
            crate::radiance_cache::update_source(),
        ),
        // Composed without defines, as `HighEndPass::ensure_ssgi_cached` builds it: the plain
        // gather's file it starts with holds the blue-noise variant's `#ifdef` (ADR-2075).
        composed("ssgi_cached.wgsl", &crate::radiance_cache::gather_source(), &[]),
        // Not in the SPIR-V gate's list (it needs a device with `EXPERIMENTAL_RAY_QUERY`); the
        // DX12 checks and the permutation matrix still read it.
        plain(
            "hwrt_shadow_ray_query.wgsl",
            crate::hwrt::RAY_QUERY_SHADOW_WGSL.to_string(),
        ),
        plain(
            "sun_trace_ray_query.wgsl",
            crate::hwrt::SUN_TRACE_RAY_QUERY_WGSL.to_string(),
        ),
        plain("decal_receiver.rs", crate::decal_receiver::SHADER.to_string()),
        // Not in the SPIR-V gate (its device has no binding arrays; `visbuffer::tests::masked` runs
        // `spirv_validate` on it, ADR-3915). The DX12 checks read it at the default table size, composed
        // as the renderer builds it: its raster stage imports the sway (ADR-3916).
        composed(
            "vis_raster_masked.wgsl",
            &crate::visbuffer::gpu::masked_raster_source().replace(
                &format!("#{{{}}}", crate::texture_table::TEXTURE_TABLE_SIZE_DEF),
                "4096",
            ),
            &[],
        ),
    ];
    // `gpu_driven.rs` (color and depth, instanced or not, alpha-tested or not, both encodings)
    // and the CPU alpha-tested forward pass (`frame.rs`, `graph_pipelines.rs`).
    let forward_sets: [&'static [&'static str]; 9] = [
        &["GPU_DRIVEN"],
        &["GPU_DRIVEN", "ALPHA_TEST"],
        &["GPU_DRIVEN", "ALPHA_TEST", "skip_srgb_encode"],
        &["GPU_DRIVEN", "GPU_INSTANCED"],
        &["GPU_DRIVEN", "GPU_INSTANCED", "skip_srgb_encode"],
        &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"],
        &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST", "skip_srgb_encode"],
        &["ALPHA_TEST"],
        &["ALPHA_TEST", "skip_srgb_encode"],
    ];
    for defs in forward_sets {
        out.push(composed("lab_forward.wgsl", forward, defs));
    }
    out
}

/// Entry shaders that never reach Direct3D 12, with the reason. Each needs a feature only
/// wgpu's Vulkan backend exposes, and its pass reports unsupported before building the module
/// on any other backend, so neither list above holds it. `xtask`'s coverage test over every
/// engine crate reads this list too (ADR-5802).
pub const VULKAN_ONLY_MODULES: &[(&str, &str)] = &[(
    "restir_gi.wgsl",
    "ray-query hit-vertex return (EXPERIMENTAL_RAY_HIT_VERTEX_RETURN): only the Vulkan backend has it \
     and naga's HLSL writer cannot write it; `RestirGiGpu::new` returns Unsupported elsewhere (ADR-3349)",
)];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use super::*;

    fn parse(source: &str) -> naga::Module {
        match naga::front::wgsl::parse_str(source) {
            Ok(module) => module,
            Err(err) => panic!("fixture does not parse: {err}"),
        }
    }

    #[test]
    fn a_small_compute_shader_has_no_dx12_problem() {
        let module = parse(
            "var<workgroup> tile: array<vec4<f32>, 2048>;\n\
             @compute @workgroup_size(32, 32, 1)\n\
             fn main(@builtin(local_invocation_index) i: u32) { tile[i] = vec4<f32>(1.0); }\n",
        );
        // 2048 × 16 bytes is exactly the 32 KiB budget; 1024 threads is exactly the cap.
        assert_eq!(dx12_module_problems("fixture", &module), Vec::<String>::new());
    }

    #[test]
    fn groupshared_over_32_kib_is_a_dx12_problem() {
        let module = parse(
            "var<workgroup> tile: array<vec4<f32>, 2049>;\n\
             @compute @workgroup_size(64)\n\
             fn main(@builtin(local_invocation_index) i: u32) { tile[i] = vec4<f32>(1.0); }\n",
        );
        let problems = dx12_module_problems("big_tile", &module);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("big_tile: ") && problems[0].contains("32784 bytes"),
            "{problems:?}"
        );
    }

    #[test]
    fn workgroup_memory_counts_every_array_the_module_declares() {
        // Two entry points, each touching one 20 KiB array: naga writes both into each
        // entry point's HLSL, so the module's 40 KiB is what counts.
        let module = parse(
            "var<workgroup> a: array<vec4<f32>, 1280>;\n\
             var<workgroup> b: array<vec4<f32>, 1280>;\n\
             @compute @workgroup_size(64)\n\
             fn use_a(@builtin(local_invocation_index) i: u32) { a[i] = vec4<f32>(1.0); }\n\
             @compute @workgroup_size(64)\n\
             fn use_b(@builtin(local_invocation_index) i: u32) { b[i] = vec4<f32>(1.0); }\n",
        );
        let problems = dx12_module_problems("split", &module);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("40960 bytes"), "{problems:?}");
    }

    #[test]
    fn thread_groups_over_the_d3d12_caps_are_dx12_problems() {
        for (size, label) in [("32, 33, 1", "1056 threads"), ("1, 1, 65", "z over 64")] {
            let module = parse(&format!(
                "@compute @workgroup_size({size})\nfn main(@builtin(global_invocation_id) id: vec3<u32>) {{ _ = id; }}\n"
            ));
            let problems = dx12_module_problems(label, &module);
            assert_eq!(problems.len(), 1, "{label}: {problems:?}");
            assert!(problems[0].contains("workgroup size"), "{label}: {problems:?}");
        }
    }

    const FLOW_PRELUDE: &str = "struct U { n: u32 }\n\
         @group(0) @binding(0) var<uniform> u: U;\n\
         @group(0) @binding(1) var<storage, read_write> out: array<f32>;\n\
         @group(0) @binding(2) var<storage, read> ro: array<u32>;\n\
         var<workgroup> flag: u32;\n\
         var<workgroup> wg: array<vec4<f32>, 64>;\n\
         struct Basis { e: array<vec3<f32>, 6>, cov: f32 }\n\
         struct Pair { x: f32, y: f32 }\n\
         fn sync() { workgroupBarrier(); }\n\
         fn axis_dir(k: u32) -> vec3<f32> { var d = vec3<f32>(0.0); d[k >> 1u] = 1.0; return d; }\n";

    /// A compute shader (`main`, 64 invocations, `i` = local index) with `functions` and `body`.
    fn compute(functions: &str, body: &str) -> naga::Module {
        parse(&format!(
            "{FLOW_PRELUDE}{functions}\n@compute @workgroup_size(64)\n\
             fn main(@builtin(local_invocation_index) i: u32) {{\n{body}\n}}\n"
        ))
    }

    /// The FXC survey's X3511 (ADR-1583): a store FXC cannot address at a dynamic index makes it
    /// unroll every enclosing loop, which fails on a loop with a runtime trip count.
    #[test]
    fn unaddressable_stores_in_runtime_loops_are_fxc_problems() {
        let cases = [
            (
                "array inside a struct (ddgi_sample.wgsl's DdgiBasis)",
                "var b: Basis;\n\
                 for (var c = 0u; c < u.n; c++) {\n\
                 for (var j = 0u; j < 6u; j++) { b.e[j] = b.e[j] + vec3<f32>(f32(c)); }\n\
                 }\nout[i] = b.e[0].x;",
            ),
            (
                "vector component (baked_gi.wgsl's wall_sum)",
                "var v = vec3<f32>(0.0);\n\
                 for (var c = 0u; c < u.n; c++) { v[c % 3u] = v[c % 3u] + 1.0; }\nout[i] = v.x;",
            ),
            (
                "matrix column",
                "var m = mat3x3<f32>();\n\
                 for (var c = 0u; c < u.n; c++) { m[c % 3u] = vec3<f32>(1.0); }\nout[i] = m[0].x;",
            ),
            (
                "through a callee (ddgi_sample.wgsl's ddgi_cube_dir)",
                "var acc = vec3<f32>(0.0);\n\
                 for (var c = 0u; c < u.n; c++) { acc = acc + axis_dir(c % 6u); }\nout[i] = acc.x;",
            ),
            (
                "in a constant loop inside a runtime one",
                "var v = vec3<f32>(0.0);\n\
                 var c = 0u;\n\
                 loop { if (c >= u.n) { break; } for (var j = 0u; j < 3u; j++) { v[j] = f32(c); } c++; }\n\
                 out[i] = v.x;",
            ),
            (
                "a runtime step",
                "var v = vec3<f32>(0.0);\n\
                 for (var c = 0u; c < 3u; c = c + u.n) { v[c] = 1.0; }\nout[i] = v.x;",
            ),
            (
                "a break-if loop",
                "var v = vec3<f32>(0.0);\n\
                 var c = 0u;\n\
                 loop { v[c % 3u] = 1.0; continuing { c = c + 1u; break if c >= 3u; } }\nout[i] = v.x;",
            ),
        ];
        for (label, body) in cases {
            let problems = dx12_module_problems(label, &compute("", body));
            assert_eq!(problems.len(), 1, "{label}: {problems:?}");
            assert!(
                problems[0].contains("(main)") && problems[0].contains("X3550"),
                "{label}: {problems:?}"
            );
        }
    }

    /// FXC addresses these (checked with `fxc.exe` on the Windows the test machine, ADR-1583), or can
    /// unroll the loop around them.
    #[test]
    fn addressable_or_unrollable_stores_pass_the_fxc_rule() {
        let cases = [
            (
                "plain local array (the DdgiBasis fix)",
                "var e: array<vec3<f32>, 6>;\n\
                 for (var c = 0u; c < u.n; c++) {\n\
                 for (var j = 0u; j < 6u; j++) { e[j] = e[j] + vec3<f32>(f32(c)); }\n\
                 }\nlet b = Basis(e, 1.0);\nout[i] = b.e[0].x;",
            ),
            (
                "array of arrays",
                "var a: array<array<f32, 3>, 2>;\n\
                 for (var c = 0u; c < u.n; c++) { a[c % 2u][c % 3u] = 1.0; }\nout[i] = a[1][2];",
            ),
            (
                "array of structs",
                "var s: array<Pair, 4>;\n\
                 for (var c = 0u; c < u.n; c++) { s[c % 4u].x = 1.0; }\nout[i] = s[3].x;",
            ),
            (
                "vector component in a constant loop",
                "var v = vec3<f32>(0.0);\n\
                 for (var c = 0u; c < 3u; c++) { v[c] = 1.0; }\nout[i] = v.x;",
            ),
            (
                "nested constant loops (regia_vfx sort_local's shape)",
                "var v = vec3<f32>(0.0);\n\
                 for (var k = 2u; k <= 8u; k = k << 1u) {\n\
                 for (var j = k >> 1u; j > 0u; j = j >> 1u) { v[j % 3u] = f32(k); }\n\
                 }\nout[i] = v.x;",
            ),
            (
                "constant component in a runtime loop",
                "var v = vec3<f32>(0.0);\n\
                 for (var c = 0u; c < u.n; c++) { v[1] = f32(c); }\nout[i] = v.y;",
            ),
            (
                "workgroup memory",
                "for (var c = i; c < u.n; c = c + 64u) { wg[c % 64u][c % 4u] = 1.0; }\nout[i] = wg[i].x;",
            ),
            (
                "built whole (the ddgi_cube_dir fix)",
                "var acc = vec3<f32>(0.0);\n\
                 for (var c = 0u; c < u.n; c++) {\n\
                 let a = c % 3u;\n\
                 acc = acc + vec3<f32>(select(0.0, 1.0, a == 0u), select(0.0, 1.0, a == 1u), select(0.0, 1.0, a == 2u));\n\
                 }\nout[i] = acc.x;",
            ),
        ];
        for (label, body) in cases {
            assert_eq!(
                dx12_module_problems(label, &compute("", body)),
                Vec::<String>::new(),
                "{label}"
            );
        }
    }

    /// The FXC survey's X4026 (ADR-1583): `ddgi_blend.wgsl` returned for a probe that was not
    /// due before `prepare_emitters`' barrier, on `load_rays`' result, which merges two returns.
    #[test]
    fn a_barrier_after_a_divergent_return_is_an_fxc_problem() {
        const LOAD: &str = "fn load(l: u32) -> u32 {\n\
             if (l == 0u) { flag = u.n; }\n\
             let due = workgroupUniformLoad(&flag);\n\
             if (due == 0u) { return 0u; }\n\
             wg[l] = vec4<f32>(1.0);\n\
             workgroupBarrier();\n\
             return 1u;\n}\n";
        const MERGED: &str = "fn due() -> u32 { if (u.n == 0u) { return 0u; } return 1u; }\n";
        let cases = [
            (
                "return on a storage read, then a barrier",
                "",
                "if (ro[0] == 0u) { return; }\nworkgroupBarrier();\nout[i] = 1.0;",
                "after a divergent `return` in `main`",
            ),
            (
                "return on a merged result, then a callee's barrier",
                MERGED,
                "if (due() == 0u) { return; }\nsync();\nout[i] = 1.0;",
                "(in `sync`) after a divergent `return` in `main`",
            ),
            (
                "return on a local, then a uniform load",
                "",
                "var k = 0u;\nif (u.n > 2u) { k = 1u; }\nif (k == 0u) { return; }\nout[i] = f32(workgroupUniformLoad(&flag));",
                "after a divergent `return` in `main`",
            ),
            (
                "ddgi_blend.wgsl's old load_rays",
                LOAD,
                "if (load(i) == 0u) { return; }\nif (u.n > 1u) { sync(); }\nout[i] = 1.0;",
                "(in `sync`) after a divergent `return` in `main`",
            ),
            (
                "a barrier in a loop with a return",
                "",
                "for (var c = 0u; c < 4u; c++) { workgroupBarrier(); if (u.n == c) { return; } }\nout[i] = 1.0;",
                "in a loop that holds a `return` in `main`",
            ),
        ];
        for (label, functions, body, want) in cases {
            let problems = dx12_module_problems(label, &compute(functions, body));
            assert_eq!(problems.len(), 1, "{label}: {problems:?}");
            assert!(
                problems[0].contains("(main)") && problems[0].contains("X4026") && problems[0].contains(want),
                "{label}: {problems:?}"
            );
        }
    }

    /// Shapes FXC accepts: every barrier first, or a return on a value it proves uniform
    /// (`sssr_reproject.wgsl` returns on a `workgroupUniformLoad` result before its barriers).
    #[test]
    fn barriers_before_any_divergent_return_pass_the_fxc_rule() {
        const LOAD: &str = "fn load(l: u32) -> u32 {\n\
             if (l == 0u) { flag = u.n; }\n\
             let due = workgroupUniformLoad(&flag);\n\
             for (var r = l; r < select(0u, 64u, due != 0u); r = r + 64u) { wg[r] = vec4<f32>(1.0); }\n\
             workgroupBarrier();\n\
             return due;\n}\n";
        const TILE: &str = "fn tile(l: u32) -> u32 {\n\
             if (l == 0u) { flag = ro[0]; }\n\
             return workgroupUniformLoad(&flag);\n}\n";
        let cases = [
            (
                "barrier, then return",
                "",
                "workgroupBarrier();\nif (ro[0] == 0u) { return; }\nout[i] = 1.0;",
            ),
            (
                "ddgi_blend.wgsl's fixed load_rays",
                LOAD,
                "let due = load(i);\nif (u.n > 1u) { sync(); }\nif (due == 0u) { return; }\nout[i] = 1.0;",
            ),
            (
                "sssr_reproject.wgsl's tile_origin",
                TILE,
                "let t = tile(i);\nif (t == 0xffffffffu) { return; }\nworkgroupBarrier();\nout[i] = f32(t);",
            ),
            (
                "a uniform-buffer condition",
                "",
                "if (u.n == 0u) { return; }\nworkgroupBarrier();\nout[i] = 1.0;",
            ),
            (
                "a callee's own return",
                "fn early(k: u32) -> u32 { if (k == 0u) { return 0u; } return 1u; }\n",
                "let e = early(u.n);\nworkgroupBarrier();\nout[i] = f32(e);",
            ),
            (
                "an unconditional return",
                "",
                "workgroupBarrier();\nout[i] = 1.0;\nreturn;",
            ),
        ];
        for (label, functions, body) in cases {
            assert_eq!(
                dx12_module_problems(label, &compute(functions, body)),
                Vec::<String>::new(),
                "{label}"
            );
        }
    }

    /// The entry points FXC rejected in the Windows survey of integ/v023 (ADR-1564: X4026 in
    /// `blend_irradiance`, X3511 in both screen lookups) pass FXC's addressing and barrier
    /// rules after ADR-1583, by name (`extra_engine_modules_have_no_dx12_problem` covers them
    /// too, but a renamed entry point would drop out of this one loudly).
    #[test]
    fn the_entry_points_fxc_rejected_pass_its_rules() {
        let modules = [
            (
                "ddgi_blend.wgsl",
                crate::ddgi_gpu::blend_source(),
                &["blend_irradiance", "blend_visibility"][..],
            ),
            (
                "ddgi_screen.wgsl",
                crate::ddgi_gpu::screen_source(),
                &["screen_lookup"][..],
            ),
            (
                "baked_gi_screen.wgsl",
                crate::baked_gi::screen_source(),
                &["baked_screen_lookup"][..],
            ),
        ];
        for (file, source, entries) in modules {
            let module = parse(&source);
            for want in entries {
                assert!(
                    module.entry_points.iter().any(|entry| entry.name == *want),
                    "{file} has no entry point {want}"
                );
            }
            let info = match naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            {
                Ok(info) => info,
                Err(err) => panic!("{file}: {err}"),
            };
            assert_eq!(
                fxc_flow_problems(file, &module, &info),
                Vec::<String>::new(),
                "{file}"
            );
        }
    }

    const ASSERTION_PRELUDE: &str = "struct Frame { cloud: vec4<f32>, other: vec4<f32>, flags: vec4<u32> }\n\
         @group(0) @binding(0) var<uniform> frame: Frame;\n\
         fn old_lookup(params: vec4<f32>, y: f32) -> f32 {\n\
         if (params.w < 0.5 || y < 0.01) { return 1.0; }\nreturn 0.25;\n}\n\
         fn new_lookup(params: vec4<f32>, y: f32) -> f32 {\n\
         if (!(params.w > 0.5) || y < 0.01) { return 1.0; }\nreturn 0.25;\n}\n\
         fn clouds_on() -> bool { return frame.cloud.w > 0.5; }\n\
         @group(0) @binding(1) var<storage, read> ground: array<vec4<f32>>;\n\
         fn old_horizon() -> f32 {\nif (ground[2].w < 0.5) { return 0.0; }\nreturn 1.0;\n}\n\
         struct P { k: f32, w: f32, junk: f32 }\n\
         struct Q { p: P, n: f32 }\n\
         fn set_junk(p: ptr<function, P>, x: f32) { (*p).junk = x; }\n";

    /// A fragment shader (`fs`) whose body is `body`, with the input `c`, a running `sun`, and
    /// two uniform values: `u` (`frame.other.x`) and `w` (`frame.cloud.w`).
    fn lit(body: &str) -> naga::Module {
        parse(&format!(
            "{ASSERTION_PRELUDE}@fragment\nfn fs(@location(0) c: vec4<f32>) -> @location(0) vec4<f32> {{\n\
             var sun = c.rgb;\nlet u = frame.other.x;\nlet w = frame.cloud.w;\n{body}\n\
             return vec4<f32>(sun, 1.0);\n}}\n"
        ))
    }

    /// Shapes `d3dcompiler_47.dll` overflowed its stack on (ADR-5971): each fixture's HLSL, as
    /// naga writes it, was compiled on the Windows min-spec laptop with wgpu's flags on a 2 MiB
    /// thread. The first is the cloud shadow lookup as `lab_forward.wgsl` called it.
    #[test]
    fn a_comparison_the_other_way_round_under_itself_is_an_fxc_problem() {
        let cases = [
            (
                "assert a callee re-tests its caller's switch",
                "if (u < 0.5) { if (frame.cloud.w > 0.5) { sun *= old_lookup(frame.cloud, c.y); } }",
                "`old_lookup` compares",
            ),
            (
                "assert nested ifs",
                "if (u < 0.5) { if (w > 0.5) { if (w < 0.5) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert the nest the other way round and a select",
                "if (w > 0.5) { if (u <= 0.5) { sun *= select(0.25, 1.0, w < 0.5); } }",
                "`fs` compares",
            ),
            (
                "assert at least under above",
                "if (u < 0.5) { if (w > 0.5) { if (w >= 0.5) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert held by an else branch",
                "if (u < 0.5) { if (w <= 0.5) { sun *= 2.0; } else { if (0.5 > w) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert below then above",
                "if (u < 0.5) { if (w < 0.5) { if (w > 0.5) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert the third value in an else branch",
                "if (u > 0.5) { sun *= 2.0; } else { if (w > 0.5) { if (w < 0.5) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert sign under a strict test against zero",
                "if (u < 0.0) { if (w > 0.0) { sun *= sign(w); } }",
                "`fs` compares",
            ),
            (
                "assert step under its own edge",
                "if (u < 0.5) { if (w > 0.5) { sun *= step(0.5, w); } }",
                "`fs` compares",
            ),
            (
                "assert a switch kept in a variable",
                "var on = w > 0.5;\nif (u < 0.5) { if (on) { if (w < 0.5) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert a switch a callee returns",
                "if (u < 0.5) { if (clouds_on()) { if (frame.cloud.w < 0.5) { sun *= 0.25; } } }",
                "holds strictly apart",
            ),
            (
                "assert the third value below the tested one",
                "let k = frame.other.y;\nif (u < w) { if (w > k) { if (w < k) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert unsigned flags",
                "let fu = frame.flags.x;\nlet fw = frame.flags.y;\n\
                 if (fu < 2u) { if (fw > 2u) { if (fw < 2u) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert the inner test in a loop",
                "if (u < 0.5) { if (w > 0.5) {\n\
                 for (var i = 0; i < 4; i++) { if (w < 0.5) { sun *= 0.9; } }\n} }",
                "`fs` compares",
            ),
            (
                "assert a vector comparison holds the tested component",
                "if (u < 0.5) { if (w > 0.5) {\n\
                 if (any(vec2<f32>(w, c.x) < vec2<f32>(0.5))) { sun *= 0.25; }\n} }",
                "`fs` compares",
            ),
            (
                "assert all of a vector test holds each component",
                "if (u < 0.5) { if (all(frame.cloud.zw > vec2<f32>(0.5))) { if (w < 0.5) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert sign of a vector",
                "if (u < 0.0) { if (w > 0.0) { sun *= sign(vec2<f32>(w, c.x)).x; } }",
                "`fs` compares",
            ),
            (
                "assert faceForward under its own dot",
                "let d = dot(c.xyz, frame.other.xyz);\n\
                 if (u < 0.0) { if (d > 0.0) { sun *= faceForward(c.xyz, c.xyz, frame.other.xyz); } }",
                "`fs` compares",
            ),
            // FXC keeps a struct's members apart (ADR-8930): a store to one member leaves what
            // it holds about the others.
            (
                "assert a store to another member between the tests",
                "var p: P;\np.k = 0.5;\np.w = w;\n\
                 if (u < 0.5) { if (p.w > p.k) { p.junk = c.x; if (p.w < p.k) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert a store to a nested member between the tests",
                "var q: Q;\nq.p.k = 0.5;\nq.p.w = w;\n\
                 if (u < 0.5) { if (q.p.w > q.p.k) { q.n = c.x; q.p.junk = c.y; if (q.p.w < q.p.k) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert a branch storing another member between the tests",
                "var p: P;\np.k = 0.5;\np.w = w;\n\
                 if (u < 0.5) { if (p.w > p.k) { if (c.y > 0.0) { p.junk = c.x; }\n\
                 if (p.w < p.k) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert a callee storing another member between the tests",
                "var p: P;\np.k = 0.5;\np.w = w;\n\
                 if (u < 0.5) { if (p.w > p.k) { set_junk(&p, c.x); if (p.w < p.k) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
            (
                "assert a store to another vector component between the tests",
                "var v = vec3<f32>(0.5, w, 0.0);\n\
                 if (u < 0.5) { if (v.y > v.x) { v.z = c.x; if (v.y < v.x) { sun *= 0.25; } } }",
                "`fs` compares",
            ),
        ];
        for (label, body, want) in cases {
            let problems = dx12_module_problems(label, &lit(body));
            assert_eq!(problems.len(), 1, "{label}: {problems:?}");
            assert!(
                problems[0].contains("(fs)")
                    && problems[0].contains("ADR-5971")
                    && problems[0].contains(want),
                "{label}: {problems:?}"
            );
        }
    }

    /// Shapes `d3dcompiler_47.dll` compiled (same host, flags and stack): the first is the cloud
    /// shadow lookup as it is written now.
    #[test]
    fn the_outer_test_negated_and_unrelated_tests_pass_the_fxc_assertion_rule() {
        let cases = [
            (
                "pass the lookup tests its switch as the caller's test negated",
                "if (u < 0.5) { if (frame.cloud.w > 0.5) { sun *= new_lookup(frame.cloud, c.y); } }",
            ),
            (
                "pass no third value",
                "if (w > 0.5) { if (w < 0.5) { sun *= 0.25; } }",
            ),
            (
                "pass the third value against another constant",
                "if (u < 0.25) { if (w > 0.5) { if (w < 0.5) { sun *= 0.25; } } }",
            ),
            (
                "pass the third value above",
                "if (u > 0.5) { if (w > 0.5) { if (w < 0.5) { sun *= 0.25; } } }",
            ),
            (
                "pass the same way round",
                "if (u < 0.5) { if (w > 0.5) { if (w > 0.5) { sun *= 0.25; } if (w <= 0.5) { sun *= 0.5; } } }",
            ),
            (
                "pass not strictly apart",
                "if (u < 0.5) { if (w >= 0.5) { if (w < 0.5) { sun *= 0.25; } } }",
            ),
            (
                "pass another constant inside",
                "if (u < 0.5) { if (w > 0.5) { if (w < 0.75) { sun *= 0.25; } } }",
            ),
            (
                "pass another value inside",
                "if (u < 0.5) { if (w > 0.5) { if (c.x < 0.5) { sun *= 0.25; } } }",
            ),
            (
                "pass an early return holds the third value",
                "if (u >= 0.5) { return vec4<f32>(sun, 0.5); }\nif (w > 0.5) { if (w < 0.5) { sun *= 0.25; } }",
            ),
            (
                "pass the third value beside the nest",
                "if (u < 0.5) { sun *= 2.0; }\nif (w > 0.5) { if (w < 0.5) { sun *= 0.25; } }",
            ),
            (
                "pass both tests in one short-circuit condition",
                "if (u < 0.5 && w > 0.5) { if (w < 0.5) { sun *= 0.25; } }",
            ),
            (
                "pass a variable stored between the two tests",
                "var d = w;\nif (u < 0.5) { if (d > 0.5) { d = d - 1.0; if (d < 0.5) { sun *= 0.25; } } }",
            ),
            (
                "pass the tested member stored between the two tests",
                "var p: P;\np.k = 0.5;\np.w = w;\n\
                 if (u < 0.5) { if (p.w > p.k) { p.w = p.w - 1.0; if (p.w < p.k) { sun *= 0.25; } } }",
            ),
            (
                "pass the whole struct stored between the two tests",
                "var p: P;\np.k = 0.5;\np.w = w;\n\
                 if (u < 0.5) { if (p.w > p.k) { p = P(0.5, w - 1.0, 0.0); if (p.w < p.k) { sun *= 0.25; } } }",
            ),
            (
                "pass a callee's store, then the tested member stored between the two tests",
                "var p: P;\np.k = 0.5;\np.w = w;\n\
                 if (u < 0.5) { if (p.w > p.k) { set_junk(&p, c.x); p.w = c.y; if (p.w < p.k) { sun *= 0.25; } } }",
            ),
            (
                "pass a branch storing the tested member between the two tests",
                "var p: P;\np.k = 0.5;\np.w = w;\n\
                 if (u < 0.5) { if (p.w > p.k) { if (c.y > 0.0) { p.w = c.x; }\n\
                 if (p.w < p.k) { sun *= 0.25; } } }",
            ),
            (
                "pass a component stored at a dynamic index between the two tests",
                "var v = vec3<f32>(0.5, w, 0.0);\n\
                 if (u < 0.5) { if (v.y > v.x) { v[u32(c.x)] = c.y; if (v.y < v.x) { sun *= 0.25; } } }",
            ),
            (
                "pass step the same way round",
                "if (u < 0.5) { if (w > 0.5) { sun *= step(w, 0.5); } }",
            ),
            (
                "pass equality inside",
                "if (u < 0.5) { if (w > 0.5) { if (w == 0.5) { sun *= 0.25; } } }",
            ),
            (
                "pass a vector comparison of other values",
                "if (u < 0.5) { if (w > 0.5) { if (any(c.xy < vec2<f32>(0.5))) { sun *= 0.25; } } }",
            ),
            (
                "pass min max clamp and smoothstep of the tested value",
                "if (u < 0.0) { if (w > 0.0) {\n\
                 sun *= max(w, 0.0) + min(w, 0.0) + clamp(w, 0.0, 1.0) + smoothstep(0.0, 1.0, w) + abs(w);\n} }",
            ),
        ];
        for (label, body) in cases {
            assert_eq!(
                dx12_module_problems(label, &lit(body)),
                Vec::<String>::new(),
                "{label}"
            );
        }
    }

    /// What the rule flags although `d3dcompiler_47.dll` compiles it (same host, flags and stack):
    /// the switch is a float read from a storage buffer, `terrain_horizon_tan`'s shape before
    /// ADR-5971. Flagged on purpose: whether a binding is a storage or a uniform buffer should not
    /// decide whether the forward shader compiles, and the rewrite is one line.
    #[test]
    fn a_storage_buffer_switch_fxc_lets_off_is_still_an_assertion_problem() {
        let label = "over a storage buffer switch";
        let body = "if (u < 0.5) { if (ground[2].w > 0.5) { sun *= old_horizon(); } }";
        let problems = dx12_module_problems(label, &lit(body));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("(fs)") && problems[0].contains("`old_horizon` compares"),
            "{problems:?}"
        );
    }

    /// `module`'s FXC assertion problems.
    fn assertion_problems(name: &str, module: &naga::Module) -> Vec<String> {
        let info = match naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(module)
        {
            Ok(info) => info,
            Err(err) => panic!("{name}: {err}"),
        };
        fxc_assertion_problems(name, module, &info)
    }

    /// The entry points `d3dcompiler_47.dll` overflowed its stack on at b80097360, per file: the
    /// sweep of that commit's Shader Model 5.1 dump on the Windows min-spec laptop (2 MiB thread,
    /// wgpu's flags, ADR-5971). ADR-1583's `fxc.exe` survey had seen the first three.
    const FXC_OVERFLOWED: &[(&str, &[&str])] = &[
        (
            "lab_forward.wgsl",
            &[
                "fs_main",
                "fs_transmit_add",
                "fs_glass_add",
                "fs_subsurface",
                "fs_oit",
            ],
        ),
        ("vis_resolve.wgsl", &["fs_vis_resolve"]),
    ];

    /// The other readers of the cloud shadow lookup: the same sweep compiled every entry point
    /// of theirs (no `if` around their read holds a third value).
    const FXC_COMPILED_READERS: &[&str] = &["volumetric_fog.wgsl", "clouds.wgsl"];

    /// Every module of the files above that the DX12 checks read (the SPIR-V gate's list and
    /// the extra forward def sets): a label, the file, its source and its defs.
    fn lookup_reader_modules() -> Vec<(
        String,
        &'static str,
        String,
        std::collections::HashMap<String, naga_oil::compose::ShaderDefValue>,
    )> {
        let listed = |file: &str| {
            FXC_OVERFLOWED.iter().any(|(name, _)| *name == file) || FXC_COMPILED_READERS.contains(&file)
        };
        let mut modules = Vec::new();
        for (name, source, defs) in crate::shader_check::shader_sources() {
            if listed(name) {
                let mut keys: Vec<&str> = defs.keys().map(String::as_str).collect();
                keys.sort_unstable();
                modules.push((
                    format!("{name} [{}]", keys.join(",")),
                    name,
                    source.to_string(),
                    defs,
                ));
            }
        }
        for extra in extra_engine_modules() {
            if let Some(defs) = extra.defs.filter(|_| listed(extra.file)) {
                let defs = defs
                    .iter()
                    .map(|def| ((*def).to_string(), naga_oil::compose::ShaderDefValue::Bool(true)))
                    .collect();
                modules.push((extra.label(), extra.file, extra.source.clone(), defs));
            }
        }
        modules
    }

    /// The rule against FXC itself on the shader that started it. With the cloud shadow lookup's
    /// old switch (`params.w < 0.5`, composed in through the shader override map) the rule finds
    /// the lookup in exactly the entry points FXC overflowed on, in every def set, and in no
    /// entry point FXC compiled: not the forward shader's other entry points, and not the fog's
    /// and the clouds' own reads of the same lookup. (With the shipped text it finds nothing:
    /// `engine_shader_list_has_no_dx12_problem`, `extra_engine_modules_have_no_dx12_problem`.)
    #[test]
    fn the_old_cloud_lookup_is_found_where_fxc_overflowed_and_nowhere_else() {
        let lookup = include_str!("../shaders/cloud_shadow.wgsl");
        let old_lookup = lookup.replace("!(params.w > 0.5)", "params.w < 0.5");
        assert_ne!(
            old_lookup, lookup,
            "cloud_shadow.wgsl no longer tests `!(params.w > 0.5)`"
        );
        let mut old = crate::shader_reload::OverrideMap::new();
        old.insert("cloud_shadow.wgsl".to_string(), old_lookup.into());
        let modules = lookup_reader_modules();
        let (mut overflowed, mut compiled) = (0usize, 0usize);
        for (label, file, source, defs) in modules {
            let module = match crate::shader_lib::compose_with_overrides(&source, file, defs, &[], Some(&old))
            {
                Ok(module) => module,
                Err(err) => panic!("{label}: {err}"),
            };
            let problems = assertion_problems(&label, &module);
            for problem in &problems {
                assert!(
                    problem.contains("`cloud_sun_transmittance") && problem.contains("ADR-5971"),
                    "{label}: a finding that is not the lookup: {problem}"
                );
            }
            let overflowing = FXC_OVERFLOWED
                .iter()
                .find(|(name, _)| *name == file)
                .map_or(&[][..], |(_, entries)| *entries);
            let mut want: Vec<&str> = module
                .entry_points
                .iter()
                .map(|entry| entry.name.as_str())
                .filter(|entry| overflowing.contains(entry))
                .collect();
            want.sort_unstable();
            let mut found: Vec<&str> = module
                .entry_points
                .iter()
                .map(|entry| entry.name.as_str())
                .filter(|entry| {
                    problems
                        .iter()
                        .any(|problem| problem.contains(&format!("({entry})")))
                })
                .collect();
            found.sort_unstable();
            assert_eq!(found, want, "{label}: {problems:?}");
            if want.is_empty() {
                compiled += 1;
            } else {
                overflowed += 1;
            }
        }
        // Not vacuous: the forward def sets and the resolve on one side, the fog and the clouds
        // (and the forward sets with no lit entry point) on the other.
        assert!(
            overflowed >= 12 && compiled >= 2,
            "{overflowed} and {compiled} modules"
        );
    }

    const GRADIENT_PRELUDE: &str = "@group(0) @binding(0) var t: texture_2d<f32>;\n\
         @group(0) @binding(1) var s: sampler;\n\
         fn tap(uv: vec2<f32>) -> f32 { return textureSample(t, s, uv).r; }\n\
         fn tap_level(uv: vec2<f32>) -> f32 { return textureSampleLevel(t, s, uv, 0.0).r; }\n";

    /// A fragment shader whose body is `body` (with `uv` and a running `acc`).
    fn fragment(body: &str) -> naga::Module {
        parse(&format!(
            "{GRADIENT_PRELUDE}@fragment\nfn fs(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {{\n\
             var acc = 0.0;\n{body}\nreturn vec4<f32>(acc);\n}}\n"
        ))
    }

    #[test]
    fn an_implicit_derivative_inside_a_loop_is_an_fxc_problem() {
        let cases = [
            (
                "direct sample",
                "for (var i = 0u; i < 4u; i++) { acc += textureSample(t, s, uv * f32(i)).r; }",
            ),
            (
                "through a callee",
                "for (var i = 0u; i < 4u; i++) { acc += tap(uv * f32(i)); }",
            ),
            (
                "derivative",
                "loop { acc += fwidth(uv.x); if (acc > 1.0) { break; } }",
            ),
            (
                "biased",
                "for (var i = 0u; i < 2u; i++) { acc += textureSampleBias(t, s, uv, 1.0).r; }",
            ),
        ];
        for (label, body) in cases {
            let problems = dx12_module_problems(label, &fragment(body));
            assert_eq!(problems.len(), 1, "{label}: {problems:?}");
            assert!(
                problems[0].contains("(fs)") && problems[0].contains("X3511"),
                "{label}: {problems:?}"
            );
        }
    }

    #[test]
    fn explicit_levels_in_loops_and_implicit_ones_outside_pass_the_fxc_rule() {
        let cases = [
            (
                "explicit level",
                "for (var i = 0u; i < 4u; i++) { acc += tap_level(uv * f32(i)); }",
            ),
            (
                "outside the loop",
                "acc = tap(uv);\nfor (var i = 0u; i < 4u; i++) { acc += f32(i); }",
            ),
            (
                "grad",
                "for (var i = 0u; i < 4u; i++) { acc += textureSampleGrad(t, s, uv, vec2<f32>(0.0), vec2<f32>(0.0)).r; }",
            ),
        ];
        for (label, body) in cases {
            assert_eq!(
                dx12_module_problems(label, &fragment(body)),
                Vec::<String>::new(),
                "{label}"
            );
        }
    }

    #[test]
    fn plain_wgsl_failures_name_their_source() {
        let ok = "@fragment\nfn main() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }\n";
        assert_eq!(wgsl_dx12_problems(&[("ok.wgsl", ok)]), Vec::<String>::new());
        let problems = wgsl_dx12_problems(&[("ok.wgsl", ok), ("broken.wgsl", "fn (")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("broken.wgsl: wgsl parse"), "{problems:?}");
    }

    /// The modules `shader_check::shader_sources` leaves out pass naga's HLSL writer (5.1 and
    /// 6.0), the Direct3D 12 compute limits and FXC's loop rule.
    #[test]
    fn extra_engine_modules_have_no_dx12_problem() {
        let mut problems = Vec::new();
        let extras = extra_engine_modules();
        for extra in &extras {
            let label = extra.label();
            match extra.module() {
                Ok(module) => problems.extend(dx12_module_problems(&label, &module)),
                Err(err) => problems.push(format!("{label}: {err}")),
            }
        }
        assert!(
            problems.is_empty(),
            "{} problem(s) in {} modules:\n{}",
            problems.len(),
            extras.len(),
            problems.join("\n")
        );
    }

    /// The SPIR-V gate's list: `shader_hlsl_check` writes it as Shader Model 6.0; this adds 5.1
    /// (FXC), the Direct3D 12 compute limits and FXC's loop rule, which naga does not check.
    #[test]
    fn engine_shader_list_has_no_dx12_problem() {
        let mut problems = Vec::new();
        let mut checked = 0usize;
        for (name, source, defs) in crate::shader_check::shader_sources() {
            match crate::shader_lib::compose_source(source, name, defs) {
                Ok(module) => {
                    problems.extend(dx12_module_problems(name, &module));
                    checked += 1;
                }
                Err(err) => problems.push(format!("{name}: compose: {err}")),
            }
        }
        assert!(checked > 0, "no module was checked");
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// Every `.wgsl` file of this crate with an entry point is in the SPIR-V gate's list or in
    /// [`extra_engine_modules`], so a new shader cannot skip the DX12 checks.
    #[test]
    fn every_entry_shader_file_is_checked() {
        let mut covered: HashSet<String> = crate::shader_check::shader_sources()
            .into_iter()
            .map(|(name, _, _)| name.to_string())
            .collect();
        covered.extend(
            extra_engine_modules()
                .into_iter()
                .map(|extra| extra.file.to_string()),
        );
        covered.extend(VULKAN_ONLY_MODULES.iter().map(|(file, _)| (*file).to_string()));
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut missing = Vec::new();
        let mut seen = 0usize;
        let mut dirs = vec![root.join("shaders"), root.join("src")];
        while let Some(dir) = dirs.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) => panic!("reading {}: {err}", dir.display()),
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("wgsl") {
                    continue;
                }
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(err) => panic!("reading {}: {err}", path.display()),
                };
                let has_entry = text.lines().any(|line| {
                    let code = line.split("//").next().unwrap_or("");
                    ["@compute", "@vertex", "@fragment"]
                        .iter()
                        .any(|attr| code.contains(attr))
                });
                if !has_entry {
                    continue;
                }
                seen += 1;
                let file = entry.file_name().to_string_lossy().into_owned();
                if !covered.contains(&file) {
                    missing.push(path.strip_prefix(root).unwrap_or(&path).display().to_string());
                }
            }
        }
        assert!(seen > 0, "no entry shader found under {}", root.display());
        missing.sort();
        assert!(
            missing.is_empty(),
            "entry shaders neither in shader_check::shader_sources nor in extra_engine_modules: {}",
            missing.join(", ")
        );
    }

    /// A Vulkan-only entry really uses hit-vertex return, so the list cannot hide an ordinary
    /// shader from the DX12 checks.
    #[test]
    fn vulkan_only_modules_need_hit_vertex_return() {
        let sources = [("restir_gi.wgsl", crate::restir_gi::SOURCE)];
        for (file, reason) in VULKAN_ONLY_MODULES {
            assert!(!reason.is_empty(), "{file}: no reason");
            let source = sources
                .iter()
                .find(|(name, _)| name == file)
                .map(|(_, source)| *source)
                .unwrap_or_else(|| panic!("{file}: no composed source in this test"));
            assert!(
                source.contains("enable wgpu_ray_query_vertex_return;"),
                "{file} does not use hit-vertex return"
            );
        }
    }
}
