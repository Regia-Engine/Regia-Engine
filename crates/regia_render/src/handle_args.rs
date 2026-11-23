// SPDX-License-Identifier: MPL-2.0
//! Texture and sampler function parameters, specialised away before SPIR-V.
//!
//! WGSL lets a function take a `texture_*` or `sampler*` parameter; naga writes it
//! as a SPIR-V `OpFunctionParameter` of image type. Arm's Mali Vulkan compiler
//! (r54p3 on the Pixel 6's Mali-G78, 2026-10-08) segfaults inside
//! `vkCreate*Pipelines` when `OpImageFetch`, `OpImageWrite`, `OpImageQueryLevels`
//! or `OpImageQuerySamples` (`textureLoad`, `textureStore`, `textureNumLevels`,
//! `textureNumSamples`) read an image that arrived as such a parameter: its
//! front end traces every image back to its descriptor and finds no variable.
//! Sampling, gathers and `textureDimensions` through a parameter compile.
//!
//! [`specialize_handle_arguments`] removes the construct from the module:
//! every call that passes a global texture or sampler gets a copy of the callee
//! with that parameter replaced by the global itself (recursively, so helpers
//! that forward the parameter are specialised too). Originals that nothing calls
//! any more are reduced to a stub returning zero, so no parameter image op
//! reaches the driver. It is the classic compiler technique of procedure cloning
//! (Cooper, Hall and Kennedy, "Procedure cloning", ICCL 1992), applied only to
//! resource handles, which WGSL only allows to come from a global or another
//! parameter.
//!
//! [`image_parameter_hazards`] lists what the pass could not remove; the shader
//! check fails on any (Mali crashes are not visible to `spirv-val` or desktop
//! drivers).

use std::collections::HashMap;

use naga::{Arena, Block, Expression, Function, Handle, ImageQuery, Module, Span, Statement, TypeInner};

/// Global variable bound to each parameter of a specialised function
/// (`None`: still a parameter).
type Bindings = Vec<Option<Handle<naga::GlobalVariable>>>;

fn is_handle(module: &Module, ty: Handle<naga::Type>) -> bool {
    matches!(
        module.types[ty].inner,
        TypeInner::Image { .. } | TypeInner::Sampler { .. }
    )
}

/// Specialises every function call that passes a global texture or sampler (see the
/// module docs). Returns whether the module changed. The module's entry points keep
/// their exact behaviour; the result validates whenever the input does.
pub fn specialize_handle_arguments(module: &mut Module) -> bool {
    let has_handle_params = module
        .functions
        .iter()
        .any(|(_, f)| f.arguments.iter().any(|a| is_handle(module, a.ty)));
    if !has_handle_params {
        return false;
    }
    let handle_params: Vec<Vec<bool>> = module
        .functions
        .iter()
        .map(|(_, f)| f.arguments.iter().map(|a| is_handle(module, a.ty)).collect())
        .collect();
    let originals: Vec<(Function, Span)> = module
        .functions
        .iter()
        .map(|(h, f)| (f.clone(), module.functions.get_span(h)))
        .collect();
    let mut builder = Builder {
        originals: &originals,
        handle_params: &handle_params,
        new: Arena::new(),
        plain: Vec::with_capacity(originals.len()),
        specs: HashMap::new(),
    };
    // Original order keeps every callee ahead of its callers (naga rejects forward calls);
    // a specialisation is appended just before the function that first needs it.
    for (original, span) in &originals {
        let mut function = original.clone();
        builder.rewrite_calls(&mut function);
        let handle = builder.new.append(function, *span);
        builder.plain.push(handle);
    }
    for entry in &mut module.entry_points {
        builder.rewrite_calls(&mut entry.function);
    }
    let mut functions = builder.new;

    // Stub every function that still takes a handle and no entry point reaches: the plain
    // copies, and partial specialisations made inside them.
    let mut reachable = vec![false; functions.len()];
    let mut pending = Vec::new();
    for entry in &module.entry_points {
        collect_calls(&entry.function.body, &mut pending);
    }
    while let Some(handle) = pending.pop() {
        if let Some(seen) = reachable.get_mut(handle.index())
            && !*seen
        {
            *seen = true;
            collect_calls(&functions[handle].body, &mut pending);
        }
    }
    let types = &module.types;
    for (handle, function) in functions.iter_mut() {
        let has_handle_param = function.arguments.iter().any(|a| {
            matches!(
                types[a.ty].inner,
                TypeInner::Image { .. } | TypeInner::Sampler { .. }
            )
        });
        if has_handle_param && !reachable[handle.index()] {
            stub(function);
        }
    }
    module.functions = functions;
    true
}

struct Builder<'a> {
    originals: &'a [(Function, Span)],
    handle_params: &'a [Vec<bool>],
    new: Arena<Function>,
    /// New handle of each original function's (call-rewritten) plain copy.
    plain: Vec<Handle<Function>>,
    specs: HashMap<(usize, Bindings), Handle<Function>>,
}

