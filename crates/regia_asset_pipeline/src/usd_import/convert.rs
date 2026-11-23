// SPDX-License-Identifier: MPL-2.0
//! The composed USD stage to a glTF scene ([`convert`]).
//!
//! Schemas and their attributes follow the public USD documentation (openusd.org): UsdGeom
//! (`Xformable` transform operations and `xformOpOrder`, `Imageable` visibility and purpose,
//! `Mesh`, `Primvars` interpolation and indexed primvars, `GeomSubset`, `Camera`, stage
//! `metersPerUnit` and `upAxis`), UsdShade (`MaterialBindingAPI`, connections, `Material`
//! outputs), the UsdPreviewSurface specification (`UsdPreviewSurface`, `UsdUVTexture`,
//! `UsdPrimvarReader_float2`) and UsdLux.

use std::collections::HashMap;

use openusd::{sdf, tf, usd};
use regia_core::math::{Mat4, Quat, Vec3};

use super::vfs::{LayerSet, display};
use super::{UsdError, UsdImportReport, UsdImportSettings};
use crate::gltf_write::{
    GlbCamera, GlbExtras, GlbImage, GlbLight, GlbLightKind, GlbMaterial, GlbMesh, GlbNode, GlbPrimitive,
    GlbProjection, GlbScene, GlbTextureRef,
};

// ---------------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------------

/// The composed value of `prim.name`: its default, else its earliest time sample.
fn value(prim: &usd::Prim, name: &str) -> Option<sdf::Value> {
    let attr = prim.attribute(name);
    if let Ok(Some(v)) = attr.get::<sdf::Value>() {
        return Some(v);
    }
    let first = attr.time_sample_times().ok()?.first().copied()?;
    attr.get_at::<sdf::Value>(usd::TimeCode::new(first))
        .ok()
        .flatten()
}

fn f64_of(v: &sdf::Value) -> Option<f64> {
    let out = match v {
        sdf::Value::Float(f) => f64::from(*f),
        sdf::Value::Double(d) => *d,
        sdf::Value::Half(h) => f64::from(h.to_f32()),
        sdf::Value::Int(i) => f64::from(*i),
        _ => return None,
    };
    out.is_finite().then_some(out)
}

fn vec2_of(v: &sdf::Value) -> Option<[f64; 2]> {
    let out = match v {
        sdf::Value::Vec2f(a) => [f64::from(a.x), f64::from(a.y)],
        sdf::Value::Vec2d(a) => [a.x, a.y],
        sdf::Value::Vec2h(a) => [f64::from(a.x.to_f32()), f64::from(a.y.to_f32())],
        _ => return None,
    };
    out.iter().all(|c| c.is_finite()).then_some(out)
}

fn vec3_of(v: &sdf::Value) -> Option<[f64; 3]> {
    let out = match v {
        sdf::Value::Vec3f(a) => [f64::from(a.x), f64::from(a.y), f64::from(a.z)],
        sdf::Value::Vec3d(a) => [a.x, a.y, a.z],
        sdf::Value::Vec3h(a) => [
            f64::from(a.x.to_f32()),
            f64::from(a.y.to_f32()),
            f64::from(a.z.to_f32()),
        ],
        _ => return None,
    };
    out.iter().all(|c| c.is_finite()).then_some(out)
}

/// A quaternion as `[x, y, z, w]`.
fn quat_of(v: &sdf::Value) -> Option<[f64; 4]> {
    let out = match v {
        sdf::Value::Quatf(q) => [q.x, q.y, q.z, q.w].map(f64::from),
        sdf::Value::Quatd(q) => [q.x, q.y, q.z, q.w],
        sdf::Value::Quath(q) => [q.x, q.y, q.z, q.w].map(|c| f64::from(c.to_f32())),
        _ => return None,
    };
    out.iter().all(|c| c.is_finite()).then_some(out)
}

fn vec2s_of(v: &sdf::Value) -> Option<Vec<[f64; 2]>> {
    Some(match v {
        sdf::Value::Vec2fVec(a) => a.iter().map(|p| [f64::from(p.x), f64::from(p.y)]).collect(),
        sdf::Value::Vec2dVec(a) => a.iter().map(|p| [p.x, p.y]).collect(),
        sdf::Value::Vec2hVec(a) => a
            .iter()
            .map(|p| [f64::from(p.x.to_f32()), f64::from(p.y.to_f32())])
            .collect(),
        _ => return None,
    })
}

fn vec3s_of(v: &sdf::Value) -> Option<Vec<[f64; 3]>> {
    Some(match v {
        sdf::Value::Vec3fVec(a) => a
            .iter()
            .map(|p| [f64::from(p.x), f64::from(p.y), f64::from(p.z)])
            .collect(),
        sdf::Value::Vec3dVec(a) => a.iter().map(|p| [p.x, p.y, p.z]).collect(),
        sdf::Value::Vec3hVec(a) => a
            .iter()
            .map(|p| {
                [
                    f64::from(p.x.to_f32()),
                    f64::from(p.y.to_f32()),
                    f64::from(p.z.to_f32()),
                ]
            })
            .collect(),
        _ => return None,
    })
}

fn floats_of(v: &sdf::Value) -> Option<Vec<f64>> {
    Some(match v {
        sdf::Value::FloatVec(a) => a.iter().map(|f| f64::from(*f)).collect(),
        sdf::Value::DoubleVec(a) => a.clone(),
        sdf::Value::HalfVec(a) => a.iter().map(|h| f64::from(h.to_f32())).collect(),
        _ => return None,
    })
}

fn ints_of(v: &sdf::Value) -> Option<Vec<i64>> {
    Some(match v {
        sdf::Value::IntVec(a) => a.iter().map(|i| i64::from(*i)).collect(),
        sdf::Value::UintVec(a) => a.iter().map(|i| i64::from(*i)).collect(),
        sdf::Value::Int64Vec(a) => a.clone(),
        _ => return None,
    })
}

