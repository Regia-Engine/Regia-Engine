// SPDX-License-Identifier: MPL-2.0
//! What the HLOD atlas bake reads of a material graph or instance (ADR-2713).
//!
//! `regia_matgraph` compiles graphs to WGSL and has no CPU evaluator, so the bake folds each
//! surface output to a constant here: parameters take their defaults (or the instance's values),
//! constants and the math nodes are evaluated, static switches take their branch. What a constant
//! cannot stand for is replaced by a neutral value and named in [`GraphBake::approximated`]:
//! texture samples read white, UVs and positions read zero, the normal reads up, time and wind
//! read zero, view-dependent nodes read as seen head-on, and function calls and noise read their
//! neutral value. `VertexColor` reads 1 and, when the base color or alpha reads it, the bake
//! multiplies the mesh's vertex colors in, which is exact when the output is a product of them.
//! A graph therefore bakes its parameter-driven color, roughness, metallic, occlusion and constant
//! tangent-space normal, not its texture detail.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use regia_matgraph::{GraphAlphaMode, MaterialGraph, MaterialInstance, Node, NormalSpace, ParamValue};

use super::{BakeImage, BakeMaterial};

/// Deepest node chain the fold follows (a graph is acyclic; this bounds a malformed one).
const MAX_DEPTH: u32 = 64;

/// A folded value: up to four components and the width the graph types it with.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Value {
    v: [f32; 4],
    width: usize,
}

impl Value {
    fn new(v: [f32; 4], width: usize) -> Self {
        Self { v, width }
    }

    fn splat(x: f32) -> Self {
        Self::new([x; 4], 1)
    }

    /// Component `i`, a `Float` broadcast to any width.
    fn at(&self, i: usize) -> f32 {
        if self.width == 1 {
            self.v[0]
        } else {
            self.v[i.min(3)]
        }
    }

    fn map(self, f: impl Fn(f32) -> f32) -> Self {
        Self::new(self.v.map(f), self.width)
    }

    fn zip(a: Self, b: Self, f: impl Fn(f32, f32) -> f32) -> Self {
        let width = a.width.max(b.width);
        Self::new(std::array::from_fn(|i| f(a.at(i), b.at(i))), width)
    }

    fn finite(self) -> Self {
        self.map(|x| if x.is_finite() { x } else { 0.0 })
    }
}

/// What the bake takes from a graph: its material and the nodes the fold replaced.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphBake {
    /// The factors and constant maps the bake samples.
    pub material: BakeMaterial,
    /// Names of the nodes read as a neutral value (sorted, no repeats): the bake's error there.
    pub approximated: Vec<String>,
}

struct Fold<'a> {
    graph: &'a MaterialGraph,
    params: BTreeMap<&'a str, ParamValue>,
    switches: BTreeMap<&'a str, bool>,
    memo: BTreeMap<String, Value>,
    approximated: Vec<String>,
    vertex_color: bool,
}