impl Builder<'_> {
    /// Rewrites the calls of a function or entry point: callees are original handles,
    /// mapped to their plain copy or a specialisation.
    fn rewrite_calls(&mut self, function: &mut Function) {
        let mut body = std::mem::take(&mut function.body);
        self.rewrite_block(&mut body, &mut function.expressions);
        function.body = body;
    }

    fn rewrite_block(&mut self, block: &mut Block, expressions: &mut Arena<Expression>) {
        for statement in block.iter_mut() {
            match statement {
                Statement::Block(inner) => self.rewrite_block(inner, expressions),
                Statement::If { accept, reject, .. } => {
                    self.rewrite_block(accept, expressions);
                    self.rewrite_block(reject, expressions);
                }
                Statement::Switch { cases, .. } => {
                    for case in cases {
                        self.rewrite_block(&mut case.body, expressions);
                    }
                }
                Statement::Loop { body, continuing, .. } => {
                    self.rewrite_block(body, expressions);
                    self.rewrite_block(continuing, expressions);
                }
                Statement::Call {
                    function,
                    arguments,
                    result,
                } => {
                    let callee = function.index();
                    let Some(params) = self.handle_params.get(callee) else {
                        continue;
                    };
                    let bindings: Bindings = arguments
                        .iter()
                        .zip(params)
                        .map(|(&arg, &is_handle)| match expressions[arg] {
                            Expression::GlobalVariable(global) if is_handle => Some(global),
                            _ => None,
                        })
                        .collect();
                    let target = if bindings.iter().any(Option::is_some) {
                        let target = self.specialization(callee, bindings.clone());
                        let mut kept = bindings.iter();
                        arguments.retain(|_| kept.next().is_some_and(Option::is_none));
                        target
                    } else if let Some(&plain) = self.plain.get(callee) {
                        plain
                    } else {
                        continue;
                    };
                    *function = target;
                    if let Some(result) = result {
                        expressions[*result] = Expression::CallResult(target);
                    }
                }
                _ => {}
            }
        }
    }

    /// The copy of original function `index` with `bindings` substituted, built on first use.
    fn specialization(&mut self, index: usize, bindings: Bindings) -> Handle<Function> {
        if let Some(&handle) = self.specs.get(&(index, bindings.clone())) {
            return handle;
        }
        let (original, span) = &self.originals[index];
        let mut function = original.clone();
        // Old argument index → new index (or the bound global).
        let mut new_index = Vec::with_capacity(bindings.len());
        let mut next = 0u32;
        for binding in &bindings {
            new_index.push(next);
            if binding.is_none() {
                next += 1;
            }
        }
        for (_, expression) in function.expressions.iter_mut() {
            if let Expression::FunctionArgument(arg) = *expression {
                let arg = arg as usize;
                *expression = match bindings.get(arg).copied().flatten() {
                    Some(global) => Expression::GlobalVariable(global),
                    None => Expression::FunctionArgument(new_index.get(arg).copied().unwrap_or(0)),
                };
            }
        }
        let mut kept = bindings.iter();
        function
            .arguments
            .retain(|_| kept.next().is_some_and(Option::is_none));
        if let Some(name) = &mut function.name {
            name.push_str(&format!("_spec{}", self.specs.len()));
        }
        self.rewrite_calls(&mut function);
        let handle = self.new.append(function, *span);
        self.specs.insert((index, bindings), handle);
        handle
    }
}

fn collect_calls(block: &Block, calls: &mut Vec<Handle<Function>>) {
    for statement in block.iter() {
        match statement {
            Statement::Block(inner) => collect_calls(inner, calls),
            Statement::If { accept, reject, .. } => {
                collect_calls(accept, calls);
                collect_calls(reject, calls);
            }
            Statement::Switch { cases, .. } => {
                for case in cases {
                    collect_calls(&case.body, calls);
                }
            }
            Statement::Loop { body, continuing, .. } => {
                collect_calls(body, calls);
                collect_calls(continuing, calls);
            }
            Statement::Call { function, .. } => calls.push(*function),
            _ => {}
        }
    }
}

/// Replaces an uncalled function's body with `return <zero>` (keeps its signature).
fn stub(function: &mut Function) {
    let mut expressions = Arena::new();
    let value = function
        .result
        .as_ref()
        .map(|result| expressions.append(Expression::ZeroValue(result.ty), Span::UNDEFINED));
    function.expressions = expressions;
    function.local_variables = Arena::new();
    function.named_expressions = Default::default();
    function.body = Block::from_vec(vec![Statement::Return { value }]);
}