fn token_of(v: &sdf::Value) -> Option<String> {
    match v {
        sdf::Value::Token(t) => Some(t.as_str().to_string()),
        sdf::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn tokens_of(v: &sdf::Value) -> Option<Vec<String>> {
    match v {
        sdf::Value::TokenVec(t) => Some(t.iter().map(|t| t.as_str().to_string()).collect()),
        sdf::Value::StringVec(s) => Some(s.clone()),
        _ => None,
    }
}

fn token_attr(prim: &usd::Prim, name: &str) -> Option<String> {
    value(prim, name).as_ref().and_then(token_of)
}

fn f64_attr(prim: &usd::Prim, name: &str) -> Option<f64> {
    value(prim, name).as_ref().and_then(f64_of)
}

fn type_name(prim: &usd::Prim) -> String {
    prim.type_name()
        .ok()
        .flatten()
        .map(|t| t.as_str().to_string())
        .unwrap_or_default()
}

fn prim_name(prim: &usd::Prim) -> String {
    prim.path().name().unwrap_or("").to_string()
}

/// A UsdLux input: `inputs:<name>`, else the pre-2021 unprefixed `<name>`.
fn lux(prim: &usd::Prim, name: &str) -> Option<sdf::Value> {
    value(prim, &format!("inputs:{name}")).or_else(|| value(prim, name))
}

// ---------------------------------------------------------------------------------------------
// Transforms
// ---------------------------------------------------------------------------------------------

fn axis_rotation(axis: char, degrees: f64) -> Mat4 {
    let r = (degrees as f32).to_radians();
    match axis {
        'X' => Mat4::from_rotation_x(r),
        'Y' => Mat4::from_rotation_y(r),
        _ => Mat4::from_rotation_z(r),
    }
}

/// One transform operation as a column-vector matrix (UsdGeomXformOp).
fn op_matrix(kind: &str, v: &sdf::Value) -> Option<Mat4> {
    let as_vec3 = |v: &sdf::Value| vec3_of(v).map(|a| Vec3::new(a[0] as f32, a[1] as f32, a[2] as f32));
    Some(match kind {
        "translate" => Mat4::from_translation(as_vec3(v)?),
        "scale" => Mat4::from_scale(as_vec3(v).or_else(|| f64_of(v).map(|s| Vec3::splat(s as f32)))?),
        "rotateX" | "rotateY" | "rotateZ" => axis_rotation(kind.chars().last()?, f64_of(v)?),
        "orient" => {
            let q = quat_of(v)?;
            Mat4::from_quat(Quat::from_xyzw(q[0] as f32, q[1] as f32, q[2] as f32, q[3] as f32).normalize())
        }
        "transform" => match v {
            // Row-major with row vectors: its flat array is the column-major array of the
            // column-vector matrix.
            sdf::Value::Matrix4d(m) => {
                let cols = m.0.map(|c| c as f32);
                if cols.iter().all(|c| c.is_finite()) {
                    Mat4::from_cols_array(&cols)
                } else {
                    return None;
                }
            }
            _ => return None,
        },
        k if k.len() == 9 && k.starts_with("rotate") => {
            // rotateABC: angles (a, b, c) in degrees, A applied first.
            let angles = vec3_of(v)?;
            let axes: Vec<char> = k[6..].chars().collect();
            let mut m = Mat4::IDENTITY;
            for (axis, angle) in axes.iter().zip(angles) {
                if !matches!(axis, 'X' | 'Y' | 'Z') {
                    return None;
                }
                m = axis_rotation(*axis, angle) * m;
            }
            m
        }
        _ => return None,
    })
}

/// The prim's local transform from `xformOpOrder`, and whether it resets the parent stack.
fn local_transform(prim: &usd::Prim, warnings: &mut Vec<String>) -> (Mat4, bool) {
    let Some(order) = value(prim, "xformOpOrder").as_ref().and_then(tokens_of) else {
        return (Mat4::IDENTITY, false);
    };
    let mut m = Mat4::IDENTITY;
    let mut reset = false;
    for entry in order {
        if entry == "!resetXformStack!" {
            reset = true;
            m = Mat4::IDENTITY;
            continue;
        }
        let (invert, name) = match entry.strip_prefix("!invert!") {
            Some(name) => (true, name.to_string()),
            None => (false, entry.clone()),
        };
        let kind = name
            .strip_prefix("xformOp:")
            .and_then(|rest| rest.split(':').next())
            .unwrap_or("");
        let op = value(prim, &name).as_ref().and_then(|v| op_matrix(kind, v));
        match op {
            Some(op) => {
                let op = if invert { op.inverse() } else { op };
                if op.is_finite() {
                    m *= op;
                }
            }
            None => push_warning(
                warnings,
                format!(
                    "{}: transform op {name} unreadable; skipped",
                    prim.path().as_str()
                ),
            ),
        }
    }
    (m, reset)
}

fn push_warning(warnings: &mut Vec<String>, message: String) {
    if warnings.len() < 256 {
        warnings.push(message);
    }
}

// ---------------------------------------------------------------------------------------------
// Conversion state
// ---------------------------------------------------------------------------------------------

/// The basis change from stage space to glTF space: uniform scale times the up-axis rotation.
#[derive(Clone, Copy)]
struct Basis {
    rotation: Mat4,
    scale: f64,
}

impl Basis {
    fn matrix(&self) -> Mat4 {
        Mat4::from_scale(Vec3::splat(self.scale as f32)) * self.rotation
    }

    fn point(&self, p: [f64; 3]) -> [f32; 3] {
        let v = self
            .rotation
            .transform_point3(Vec3::new(p[0] as f32, p[1] as f32, p[2] as f32))
            * self.scale as f32;
        [v.x, v.y, v.z]
    }

    fn normal(&self, n: [f64; 3]) -> [f32; 3] {
        let v = self
            .rotation
            .transform_vector3(Vec3::new(n[0] as f32, n[1] as f32, n[2] as f32))
            .normalize_or_zero();
        [v.x, v.y, v.z]
    }

    /// `M` in stage space as a node transform in glTF space: `C M C⁻¹`.
    fn node(&self, m: Mat4) -> Mat4 {
        let c = self.matrix();
        c * m * c.inverse()
    }
}

/// A texture input: the image, the channel it reads, and the UV set name.
#[derive(Clone, Debug, PartialEq)]
struct TextureInput {
    id: String,
    channel: String,
    uv_set: String,
}

/// A preview surface input: a constant, a texture, or nothing authored.
#[derive(Clone, Debug, Default, PartialEq)]
struct Input {
    value: Option<sdf::Value>,
    texture: Option<TextureInput>,
}

/// The parts of a material the converter reads.
#[derive(Clone, Debug, Default)]
struct Surface {
    name: String,
    diffuse: Input,
    emissive: Input,
    metallic: Input,
    roughness: Input,
    opacity: Input,
    opacity_threshold: Option<f64>,
    normal: Input,
    occlusion: Input,
}

struct Converter<'a> {
    set: &'a LayerSet,
    stage: usd::Stage,
    settings: &'a UsdImportSettings,
    basis: Basis,
    scene: GlbScene,
    extras: GlbExtras,
    report: UsdImportReport,
    warnings: Vec<String>,
    /// Material prim path to its parsed surface (or `None`: not a preview surface).
    surfaces: HashMap<String, Option<Surface>>,
    /// `(material path, UV set names, double-sided)` to glTF material index.
    materials: HashMap<(String, Vec<String>, bool), usize>,
    /// Display color (as bits) to a default material.
    color_materials: HashMap<[u32; 4], usize>,
    /// Texture identifier to glTF image index (`None`: unreadable).
    images: HashMap<String, Option<usize>>,
    /// Packed images by their sources.
    packed: HashMap<Vec<(String, String)>, Option<usize>>,
    /// `(prototype mesh path, inherited binding)` to mesh index (instancing).
    instanced: HashMap<(String, String), usize>,
    total_vertices: usize,
    total_indices: usize,
}

/// Composes the walked stage and converts it to GLB bytes and a report (layers, format and the
/// walk's warnings are filled in by the caller).
pub(super) fn convert(
    set: &LayerSet,
    settings: &UsdImportSettings,
) -> Result<(Vec<u8>, UsdImportReport), UsdError> {
    let stage = usd::Stage::builder()
        .resolver(set.resolver())
        .open(&set.root_id)
        .map_err(|e| UsdError::Parse(e.to_string()))?;
    let mut warnings = Vec::new();
    for diagnostic in stage.composition_errors() {
        if matches!(diagnostic, openusd::pcp::CompositionDiagnostic::ArcCycle(_)) {
            return Err(UsdError::ReferenceCycle(diagnostic.to_string()));
        }
        push_warning(&mut warnings, diagnostic.to_string());
    }
    let meters_per_unit = settings
        .meters_per_unit
        .or_else(|| {
            stage
                .stage_metadata("metersPerUnit")
                .ok()
                .flatten()
                .as_ref()
                .and_then(f64_of)
        })
        .filter(|m| m.is_finite() && *m > 0.0)
        .unwrap_or(0.01);
    let up_axis = settings
        .up_axis
        .clone()
        .or_else(|| {
            stage
                .stage_metadata("upAxis")
                .ok()
                .flatten()
                .as_ref()
                .and_then(token_of)
        })
        .unwrap_or_else(|| "Y".into());
    let up_axis = if up_axis == "Z" { "Z" } else { "Y" }.to_string();
    let rotation = if up_axis == "Z" {
        Mat4::from_rotation_x(-std::f32::consts::FRAC_PI_2)
    } else {
        Mat4::IDENTITY
    };
    let scale = meters_per_unit * f64::from(settings.scale);
    let mut converter = Converter {
        set,
        stage,
        settings,
        basis: Basis { rotation, scale },
        scene: GlbScene {
            generator: "Regia Engine USD import".into(),
            ..GlbScene::default()
        },
        extras: GlbExtras::default(),
        report: UsdImportReport {
            meters_per_unit,
            up_axis,
            scale,
            ..UsdImportReport::default()
        },
        warnings,
        surfaces: HashMap::new(),
        materials: HashMap::new(),
        color_materials: HashMap::new(),
        images: HashMap::new(),
        packed: HashMap::new(),
        instanced: HashMap::new(),
        total_vertices: 0,
        total_indices: 0,
    };
    converter.walk()?;
    if converter.report.meshes == 0 {
        return Err(UsdError::NoMesh);
    }
    let Converter {
        scene,
        extras,
        mut report,
        warnings,
        ..
    } = converter;
    report.nodes = scene.nodes.len();
    report.materials = scene.materials.len();
    report.warnings = warnings;
    let glb = scene
        .to_glb_with(&extras)
        .map_err(|err| UsdError::Parse(err.to_string()))?;
    Ok((glb, report))
}

/// One prim waiting in the walk.
struct Pending {
    path: sdf::Path,
    parent: Option<usize>,
    depth: usize,
    /// The nearest ancestor's material binding (UsdShade binding inheritance).
    binding: String,
}

impl Converter<'_> {
    fn warn(&mut self, message: String) {
        push_warning(&mut self.warnings, message);
    }

    fn walk(&mut self) -> Result<(), UsdError> {
        let roots = self
            .stage
            .root_prims()
            .map_err(|e| UsdError::Parse(e.to_string()))?;
        let mut stack: Vec<Pending> = roots
            .iter()
            .rev()
            .filter_map(|name| sdf::Path::abs_root().append_path(name.as_str()).ok())
            .map(|path| Pending {
                path,
                parent: None,
                depth: 1,
                binding: String::new(),
            })
            .collect();
        while let Some(pending) = stack.pop() {
            self.report.prims += 1;
            if self.report.prims > self.settings.limits.max_prims {
                return Err(UsdError::Limit(format!(
                    "more than {} prims",
                    self.settings.limits.max_prims
                )));
            }
            if pending.depth > self.settings.limits.max_hierarchy_depth {
                return Err(UsdError::Limit(format!(
                    "prims nest deeper than {}",
                    self.settings.limits.max_hierarchy_depth
                )));
            }
            let prim = self
                .stage
                .prim(pending.path.clone())
                .map_err(|e| UsdError::Parse(e.to_string()))?;
            if !self.visible(&prim) {
                continue;
            }
            let kind = type_name(&prim);
            let binding = self.binding(&prim).unwrap_or(pending.binding);
            if let Ok(selections) = prim.prim_index().variant_selections() {
                for (set, selection) in selections {
                    if self.report.variant_selections.len() < 4096 {
                        self.report
                            .variant_selections
                            .push(format!("{}: {set} = {selection}", prim.path().as_str()));
                    }
                }
            }
            match kind.as_str() {
                "Material" | "Shader" | "NodeGraph" | "GeomSubset" | "Skeleton" | "SkelAnimation"
                | "BlendShape" => continue,
                "" | "Xform" | "Scope" | "SkelRoot" | "Mesh" | "Camera" | "DistantLight" | "SphereLight"
                | "DiskLight" | "RectLight" => {}
                other => {
                    self.warn(format!(
                        "{}: prim type {other} is not imported (subtree skipped)",
                        prim.path().as_str()
                    ));
                    continue;
                }
            }
            if kind == "SkelRoot" {
                self.warn(format!(
                    "{}: skinning (UsdSkel) is not imported; its meshes come in rigid",
                    prim.path().as_str()
                ));
            }
            let node = self.node(&prim, pending.parent)?;
            match kind.as_str() {
                "Mesh" => self.mesh(&prim, node, &binding)?,
                "Camera" if self.settings.import_cameras => self.camera(&prim, node),
                "DistantLight" | "SphereLight" | "DiskLight" | "RectLight" if self.settings.import_lights => {
                    self.light(&prim, &kind, node);
                }
                _ => {}
            }
            let children = prim.children().map_err(|e| UsdError::Parse(e.to_string()))?;
            for child in children.into_iter().rev() {
                stack.push(Pending {
                    path: child.path().clone(),
                    parent: Some(node),
                    depth: pending.depth + 1,
                    binding: binding.clone(),
                });
            }
        }
        Ok(())
    }

    /// Whether the prim is active, defined, not abstract, visible and of default or render
    /// purpose.
    fn visible(&self, prim: &usd::Prim) -> bool {
        if !prim.is_active().unwrap_or(false)
            || !prim.is_defined().unwrap_or(false)
            || prim.is_abstract().unwrap_or(true)
        {
            return false;
        }
        if token_attr(prim, "visibility").as_deref() == Some("invisible") {
            return false;
        }
        !matches!(token_attr(prim, "purpose").as_deref(), Some("guide" | "proxy"))
    }

    /// The prim's direct material binding (preview purpose first), as a material path.
    fn binding(&self, prim: &usd::Prim) -> Option<String> {
        for name in ["material:binding:preview", "material:binding"] {
            let targets = prim.relationship(name).targets().ok()?;
            if let Some(target) = targets.first() {
                return Some(target.as_str().to_string());
            }
        }
        None
    }

    /// Adds the prim's node under `parent` and returns its index.
    fn node(&mut self, prim: &usd::Prim, parent: Option<usize>) -> Result<usize, UsdError> {
        let mut warnings = Vec::new();
        let (local, reset) = local_transform(prim, &mut warnings);
        for w in warnings {
            self.warn(w);
        }
        if reset && parent.is_some() {
            self.warn(format!(
                "{}: !resetXformStack! is read as a plain transform under its parent",
                prim.path().as_str()
            ));
        }
        let (scale, rotation, translation) = self.basis.node(local).to_scale_rotation_translation();
        let finite =
            |v: Vec3, fallback: f32| [v.x, v.y, v.z].map(|c| if c.is_finite() { c } else { fallback });
        let rotation = if rotation.is_finite() {
            rotation.normalize()
        } else {
            Quat::IDENTITY
        };
        self.scene.nodes.push(GlbNode {
            name: prim_name(prim),
            translation: finite(translation, 0.0),
            rotation: [rotation.x, rotation.y, rotation.z, rotation.w],
            scale: finite(scale, 1.0),
            ..GlbNode::default()
        });
        let index = self.scene.nodes.len() - 1;
        match parent {
            Some(parent) => self.scene.nodes[parent].children.push(index),
            None => self.scene.roots.push(index),
        }
        Ok(index)
    }

    /// The node a camera or light hangs from: the prim's node for a +Y-up stage; for +Z-up a
    /// child turning the stage's local frame into glTF's (both look down local -Z).
    fn axis_node(&mut self, node: usize) -> usize {
        if self.report.up_axis != "Z" {
            return node;
        }
        let q = Quat::from_mat4(&self.basis.rotation);
        let name = format!("{}_axis", self.scene.nodes[node].name);
        self.scene.nodes.push(GlbNode {
            name,
            rotation: [q.x, q.y, q.z, q.w],
            ..GlbNode::default()
        });
        let index = self.scene.nodes.len() - 1;
        self.scene.nodes[node].children.push(index);
        index
    }

    fn camera(&mut self, prim: &usd::Prim, node: usize) {
        let scale = self.basis.scale as f32;
        let clip = value(prim, "clippingRange")
            .as_ref()
            .and_then(vec2_of)
            .unwrap_or([1.0, 1_000_000.0]);
        let znear = (clip[0] as f32 * scale).max(1e-6);
        let zfar = (clip[1] as f32 * scale).max(znear * 2.0);
        let vertical = f64_attr(prim, "verticalAperture").unwrap_or(15.2908);
        let horizontal = f64_attr(prim, "horizontalAperture").unwrap_or(20.955);
        let projection = if token_attr(prim, "projection").as_deref() == Some("orthographic") {
            // Orthographic apertures are in tenths of a scene unit.
            GlbProjection::Orthographic {
                xmag: (horizontal * 0.05) as f32 * scale,
                ymag: (vertical * 0.05) as f32 * scale,
                znear,
                zfar,
            }
        } else {
            let focal = f64_attr(prim, "focalLength").filter(|f| *f > 0.0).unwrap_or(50.0);
            let yfov = (2.0 * (vertical / (2.0 * focal)).atan()) as f32;
            GlbProjection::Perspective {
                yfov: if yfov.is_finite() && yfov > 0.0 { yfov } else { 0.8 },
                znear,
                zfar: Some(zfar),
            }
        };
        let carrier = self.axis_node(node);
        self.extras.cameras.push(GlbCamera {
            name: prim_name(prim),
            projection,
        });
        self.extras
            .node_cameras
            .insert(carrier, self.extras.cameras.len() - 1);
        self.report.cameras += 1;
    }

    fn light(&mut self, prim: &usd::Prim, kind: &str, node: usize) {
        let intensity = lux(prim, "intensity").as_ref().and_then(f64_of).unwrap_or(1.0);
        let exposure = lux(prim, "exposure").as_ref().and_then(f64_of).unwrap_or(0.0);
        let color = lux(prim, "color").as_ref().and_then(vec3_of).unwrap_or([1.0; 3]);
        let cone = value(prim, "inputs:shaping:cone:angle")
            .or_else(|| value(prim, "shaping:cone:angle"))
            .as_ref()
            .and_then(f64_of)
            .map(|deg| deg.clamp(0.0, 90.0).to_radians() as f32);
        let softness = value(prim, "inputs:shaping:cone:softness")
            .as_ref()
            .and_then(f64_of)
            .unwrap_or(0.0)
            .clamp(0.0, 1.0) as f32;
        let spot = |outer: f32| GlbLightKind::Spot {
            inner: outer * (1.0 - softness),
            outer,
        };
        let light_kind = match kind {
            "DistantLight" => GlbLightKind::Directional,
            "SphereLight" => cone.map_or(GlbLightKind::Point, spot),
            _ => {
                self.warn(format!(
                    "{}: {kind} is approximated by a spot light",
                    prim.path().as_str()
                ));
                spot(cone.unwrap_or(std::f32::consts::FRAC_PI_2))
            }
        };
        let carrier = self.axis_node(node);
        self.extras.lights.push(GlbLight {
            name: prim_name(prim),
            kind: light_kind,
            color: color.map(|c| c.max(0.0) as f32),
            intensity: (intensity * exposure.exp2()).max(0.0) as f32,
            range: None,
        });
        self.extras
            .node_lights
            .insert(carrier, self.extras.lights.len() - 1);
        self.report.lights += 1;
    }
}