impl Fold<'_> {
    fn neutral(&mut self, name: &str, value: Value) -> Value {
        self.approximated.push(name.to_owned());
        value
    }

    fn eval(&mut self, name: &str, depth: u32) -> Value {
        if let Some(v) = self.memo.get(name) {
            return *v;
        }
        let graph = self.graph;
        let value = match graph.nodes.get(name) {
            Some(node) if depth < MAX_DEPTH => self.node(name, node, depth + 1),
            _ => self.neutral(name, Value::splat(0.0)),
        }
        .finite();
        self.memo.insert(name.to_owned(), value);
        value
    }

    fn node(&mut self, name: &str, node: &Node, depth: u32) -> Value {
        let e = |this: &mut Self, input: &str| this.eval(input, depth);
        match node {
            Node::Float(x) => Value::splat(*x),
            Node::Vec2([x, y]) => Value::new([*x, *y, 0.0, 0.0], 2),
            Node::Vec3([x, y, z]) => Value::new([*x, *y, *z, 0.0], 3),
            Node::Vec4(v) => Value::new(*v, 4),
            Node::Param(p) => match self.params.get(p.as_str()) {
                Some(value) => {
                    let width = match value {
                        ParamValue::Float(_) => 1,
                        ParamValue::Vec2(_) => 2,
                        ParamValue::Vec3(_) | ParamValue::Color(_) => 3,
                        ParamValue::Vec4(_) => 4,
                    };
                    Value::new(value.to_vec4(), width)
                }
                None => self.neutral(name, Value::splat(0.0)),
            },
            Node::VertexColor => {
                self.vertex_color = true;
                Value::new([1.0; 4], 4)
            }
            Node::Add(a, b) => {
                let (a, b) = (e(self, a), e(self, b));
                Value::zip(a, b, |x, y| x + y)
            }
            Node::Sub(a, b) => {
                let (a, b) = (e(self, a), e(self, b));
                Value::zip(a, b, |x, y| x - y)
            }
            Node::Mul(a, b) => {
                let (a, b) = (e(self, a), e(self, b));
                Value::zip(a, b, |x, y| x * y)
            }
            Node::Div(a, b) => {
                let (a, b) = (e(self, a), e(self, b));
                Value::zip(a, b, |x, y| if y == 0.0 { 0.0 } else { x / y })
            }
            Node::Min(a, b) => {
                let (a, b) = (e(self, a), e(self, b));
                Value::zip(a, b, f32::min)
            }
            Node::Max(a, b) => {
                let (a, b) = (e(self, a), e(self, b));
                Value::zip(a, b, f32::max)
            }
            Node::Pow(a, b) => {
                let (a, b) = (e(self, a), e(self, b));
                Value::zip(a, b, f32::powf)
            }
            Node::Dot(a, b) => {
                let (a, b) = (e(self, a), e(self, b));
                let width = a.width.max(b.width);
                Value::splat((0..width).map(|i| a.at(i) * b.at(i)).sum())
            }
            Node::Lerp { a, b, t } => {
                let (a, b, t) = (e(self, a), e(self, b), e(self, t));
                let width = a.width.max(b.width);
                Value::new(
                    std::array::from_fn(|i| a.at(i) + (b.at(i) - a.at(i)) * t.at(i)),
                    width,
                )
            }
            Node::Smoothstep { edge0, edge1, x } => {
                let (e0, e1, x) = (e(self, edge0), e(self, edge1), e(self, x));
                let width = x.width.max(e0.width).max(e1.width);
                Value::new(
                    std::array::from_fn(|i| {
                        let span = e1.at(i) - e0.at(i);
                        if span <= 0.0 {
                            return if x.at(i) < e0.at(i) { 0.0 } else { 1.0 };
                        }
                        let t = ((x.at(i) - e0.at(i)) / span).clamp(0.0, 1.0);
                        t * t * (3.0 - 2.0 * t)
                    }),
                    width,
                )
            }
            Node::Saturate(x) => e(self, x).map(|v| v.clamp(0.0, 1.0)),
            Node::OneMinus(x) => e(self, x).map(|v| 1.0 - v),
            Node::Abs(x) => e(self, x).map(f32::abs),
            Node::Sin(x) => e(self, x).map(f32::sin),
            Node::Cos(x) => e(self, x).map(f32::cos),
            Node::Frac(x) => e(self, x).map(|v| v - v.floor()),
            Node::Normalize(x) => {
                let x = e(self, x);
                let len = (0..x.width).map(|i| x.v[i] * x.v[i]).sum::<f32>().sqrt();
                if len > 0.0 { x.map(|v| v / len) } else { x }
            }
            Node::Length(x) => {
                let x = e(self, x);
                Value::splat((0..x.width).map(|i| x.v[i] * x.v[i]).sum::<f32>().sqrt())
            }
            Node::Swizzle { input, channels } => {
                let x = e(self, input);
                let mut out = [0.0; 4];
                let mut width = 0;
                for (slot, c) in out.iter_mut().zip(channels.chars()) {
                    let i = match c {
                        'r' | 'x' => 0,
                        'g' | 'y' => 1,
                        'b' | 'z' => 2,
                        'a' | 'w' => 3,
                        _ => return self.neutral(name, Value::splat(0.0)),
                    };
                    *slot = x.at(i);
                    width += 1;
                }
                Value::new(out, width.max(1))
            }
            Node::Append(a, b) => {
                let (a, b) = (e(self, a), e(self, b));
                let mut out = [0.0; 4];
                let mut width = 0;
                for &x in a.v.iter().take(a.width) {
                    if width < 4 {
                        out[width] = x;
                        width += 1;
                    }
                }
                for &x in b.v.iter().take(b.width) {
                    if width < 4 {
                        out[width] = x;
                        width += 1;
                    }
                }
                Value::new(out, width.max(1))
            }
            Node::UnpackNormal(x) => {
                let x = e(self, x);
                Value::new(std::array::from_fn(|i| x.at(i) * 2.0 - 1.0), 3)
            }
            Node::NormalStrength { normal, strength } => {
                let (n, s) = (e(self, normal), e(self, strength));
                Value::new([n.at(0) * s.at(0), n.at(1) * s.at(0), n.at(2), 0.0], 3)
            }
            Node::Switch { switch, on, off } => {
                let pick = self.switches.get(switch.as_str()).copied().unwrap_or(false);
                e(self, if pick { on } else { off })
            }
            Node::Sample { .. } | Node::Triplanar { .. } => self.neutral(name, Value::new([1.0; 4], 4)),
            Node::TexCoord | Node::TexCoord1 | Node::Wind => self.neutral(name, Value::new([0.0; 4], 2)),
            Node::UvTransform { uv, .. } | Node::Parallax { uv, .. } => {
                let uv = e(self, uv);
                self.neutral(name, uv)
            }
            Node::WorldPosition | Node::ObjectPosition => self.neutral(name, Value::new([0.0; 4], 3)),
            Node::WorldNormal | Node::ViewDirection => {
                self.neutral(name, Value::new([0.0, 1.0, 0.0, 0.0], 3))
            }
            Node::CameraDistance | Node::Time => self.neutral(name, Value::splat(0.0)),
            // Head-on: N·V = 1.
            Node::Fresnel { .. } => self.neutral(name, Value::splat(0.0)),
            Node::BlendNormals { base, .. } => {
                let base = e(self, base);
                self.neutral(name, base)
            }
            Node::Noise { .. } => self.neutral(name, Value::splat(0.5)),
            // WGSL is not evaluated on the CPU: a Code node bakes as mid grey (ADR-7852).
            Node::Code { .. } => self.neutral(name, Value::splat(0.5)),
            Node::HeightBlend { a, b, t, .. } => {
                let (a, b, t) = (e(self, a), e(self, b), e(self, t));
                let width = a.width.max(b.width);
                let mixed = Value::new(
                    std::array::from_fn(|i| a.at(i) + (b.at(i) - a.at(i)) * t.at(0).clamp(0.0, 1.0)),
                    width,
                );
                self.neutral(name, mixed)
            }
            Node::Call { .. } | Node::Input(_) => self.neutral(name, Value::splat(0.0)),
        }
    }

    /// The output node `name`, or `None` when the output is unset.
    fn output(&mut self, name: &Option<String>) -> Option<Value> {
        name.as_ref().map(|n| self.eval(n, 0))
    }
}