/// Image operations the Mali compiler crashes on (module docs) that still read a
/// function parameter, as `function: operation` lines. Empty after
/// [`specialize_handle_arguments`] for any WGSL-derived module.
pub fn image_parameter_hazards(module: &Module) -> Vec<String> {
    let mut hazards = Vec::new();
    let functions = module
        .functions
        .iter()
        .map(|(_, f)| f)
        .chain(module.entry_points.iter().map(|e| &e.function));
    for function in functions {
        let name = function.name.as_deref().unwrap_or("<unnamed>");
        let is_param = |image: Handle<Expression>| {
            matches!(function.expressions[image], Expression::FunctionArgument(_))
        };
        for (_, expression) in function.expressions.iter() {
            let op = match *expression {
                Expression::ImageLoad { image, .. } if is_param(image) => "textureLoad",
                Expression::ImageQuery {
                    image,
                    query: ImageQuery::NumLevels,
                } if is_param(image) => "textureNumLevels",
                Expression::ImageQuery {
                    image,
                    query: ImageQuery::NumSamples,
                } if is_param(image) => "textureNumSamples",
                _ => continue,
            };
            hazards.push(format!("{name}: {op} on a texture parameter"));
        }
        store_hazards(&function.body, name, &is_param, &mut hazards);
    }
    hazards
}

fn store_hazards(
    block: &Block,
    name: &str,
    is_param: &dyn Fn(Handle<Expression>) -> bool,
    hazards: &mut Vec<String>,
) {
    for statement in block.iter() {
        match statement {
            Statement::Block(inner) => store_hazards(inner, name, is_param, hazards),
            Statement::If { accept, reject, .. } => {
                store_hazards(accept, name, is_param, hazards);
                store_hazards(reject, name, is_param, hazards);
            }
            Statement::Switch { cases, .. } => {
                for case in cases {
                    store_hazards(&case.body, name, is_param, hazards);
                }
            }
            Statement::Loop { body, continuing, .. } => {
                store_hazards(body, name, is_param, hazards);
                store_hazards(continuing, name, is_param, hazards);
            }
            Statement::ImageStore { image, .. } if is_param(*image) => {
                hazards.push(format!("{name}: textureStore on a texture parameter"));
            }
            Statement::ImageAtomic { image, .. } if is_param(*image) => {
                hazards.push(format!("{name}: textureAtomic on a texture parameter"));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Module {
        naga::front::wgsl::parse_str(source).unwrap_or_else(|err| panic!("{}", err.emit_to_string(source)))
    }

    fn validate(module: &Module) {
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(module)
        .unwrap_or_else(|err| panic!("{err:?}"));
    }

    const NESTED: &str = r"
@group(0) @binding(0) var a: texture_2d<f32>;
@group(0) @binding(1) var b: texture_2d<f32>;
@group(0) @binding(2) var s: sampler;
@group(0) @binding(3) var out_tex: texture_storage_2d<rgba8unorm, write>;
// (naga itself rejects `textureStore` through a parameter, so the stores stay in `main`.)
fn load(tex: texture_2d<f32>, c: vec2<i32>) -> vec4<f32> { return textureLoad(tex, c, 0); }
fn levels(tex: texture_2d<f32>) -> u32 { return textureNumLevels(tex); }
fn both(tex: texture_2d<f32>, k: f32, c: vec2<i32>) -> vec4<f32> {
    return load(tex, c) * k + vec4<f32>(f32(levels(tex))) + textureSampleLevel(tex, s, vec2<f32>(0.5), 0.0);
}
fn plain(x: f32) -> f32 { return x * 2.0; }
@compute @workgroup_size(1) fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let c = vec2<i32>(id.xy);
    let v = both(a, plain(1.0), c) + both(b, 3.0, c) + both(a, 4.0, c);
    textureStore(out_tex, c, v);
}
";

    #[test]
    fn specialises_nested_handle_parameters() {
        let mut module = parse(NESTED);
        validate(&module);
        assert!(!image_parameter_hazards(&module).is_empty());
        assert!(specialize_handle_arguments(&mut module));
        validate(&module);
        assert_eq!(image_parameter_hazards(&module), Vec::<String>::new());
        // `both` is specialised once per distinct texture, not per call.
        let specs = module
            .functions
            .iter()
            .filter(|(_, f)| f.name.as_deref().is_some_and(|n| n.starts_with("both_spec")))
            .count();
        assert_eq!(specs, 2);
        // The SPIR-V writer accepts it.
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|err| panic!("{err:?}"));
        naga::back::spv::write_vec(&module, &info, &naga::back::spv::Options::default(), None)
            .unwrap_or_else(|err| panic!("{err}"));
    }

    #[test]
    fn modules_without_handle_parameters_are_untouched() {
        let source = "fn f(x: f32) -> f32 { return x; }\n@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(f(1.0)); }";
        let mut module = parse(source);
        let before = format!("{module:?}");
        assert!(!specialize_handle_arguments(&mut module));
        assert_eq!(format!("{module:?}"), before);
    }

    #[test]
    fn sampling_through_a_parameter_is_not_a_hazard() {
        let module = parse(
            r"
@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
fn f(tex: texture_2d<f32>) -> vec4<f32> { return textureSample(tex, s, vec2<f32>(0.5)) + vec4<f32>(f32(textureDimensions(tex).x)); }
@fragment fn main() -> @location(0) vec4<f32> { return f(t); }
",
        );
        assert!(image_parameter_hazards(&module).is_empty());
    }
}