// ---------------------------------------------------------------------------------------------
// Meshes
// ---------------------------------------------------------------------------------------------

/// A primvar's values and how they map to corners.
struct Primvar<T> {
    values: Vec<T>,
    indices: Option<Vec<i64>>,
    interpolation: String,
}

impl<T: Copy> Primvar<T> {
    /// The value at a corner, by interpolation: `constant` (one value), `uniform` (per face),
    /// `vertex`/`varying` (per point), `faceVarying` (per corner).
    fn index(&self, face: usize, point: usize, corner: usize) -> Option<usize> {
        let slot = match self.interpolation.as_str() {
            "constant" => 0,
            "uniform" => face,
            "faceVarying" => corner,
            _ => point,
        };
        let index = match &self.indices {
            Some(indices) => usize::try_from(*indices.get(slot)?).ok()?,
            None => slot,
        };
        (index < self.values.len()).then_some(index)
    }
}

fn interpolation(attr: &usd::Attribute, fallback: &str) -> String {
    attr.get_metadata::<tf::Token>("interpolation")
        .ok()
        .flatten()
        .map_or_else(|| fallback.to_string(), |t| t.as_str().to_string())
}

fn primvar<T>(
    prim: &usd::Prim,
    name: &str,
    fallback: &str,
    read: impl Fn(&sdf::Value) -> Option<Vec<T>>,
) -> Option<Primvar<T>> {
    let values = value(prim, name).as_ref().and_then(read)?;
    let attr = prim.attribute(name);
    let indices = value(prim, &format!("{name}:indices")).as_ref().and_then(ints_of);
    Some(Primvar {
        values,
        indices,
        interpolation: interpolation(&attr, fallback),
    })
}