/// A 1x1 linear image.
fn constant_image(rgb: [f32; 3]) -> Arc<BakeImage> {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Arc::new(BakeImage {
        width: 1,
        height: 1,
        rgba: vec![byte(rgb[0]), byte(rgb[1]), byte(rgb[2]), 255],
    })
}

/// Folds `graph` (with `instance`'s parameter and switch values when given) into what the atlas
/// bake samples. See the module docs for what is exact and what is approximated.
pub fn bake_material_from_graph(graph: &MaterialGraph, instance: Option<&MaterialInstance>) -> GraphBake {
    let _span = tracing::debug_span!("hlod_atlas_graph_fold", nodes = graph.nodes.len()).entered();
    let mut params: BTreeMap<&str, ParamValue> =
        graph.params.iter().map(|p| (p.name.as_str(), p.value)).collect();
    let mut switches: BTreeMap<&str, bool> = graph
        .switches
        .iter()
        .map(|s| (s.name.as_str(), s.default))
        .collect();
    if let Some(instance) = instance {
        for (name, value) in &instance.params {
            if params.contains_key(name.as_str()) {
                params.insert(name.as_str(), *value);
            }
        }
        for (name, on) in &instance.switches {
            switches.insert(name.as_str(), *on);
        }
    }
    let mut fold = Fold {
        graph,
        params,
        switches,
        memo: BTreeMap::new(),
        approximated: Vec::new(),
        vertex_color: false,
    };
    let out = &graph.output;
    let base = fold.output(&out.base_color);
    let alpha = fold.output(&out.alpha);
    // Vertex colors only count where the color itself reads them.
    let vertex_color = fold.vertex_color;
    let metallic = fold.output(&out.metallic);
    let roughness = fold.output(&out.roughness);
    let occlusion = fold.output(&out.occlusion);
    let normal = match out.normal_space {
        NormalSpace::Tangent => fold.output(&out.normal),
        NormalSpace::World => None,
    };
    let rgb = base.map_or([0.8; 3], |b| [b.at(0), b.at(1), b.at(2)]);
    let mut material = BakeMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], alpha.map_or(1.0, |a| a.at(0))],
        metallic: metallic.map_or(0.0, |m| m.at(0)).clamp(0.0, 1.0),
        roughness: roughness.map_or(0.5, |r| r.at(0)).clamp(0.0, 1.0),
        vertex_color,
        ..BakeMaterial::default()
    };
    if let Some(ao) = occlusion {
        // The occlusion rides a 1x1 packed map whose roughness and metallic scale by 1.
        material.orm_map = Some(constant_image([ao.at(0), 1.0, 1.0]));
        material.orm_packed = true;
    }
    if let Some(n) = normal {
        let len = (n.at(0) * n.at(0) + n.at(1) * n.at(1) + n.at(2) * n.at(2)).sqrt();
        if len > 0.0 {
            let unit = [n.at(0) / len, n.at(1) / len, n.at(2) / len];
            material.normal_map = Some(constant_image(unit.map(|c| c * 0.5 + 0.5)));
        }
    }
    if let GraphAlphaMode::Mask(cutoff) = graph.shading.alpha_mode {
        material.alpha_cutoff = Some(cutoff);
    }
    let mut approximated = fold.approximated;
    approximated.sort();
    approximated.dedup();
    GraphBake {
        material,
        approximated,
    }
}

/// The bake material of the graph (`.matgraph.ron`) or instance (`.matinst.ron`) at the
/// project-relative `path` under `project_root`, or `None` when it does not read or parse (the
/// bake then uses vertex colors, as for a mesh without a material).
pub fn read_graph_bake_material(project_root: &Path, path: &str) -> Option<BakeMaterial> {
    let read = |p: &str| {
        crate::cook::join_in_project(project_root, p)
            .ok()
            .and_then(|full| std::fs::read_to_string(full).ok())
    };
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".matinst.ron") {
        let instance = MaterialInstance::from_ron(&read(path)?).ok()?;
        let graph = MaterialGraph::from_ron(&read(&instance.parent)?).ok()?;
        Some(bake_material_from_graph(&graph, Some(&instance)).material)
    } else if lower.ends_with(".matgraph.ron") {
        let graph = MaterialGraph::from_ron(&read(path)?).ok()?;
        Some(bake_material_from_graph(&graph, None).material)
    } else {
        None
    }
}