/// The UV set primvars of a mesh: `st` first, then the other two-component primvars by name.
fn uv_set_names(prim: &usd::Prim) -> Vec<String> {
    let mut names: Vec<String> = prim
        .attributes_in_namespace("primvars")
        .unwrap_or_default()
        .iter()
        .map(|a| a.name().to_string())
        .filter(|n| !n.ends_with(":indices"))
        .filter(|n| value(prim, n).as_ref().and_then(vec2s_of).is_some())
        .collect();
    names.sort_by_key(|n| (n != "primvars:st", n.clone()));
    names.dedup();
    names.truncate(8);
    names
}

impl Converter<'_> {
    fn mesh(&mut self, prim: &usd::Prim, node: usize, binding: &str) -> Result<(), UsdError> {
        // Instance proxies of one prototype mesh share the converted mesh.
        let prototype = if prim.is_instance_proxy().unwrap_or(false) {
            prim.prim_in_prototype()
                .ok()
                .flatten()
                .map(|p| p.path().as_str().to_string())
        } else {
            None
        };
        if let Some(key) = &prototype
            && let Some(&mesh) = self.instanced.get(&(key.clone(), binding.to_string()))
        {
            self.scene.nodes[node].mesh = Some(mesh);
            self.report.instanced_meshes += 1;
            return Ok(());
        }
        let Some(mesh) = self.convert_mesh(prim, binding)? else {
            return Ok(());
        };
        self.scene.meshes.push(mesh);
        let index = self.scene.meshes.len() - 1;
        self.scene.nodes[node].mesh = Some(index);
        self.report.meshes += 1;
        if let Some(key) = prototype {
            self.instanced.insert((key, binding.to_string()), index);
        }
        Ok(())
    }

    fn convert_mesh(&mut self, prim: &usd::Prim, binding: &str) -> Result<Option<GlbMesh>, UsdError> {
        let path = prim.path().as_str().to_string();
        // The settings live outside the converter: borrowing them does not borrow `self`.
        let settings: &UsdImportSettings = self.settings;
        let limits = &settings.limits;
        let points = value(prim, "points")
            .as_ref()
            .and_then(vec3s_of)
            .unwrap_or_default();
        let counts = value(prim, "faceVertexCounts")
            .as_ref()
            .and_then(ints_of)
            .unwrap_or_default();
        let indices = value(prim, "faceVertexIndices")
            .as_ref()
            .and_then(ints_of)
            .unwrap_or_default();
        if points.is_empty() || counts.is_empty() {
            self.warn(format!("{path}: mesh without points or faces; skipped"));
            return Ok(None);
        }
        // Faces: (first corner, corner count), validated.
        let mut faces: Vec<(usize, usize)> = Vec::with_capacity(counts.len());
        let mut start = 0usize;
        for &count in &counts {
            let Ok(count) = usize::try_from(count) else {
                return Err(UsdError::Parse(format!("{path}: negative face vertex count")));
            };
            if count > limits.max_polygon_corners {
                return Err(UsdError::Limit(format!(
                    "{path}: a face with {count} corners (limit {})",
                    limits.max_polygon_corners
                )));
            }
            faces.push((start, count));
            start = start.saturating_add(count);
        }
        if start != indices.len() {
            return Err(UsdError::Parse(format!(
                "{path}: faceVertexCounts sum to {start} but faceVertexIndices has {}",
                indices.len()
            )));
        }
        let mut point_index: Vec<usize> = Vec::with_capacity(indices.len());
        for &i in &indices {
            match usize::try_from(i).ok().filter(|i| *i < points.len()) {
                Some(i) => point_index.push(i),
                None => {
                    return Err(UsdError::Parse(format!(
                        "{path}: face vertex index {i} outside the {} points",
                        points.len()
                    )));
                }
            }
        }
        let left_handed = token_attr(prim, "orientation").as_deref() == Some("leftHanded");
        let double_sided = value(prim, "doubleSided").is_some_and(|v| matches!(v, sdf::Value::Bool(true)));
        let normals = primvar(prim, "primvars:normals", "faceVarying", vec3s_of)
            .or_else(|| primvar(prim, "normals", "vertex", vec3s_of));
        let uv_names = uv_set_names(prim);
        let uvs: Vec<Primvar<[f64; 2]>> = uv_names
            .iter()
            .filter_map(|n| primvar(prim, n, "faceVarying", vec2s_of))
            .collect();
        let colors = primvar(prim, "primvars:displayColor", "constant", vec3s_of);
        let opacity = primvar(prim, "primvars:displayOpacity", "constant", floats_of);
        let constant_color = colors
            .as_ref()
            .filter(|c| c.interpolation == "constant")
            .and_then(|c| c.values.first().copied());
        let vertex_colors = colors.as_ref().filter(|c| c.interpolation != "constant");

        // Faces by material: subsets of family `materialBind` with their own binding, the rest
        // with the mesh's.
        let mut face_material: Vec<usize> = vec![0; faces.len()];
        let mut groups: Vec<String> = vec![binding.to_string()];
        for child in prim.children().unwrap_or_default() {
            if type_name(&child) != "GeomSubset" {
                continue;
            }
            if token_attr(&child, "elementType").as_deref().unwrap_or("face") != "face" {
                continue;
            }
            let family = token_attr(&child, "familyName").unwrap_or_default();
            let Some(subset_binding) = self.binding(&child) else {
                continue;
            };
            if !family.is_empty() && family != "materialBind" {
                continue;
            }
            let group = groups.len();
            groups.push(subset_binding);
            for f in value(&child, "indices")
                .as_ref()
                .and_then(ints_of)
                .unwrap_or_default()
            {
                if let Some(slot) = usize::try_from(f).ok().and_then(|f| face_material.get_mut(f)) {
                    *slot = group;
                }
            }
        }

        let mut mesh = GlbMesh {
            name: prim_name(prim),
            ..GlbMesh::default()
        };
        let uv_set_short: Vec<String> = uv_names
            .iter()
            .map(|n| n.trim_start_matches("primvars:").to_string())
            .collect();
        for (group, group_binding) in groups.iter().enumerate() {
            let mut primitive = GlbPrimitive::default();
            let mut unique: HashMap<Vec<u32>, u32> = HashMap::new();
            let mut normals_ok = normals.is_some();
            let mut uvs_ok = vec![true; uvs.len()];
            let mut corner_key: Vec<u32> = Vec::new();
            for (face, &(first, count)) in faces.iter().enumerate() {
                if face_material[face] != group || count < 3 {
                    continue;
                }
                let corner_at = |k: usize| {
                    if left_handed {
                        first + (count - k) % count
                    } else {
                        first + k
                    }
                };
                for k in 1..count - 1 {
                    for corner in [corner_at(0), corner_at(k), corner_at(k + 1)] {
                        let point = point_index[corner];
                        corner_key.clear();
                        corner_key.push(point as u32);
                        let n = normals.as_ref().and_then(|p| p.index(face, point, corner));
                        corner_key.push(n.map_or(u32::MAX, |n| n as u32));
                        for set in &uvs {
                            corner_key.push(set.index(face, point, corner).map_or(u32::MAX, |i| i as u32));
                        }
                        let c = vertex_colors.and_then(|p| p.index(face, point, corner));
                        corner_key.push(c.map_or(u32::MAX, |c| c as u32));
                        let o = opacity.as_ref().and_then(|p| p.index(face, point, corner));
                        corner_key.push(o.map_or(u32::MAX, |o| o as u32));
                        let vertex = if let Some(&v) = unique.get(&corner_key) {
                            v
                        } else {
                            let v = primitive.positions.len() as u32;
                            primitive.positions.push(self.basis.point(points[point]));
                            match (n, &normals) {
                                (Some(n), Some(p)) => primitive.normals.push(self.basis.normal(p.values[n])),
                                _ => normals_ok = false,
                            }
                            if primitive.uvs.len() < uvs.len() {
                                primitive.uvs.resize(uvs.len(), Vec::new());
                            }
                            for (s, set) in uvs.iter().enumerate() {
                                match set.index(face, point, corner) {
                                    Some(i) => {
                                        let uv = set.values[i];
                                        primitive.uvs[s].push([uv[0] as f32, 1.0 - uv[1] as f32]);
                                    }
                                    None => {
                                        uvs_ok[s] = false;
                                        primitive.uvs[s].push([0.0, 0.0]);
                                    }
                                }
                            }
                            if let Some(colors) = vertex_colors {
                                let rgb = c.map_or([1.0; 3], |c| colors.values[c]);
                                let a = match (&opacity, o) {
                                    (Some(p), Some(o)) => p.values[o],
                                    _ => 1.0,
                                };
                                primitive.colors.push([
                                    rgb[0] as f32,
                                    rgb[1] as f32,
                                    rgb[2] as f32,
                                    a as f32,
                                ]);
                            }
                            unique.insert(corner_key.clone(), v);
                            self.total_vertices += 1;
                            if self.total_vertices > limits.max_vertices {
                                return Err(UsdError::Limit(format!(
                                    "more than {} vertices",
                                    limits.max_vertices
                                )));
                            }
                            v
                        };
                        primitive.indices.push(vertex);
                    }
                    self.total_indices += 3;
                    if self.total_indices > limits.max_indices {
                        return Err(UsdError::Limit(format!(
                            "more than {} triangle indices",
                            limits.max_indices
                        )));
                    }
                }
            }
            if primitive.indices.is_empty() {
                continue;
            }
            if !normals_ok || primitive.normals.len() != primitive.positions.len() {
                primitive.normals.clear();
            }
            // A UV set some corners lack ends the list there (texture coordinates stay dense).
            if let Some(first_bad) = uvs_ok.iter().position(|ok| !ok) {
                primitive.uvs.truncate(first_bad);
            }
            let kept_uv_sets = uv_set_short[..primitive.uvs.len()].to_vec();
            primitive.material = if group_binding.is_empty() {
                constant_color.map(|c| self.color_material(c, double_sided))
            } else {
                self.material(group_binding, &kept_uv_sets, double_sided)
                    .or_else(|| constant_color.map(|c| self.color_material(c, double_sided)))
            };
            self.report.vertices += primitive.positions.len();
            self.report.triangles += primitive.indices.len() / 3;
            mesh.primitives.push(primitive);
        }
        if mesh.primitives.is_empty() {
            self.warn(format!("{path}: no triangles; skipped"));
            return Ok(None);
        }
        Ok(Some(mesh))
    }

    /// A material for a mesh with a constant display color and no bound material.
    fn color_material(&mut self, color: [f64; 3], double_sided: bool) -> usize {
        let rgba = [
            color[0] as f32,
            color[1] as f32,
            color[2] as f32,
            if double_sided { 1.0 } else { 0.0 },
        ];
        let key = rgba.map(f32::to_bits);
        if let Some(&index) = self.color_materials.get(&key) {
            return index;
        }
        self.scene.materials.push(GlbMaterial {
            name: format!("displayColor_{}", self.color_materials.len()),
            base_color: [rgba[0], rgba[1], rgba[2], 1.0],
            metallic: 0.0,
            roughness: 0.5,
            double_sided,
            ..GlbMaterial::default()
        });
        let index = self.scene.materials.len() - 1;
        self.color_materials.insert(key, index);
        index
    }
}

// ---------------------------------------------------------------------------------------------
// Materials
// ---------------------------------------------------------------------------------------------

impl Converter<'_> {
    /// The attribute a connection on `prim.name` leads to, following through node-graph and
    /// material interface outputs and inputs (at most 8 hops).
    fn source(&self, prim: &usd::Prim, name: &str) -> Option<(usd::Prim, String)> {
        let mut attr_path = prim.attribute(name).connections().ok()?.first()?.clone();
        for _ in 0..8 {
            let (owner, property) = attr_path.split_property()?;
            let property = property.to_string();
            let source = self.stage.prim(owner).ok()?;
            let kind = type_name(&source);
            if kind == "Shader" {
                return Some((source, property));
            }
            // A node graph's output or a material's input: follow its own connection.
            match source
                .attribute(property.as_str())
                .connections()
                .ok()
                .and_then(|c| c.first().cloned())
            {
                Some(next) => attr_path = next,
                None => return Some((source, property)),
            }
        }
        None
    }

    /// A preview-surface input: a texture through `UsdUVTexture`, else the composed value (of
    /// the input itself or the interface input it connects to).
    fn input(&mut self, shader: &usd::Prim, name: &str) -> Input {
        let attr_name = format!("inputs:{name}");
        if let Some((source, output)) = self.source(shader, &attr_name) {
            let id = token_attr(&source, "info:id").unwrap_or_default();
            if id == "UsdUVTexture" {
                if let Some(texture) = self.texture_input(&source, &output) {
                    return Input {
                        value: None,
                        texture: Some(texture),
                    };
                }
                return Input::default();
            }
            if type_name(&source) != "Shader" {
                return Input {
                    value: value(&source, &output),
                    texture: None,
                };
            }
            self.warn(format!(
                "{}: input {name} is driven by shader {id:?}, which is not read",
                shader.path().as_str()
            ));
            return Input::default();
        }
        Input {
            value: value(shader, &attr_name),
            texture: None,
        }
    }

    fn texture_input(&mut self, texture: &usd::Prim, output: &str) -> Option<TextureInput> {
        // A typed read: value resolution anchors the path at the layer that authored it and
        // resolves it through the import's resolver.
        let file = match texture.attribute("inputs:file").get::<sdf::AssetPath>() {
            Ok(Some(asset)) => asset,
            _ => {
                self.warn(format!(
                    "{}: UsdUVTexture without a file",
                    texture.path().as_str()
                ));
                return None;
            }
        };
        let Some(id) = file.resolved_path().map(str::to_string) else {
            self.warn(format!(
                "{}: texture @{}@ not found; slot dropped",
                texture.path().as_str(),
                file.as_str()
            ));
            return None;
        };
        let channel = output.strip_prefix("outputs:").unwrap_or(output).to_string();
        let mut uv_set = "st".to_string();
        if let Some((reader, _)) = self.source(texture, "inputs:st") {
            let id = token_attr(&reader, "info:id").unwrap_or_default();
            if id.starts_with("UsdPrimvarReader") {
                let varname = match self.source(&reader, "inputs:varname") {
                    Some((owner, property)) => token_attr(&owner, &property),
                    None => token_attr(&reader, "inputs:varname"),
                };
                if let Some(varname) = varname.filter(|v| !v.is_empty()) {
                    uv_set = varname;
                }
            } else if id == "UsdTransform2d" {
                self.warn(format!(
                    "{}: UsdTransform2d is not applied",
                    texture.path().as_str()
                ));
                if let Some((reader, _)) = self.source(&reader, "inputs:in")
                    && let Some(varname) = token_attr(&reader, "inputs:varname")
                {
                    uv_set = varname;
                }
            }
        }
        Some(TextureInput { id, channel, uv_set })
    }

    /// The UsdPreviewSurface behind a material prim.
    fn surface(&mut self, material: &str) -> Option<Surface> {
        if let Some(found) = self.surfaces.get(material) {
            return found.clone();
        }
        let parsed = self.parse_surface(material);
        self.surfaces.insert(material.to_string(), parsed.clone());
        parsed
    }

    fn parse_surface(&mut self, material: &str) -> Option<Surface> {
        let prim = self.stage.prim(material).ok()?;
        if type_name(&prim) != "Material" {
            self.warn(format!("{material}: bound material is not a Material prim"));
            return None;
        }
        let shader = ["outputs:surface", "outputs:preview:surface"]
            .iter()
            .find_map(|output| self.source(&prim, output))
            .map(|(shader, _)| shader);
        let Some(shader) = shader else {
            self.warn(format!("{material}: no surface shader connected"));
            return None;
        };
        let id = token_attr(&shader, "info:id").unwrap_or_default();
        if id != "UsdPreviewSurface" {
            self.warn(format!(
                "{material}: surface shader {id:?} is not UsdPreviewSurface; default material"
            ));
            return None;
        }
        if value(&shader, "inputs:useSpecularWorkflow").is_some_and(|v| matches!(v, sdf::Value::Int(1))) {
            self.warn(format!(
                "{material}: the specular workflow is read as metallic-roughness"
            ));
        }
        Some(Surface {
            name: prim_name(&prim),
            diffuse: self.input(&shader, "diffuseColor"),
            emissive: self.input(&shader, "emissiveColor"),
            metallic: self.input(&shader, "metallic"),
            roughness: self.input(&shader, "roughness"),
            opacity: self.input(&shader, "opacity"),
            opacity_threshold: f64_attr(&shader, "inputs:opacityThreshold"),
            normal: self.input(&shader, "normal"),
            occlusion: self.input(&shader, "occlusion"),
        })
    }

    /// The glTF material for `material` on a primitive with UV sets `uv_sets` (short names).
    fn material(&mut self, material: &str, uv_sets: &[String], double_sided: bool) -> Option<usize> {
        let key = (material.to_string(), uv_sets.to_vec(), double_sided);
        if let Some(&index) = self.materials.get(&key) {
            return Some(index);
        }
        let surface = self.surface(material)?;
        let tex_coord =
            |t: &TextureInput| -> u32 { uv_sets.iter().position(|n| *n == t.uv_set).unwrap_or(0) as u32 };
        let rgb = |input: &Input, fallback: [f64; 3]| -> [f32; 3] {
            input
                .value
                .as_ref()
                .and_then(vec3_of)
                .unwrap_or(fallback)
                .map(|c| c as f32)
        };
        let scalar = |input: &Input, fallback: f64| -> f32 {
            input.value.as_ref().and_then(f64_of).unwrap_or(fallback) as f32
        };
        let base = if surface.diffuse.texture.is_some() {
            [1.0; 3]
        } else {
            rgb(&surface.diffuse, [0.18; 3])
        };
        let alpha = if surface.opacity.texture.is_some() {
            1.0
        } else {
            scalar(&surface.opacity, 1.0)
        };
        let mut out = GlbMaterial {
            name: surface.name.clone(),
            base_color: [base[0], base[1], base[2], alpha],
            metallic: if surface.metallic.texture.is_some() {
                1.0
            } else {
                scalar(&surface.metallic, 0.0)
            },
            roughness: if surface.roughness.texture.is_some() {
                1.0
            } else {
                scalar(&surface.roughness, 0.5)
            },
            emissive: if surface.emissive.texture.is_some() {
                [1.0; 3]
            } else {
                rgb(&surface.emissive, [0.0; 3])
            },
            double_sided,
            ..GlbMaterial::default()
        };
        // Base color: the texture's RGB, with its alpha when opacity reads the same file.
        if let Some(t) = &surface.diffuse.texture {
            let alpha_from = surface
                .opacity
                .texture
                .as_ref()
                .filter(|o| o.id == t.id && o.channel == "a");
            if surface.opacity.texture.is_some() && alpha_from.is_none() {
                self.warn(format!(
                    "{material}: opacity from another texture or channel is not read"
                ));
            }
            out.base_color_texture = self.image(&t.id).map(|image| GlbTextureRef {
                image,
                tex_coord: tex_coord(t),
            });
        }
        if let Some(t) = &surface.emissive.texture {
            out.emissive_texture = self.image(&t.id).map(|image| GlbTextureRef {
                image,
                tex_coord: tex_coord(t),
            });
        }
        if let Some(t) = &surface.normal.texture {
            out.normal_texture = self.image(&t.id).map(|image| GlbTextureRef {
                image,
                tex_coord: tex_coord(t),
            });
        }
        // Metallic in blue, roughness in green (glTF): as authored when one file has them there,
        // else packed into a new image.
        let metallic = surface.metallic.texture.clone();
        let roughness = surface.roughness.texture.clone();
        if metallic.is_some() || roughness.is_some() {
            let uv = metallic
                .as_ref()
                .or(roughness.as_ref())
                .map(tex_coord)
                .unwrap_or(0);
            let image = match (&metallic, &roughness) {
                (Some(m), Some(r)) if m.id == r.id && m.channel == "b" && r.channel == "g" => {
                    self.image(&m.id)
                }
                _ => {
                    let sources = vec![
                        (String::new(), String::new()),
                        roughness
                            .as_ref()
                            .map_or_else(Default::default, |r| (r.id.clone(), r.channel.clone())),
                        metallic
                            .as_ref()
                            .map_or_else(Default::default, |m| (m.id.clone(), m.channel.clone())),
                    ];
                    let fill = [
                        1.0,
                        scalar(&surface.roughness, 0.5),
                        scalar(&surface.metallic, 0.0),
                    ];
                    self.packed_image(sources, fill)
                }
            };
            out.metallic_roughness_texture = image.map(|image| GlbTextureRef { image, tex_coord: uv });
            if metallic.is_none() {
                out.metallic = 1.0;
            }
            if roughness.is_none() {
                out.roughness = 1.0;
            }
        }
        if let Some(t) = surface.occlusion.texture.clone() {
            let image = if t.channel == "r" || t.channel == "rgb" {
                self.image(&t.id)
            } else {
                self.packed_image(
                    vec![
                        (t.id.clone(), t.channel.clone()),
                        Default::default(),
                        Default::default(),
                    ],
                    [1.0, 1.0, 1.0],
                )
            };
            if let Some(image) = image {
                let index = self.scene.materials.len();
                self.extras.occlusion.insert(
                    index,
                    (
                        GlbTextureRef {
                            image,
                            tex_coord: tex_coord(&t),
                        },
                        1.0,
                    ),
                );
            }
        }
        let index = self.scene.materials.len();
        match surface.opacity_threshold.filter(|t| *t > 0.0) {
            Some(cutoff) => {
                self.extras.alpha_mask.insert(index, cutoff as f32);
            }
            None => out.alpha_blend = alpha < 1.0 || surface.opacity.texture.is_some(),
        }
        self.scene.materials.push(out);
        self.materials.insert(key, index);
        Some(index)
    }

    /// The glTF image of a texture file, embedded (PNG and JPEG as they are, anything else
    /// decoded and re-encoded as PNG).
    fn image(&mut self, id: &str) -> Option<usize> {
        if let Some(found) = self.images.get(id) {
            return *found;
        }
        let found = self.embed(id);
        self.images.insert(id.to_string(), found);
        found
    }

    fn embed(&mut self, id: &str) -> Option<usize> {
        let Some(bytes) = self.set.asset_bytes(id) else {
            self.warn(format!("texture {} not found; slot dropped", display(id)));
            return None;
        };
        let image = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
            GlbImage::Embedded {
                bytes,
                mime: "image/png",
            }
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            GlbImage::Embedded {
                bytes,
                mime: "image/jpeg",
            }
        } else {
            let Some(decoded) = image::load_from_memory(&bytes).ok() else {
                self.warn(format!(
                    "texture {} could not be decoded; slot dropped",
                    display(id)
                ));
                return None;
            };
            let mut png = std::io::Cursor::new(Vec::new());
            decoded.write_to(&mut png, image::ImageFormat::Png).ok()?;
            GlbImage::Embedded {
                bytes: png.into_inner(),
                mime: "image/png",
            }
        };
        self.scene.images.push(image);
        self.report.textures.push(display(id));
        Some(self.scene.images.len() - 1)
    }

    /// A new RGB image whose channel `i` is channel `sources[i].1` of image `sources[i].0`, or
    /// `fill[i]` where the source is empty or unreadable. Sizes follow the largest source.
    fn packed_image(&mut self, sources: Vec<(String, String)>, fill: [f32; 3]) -> Option<usize> {
        let key: Vec<(String, String)> = sources
            .iter()
            .cloned()
            .chain(std::iter::once((format!("{fill:?}"), String::new())))
            .collect();
        if let Some(found) = self.packed.get(&key) {
            return *found;
        }
        let mut decoded: Vec<Option<image::RgbaImage>> = Vec::new();
        for (id, _) in &sources {
            let image = if id.is_empty() {
                None
            } else {
                let bytes = self.set.asset_bytes(id);
                if !self.report.textures.contains(&display(id)) && bytes.is_some() {
                    self.report.textures.push(display(id));
                }
                bytes
                    .and_then(|b| image::load_from_memory(&b).ok())
                    .map(|i| i.to_rgba8())
            };
            decoded.push(image);
        }
        let (width, height) = decoded
            .iter()
            .flatten()
            .map(|i| i.dimensions())
            .max()
            .unwrap_or((0, 0));
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 1 << 26 {
            self.warn("packed texture sources unreadable; slot dropped".into());
            self.packed.insert(key, None);
            return None;
        }
        let resized: Vec<Option<image::RgbaImage>> = decoded
            .into_iter()
            .map(|i| {
                i.map(|i| {
                    if i.dimensions() == (width, height) {
                        i
                    } else {
                        image::imageops::resize(&i, width, height, image::imageops::FilterType::Triangle)
                    }
                })
            })
            .collect();
        let channel_index = |c: &str| match c {
            "g" => 1,
            "b" => 2,
            "a" => 3,
            _ => 0,
        };
        let mut out = image::RgbImage::new(width, height);
        for (x, y, pixel) in out.enumerate_pixels_mut() {
            for (slot, ((_, channel), source)) in sources.iter().zip(&resized).enumerate() {
                pixel[slot] = match source {
                    Some(source) => source.get_pixel(x, y)[channel_index(channel)],
                    None => (fill[slot].clamp(0.0, 1.0) * 255.0).round() as u8,
                };
            }
        }
        let mut png = std::io::Cursor::new(Vec::new());
        let written = image::DynamicImage::ImageRgb8(out)
            .write_to(&mut png, image::ImageFormat::Png)
            .is_ok();
        let found = written.then(|| {
            self.scene.images.push(GlbImage::Embedded {
                bytes: png.into_inner(),
                mime: "image/png",
            });
            self.scene.images.len() - 1
        });
        self.packed.insert(key, found);
        found
    }
}
