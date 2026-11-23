// SPDX-License-Identifier: MPL-2.0
//! Surface shaders: `standard_surface` (Standard Surface specification) and
//! `open_pbr_surface` (OpenPBR Surface specification) onto the `material_surface` outputs and
//! the graph's shading block (`specs/13` § 7).
//!
//! Both shaders are layered BSDFs with more lobes than Regia's lit model. What maps directly:
//! base colour (times its weight), metalness, specular roughness, the normal, emission, coverage
//! (opacity), a constant clearcoat weight and roughness, and a constant specular anisotropy.
//! What is approximated: subsurface (the diffuse colour blends toward the subsurface colour by
//! its weight) and transmission (coverage `1 - transmission` with alpha blending, no
//! refraction). Everything else is dropped with a `mtlx_lossy_input` warning when the document
//! sets it away from its default.

use regia_matgraph::{GraphAlphaMode, NormalSpace, ValueType};

use super::doc::{Document, NodeElem};
use super::translate::{Shape, Translator, Val};
use super::{AlphaPolicy, MaterialXError, MaterialXSettings, ShadingModel};

/// Input names and defaults of one surface shader.
struct Spec {
    base_weight: (&'static str, f32),
    base_color: (&'static str, [f32; 3]),
    metalness: (&'static str, f32),
    roughness: (&'static str, f32),
    subsurface: (&'static str, &'static str, [f32; 3]),
    transmission: &'static str,
    coat: (&'static str, &'static str, f32),
    anisotropy: &'static str,
    rotation: Option<&'static str>,
    emission: (&'static str, &'static str),
    /// The emission weight is a luminance in nits (OpenPBR), not a unitless weight.
    emission_nits: bool,
    /// Opacity input and its type (`color3` in Standard Surface, `float` in OpenPBR).
    opacity: (&'static str, ValueType),
    normal: &'static str,
    /// Inputs dropped when set away from their default: name, default, what is lost.
    dropped: &'static [(&'static str, [f32; 4], &'static str)],
    /// Vector inputs whose default is a geometric property: any connection is dropped.
    ignored_connections: &'static [&'static str],
}

const F0: &str = "the lit model's specular is a fixed dielectric F0 of 0.04 (IOR 1.5) tinted by metalness";

const STANDARD_SURFACE: Spec = Spec {
    base_weight: ("base", 1.0),
    base_color: ("base_color", [0.8, 0.8, 0.8]),
    metalness: ("metalness", 0.0),
    roughness: ("specular_roughness", 0.2),
    subsurface: ("subsurface", "subsurface_color", [1.0, 1.0, 1.0]),
    transmission: "transmission",
    coat: ("coat", "coat_roughness", 0.1),
    anisotropy: "specular_anisotropy",
    rotation: Some("specular_rotation"),
    emission: ("emission", "emission_color"),
    emission_nits: false,
    opacity: ("opacity", ValueType::Vec3),
    normal: "normal",
    dropped: &[
        (
            "diffuse_roughness",
            [0.0; 4],
            "the lit model's diffuse is Lambertian",
        ),
        ("specular", [1.0, 0.0, 0.0, 0.0], F0),
        ("specular_color", [1.0, 1.0, 1.0, 0.0], F0),
        ("specular_IOR", [1.5, 0.0, 0.0, 0.0], F0),
        (
            "transmission_color",
            [1.0, 1.0, 1.0, 0.0],
            "transmission tint is not modelled",
        ),
        (
            "transmission_depth",
            [0.0; 4],
            "transmission depth is not modelled",
        ),
        (
            "transmission_scatter",
            [0.0; 4],
            "transmission scattering is not modelled",
        ),
        (
            "transmission_scatter_anisotropy",
            [0.0; 4],
            "transmission scattering is not modelled",
        ),
        ("transmission_dispersion", [0.0; 4], "dispersion is not modelled"),
        (
            "transmission_extra_roughness",
            [0.0; 4],
            "transmission roughness is not modelled",
        ),
        (
            "subsurface_radius",
            [1.0, 1.0, 1.0, 0.0],
            "subsurface scattering distance is not modelled",
        ),
        (
            "subsurface_scale",
            [1.0, 0.0, 0.0, 0.0],
            "subsurface scattering distance is not modelled",
        ),
        (
            "subsurface_anisotropy",
            [0.0; 4],
            "subsurface anisotropy is not modelled",
        ),
        ("sheen", [0.0; 4], "the lit model has no sheen lobe"),
        (
            "sheen_color",
            [1.0, 1.0, 1.0, 0.0],
            "the lit model has no sheen lobe",
        ),
        (
            "sheen_roughness",
            [0.3, 0.0, 0.0, 0.0],
            "the lit model has no sheen lobe",
        ),
        ("coat_color", [1.0, 1.0, 1.0, 0.0], "the clearcoat is untinted"),
        ("coat_anisotropy", [0.0; 4], "the clearcoat is isotropic"),
        ("coat_rotation", [0.0; 4], "the clearcoat is isotropic"),
        ("coat_IOR", [1.5, 0.0, 0.0, 0.0], "the clearcoat IOR is fixed"),
        (
            "coat_affect_color",
            [0.0; 4],
            "the clearcoat does not darken the base",
        ),
        (
            "coat_affect_roughness",
            [0.0; 4],
            "the clearcoat does not roughen the base",
        ),
        (
            "thin_film_thickness",
            [0.0; 4],
            "thin-film iridescence is not modelled",
        ),
        (
            "thin_film_IOR",
            [1.5, 0.0, 0.0, 0.0],
            "thin-film iridescence is not modelled",
        ),
        ("thin_walled", [0.0; 4], "thin-walled geometry is shaded as solid"),
    ],
    ignored_connections: &["coat_normal", "tangent"],
};

const OPEN_PBR_SURFACE: Spec = Spec {
    base_weight: ("base_weight", 1.0),
    base_color: ("base_color", [0.8, 0.8, 0.8]),
    metalness: ("base_metalness", 0.0),
    roughness: ("specular_roughness", 0.3),
    subsurface: ("subsurface_weight", "subsurface_color", [0.8, 0.8, 0.8]),
    transmission: "transmission_weight",
    coat: ("coat_weight", "coat_roughness", 0.0),
    anisotropy: "specular_roughness_anisotropy",
    rotation: None,
    emission: ("emission_luminance", "emission_color"),
    emission_nits: true,
    opacity: ("geometry_opacity", ValueType::Float),
    normal: "geometry_normal",
    dropped: &[
        (
            "base_diffuse_roughness",
            [0.0; 4],
            "the lit model's diffuse is Lambertian",
        ),
        ("specular_weight", [1.0, 0.0, 0.0, 0.0], F0),
        ("specular_color", [1.0, 1.0, 1.0, 0.0], F0),
        ("specular_ior", [1.5, 0.0, 0.0, 0.0], F0),
        (
            "transmission_color",
            [1.0, 1.0, 1.0, 0.0],
            "transmission tint is not modelled",
        ),
        (
            "transmission_depth",
            [0.0; 4],
            "transmission depth is not modelled",
        ),
        (
            "transmission_scatter",
            [0.0; 4],
            "transmission scattering is not modelled",
        ),
        (
            "transmission_scatter_anisotropy",
            [0.0; 4],
            "transmission scattering is not modelled",
        ),
        (
            "transmission_dispersion_scale",
            [0.0; 4],
            "dispersion is not modelled",
        ),
        (
            "transmission_dispersion_abbe_number",
            [20.0, 0.0, 0.0, 0.0],
            "dispersion is not modelled",
        ),
        (
            "subsurface_radius",
            [1.0, 0.0, 0.0, 0.0],
            "subsurface scattering distance is not modelled",
        ),
        (
            "subsurface_radius_scale",
            [1.0, 0.5, 0.25, 0.0],
            "subsurface scattering distance is not modelled",
        ),
        (
            "subsurface_scatter_anisotropy",
            [0.0; 4],
            "subsurface anisotropy is not modelled",
        ),
        ("fuzz_weight", [0.0; 4], "the lit model has no fuzz (sheen) lobe"),
        (
            "fuzz_color",
            [1.0, 1.0, 1.0, 0.0],
            "the lit model has no fuzz (sheen) lobe",
        ),
        (
            "fuzz_roughness",
            [0.5, 0.0, 0.0, 0.0],
            "the lit model has no fuzz (sheen) lobe",
        ),
        ("coat_color", [1.0, 1.0, 1.0, 0.0], "the clearcoat is untinted"),
        (
            "coat_roughness_anisotropy",
            [0.0; 4],
            "the clearcoat is isotropic",
        ),
        ("coat_ior", [1.6, 0.0, 0.0, 0.0], "the clearcoat IOR is fixed"),
        (
            "coat_darkening",
            [1.0, 0.0, 0.0, 0.0],
            "the clearcoat does not darken the base",
        ),
        (
            "thin_film_weight",
            [0.0; 4],
            "thin-film iridescence is not modelled",
        ),
        (
            "thin_film_thickness",
            [0.5, 0.0, 0.0, 0.0],
            "thin-film iridescence is not modelled",
        ),
        (
            "thin_film_ior",
            [1.4, 0.0, 0.0, 0.0],
            "thin-film iridescence is not modelled",
        ),
        (
            "geometry_thin_walled",
            [0.0; 4],
            "thin-walled geometry is shaded as solid",
        ),
    ],
    ignored_connections: &[
        "geometry_coat_normal",
        "geometry_tangent",
        "geometry_coat_tangent",
    ],
};

fn spec_for(category: &str) -> Option<&'static Spec> {
    match category {
        "standard_surface" => Some(&STANDARD_SURFACE),
        "open_pbr_surface" => Some(&OPEN_PBR_SURFACE),
        _ => None,
    }
}

fn top_level(doc: &Document) -> impl Iterator<Item = &NodeElem> {
    doc.order.iter().filter_map(|n| doc.nodes.get(n))
}

/// The material to import and its surface shader node.
pub(crate) fn pick_material<'d>(
    doc: &'d Document,
    settings: &MaterialXSettings,
) -> Result<(String, &'d NodeElem), MaterialXError> {
    let materials: Vec<&NodeElem> = top_level(doc)
        .filter(|n| n.category == "surfacematerial")
        .collect();
    let chosen = match &settings.material {
        Some(want) => Some(
            materials
                .iter()
                .copied()
                .find(|m| &m.name == want)
                .ok_or_else(|| {
                    let found: Vec<&str> = materials.iter().map(|m| m.name.as_str()).collect();
                    MaterialXError::NoMaterial(format!(
                        "no surfacematerial `{want}` (found: {})",
                        found.join(", ")
                    ))
                })?,
        ),
        None => materials.first().copied(),
    };
    let (name, shader) = match chosen {
        Some(material) => {
            let shader_name = material
                .input("surfaceshader")
                .and_then(|i| i.nodename.as_deref())
                .ok_or_else(|| {
                    MaterialXError::NoMaterial(format!(
                        "`{}` has no surfaceshader input connected to a node",
                        material.name
                    ))
                })?;
            let shader = doc.nodes.get(shader_name).ok_or_else(|| {
                MaterialXError::NoMaterial(format!(
                    "`{}` reads `{shader_name}`, which is not a top-level node",
                    material.name
                ))
            })?;
            (material.name.clone(), shader)
        }
        None => {
            let shader = top_level(doc).find(|n| n.ty == "surfaceshader").ok_or_else(|| {
                MaterialXError::NoMaterial("the document has no surfacematerial and no surface shader".into())
            })?;
            (shader.name.clone(), shader)
        }
    };
    if spec_for(&shader.category).is_none() {
        return Err(MaterialXError::UnsupportedShader {
            name: shader.name.clone(),
            category: shader.category.clone(),
        });
    }
    Ok((name, shader))
}

/// The document's other materials (one file imports one).
pub(crate) fn extra_materials(doc: &Document, chosen: &str) -> Vec<String> {
    top_level(doc)
        .filter(|n| n.category == "surfacematerial" && n.name != chosen)
        .map(|n| n.name.clone())
        .collect()
}

fn pad3(c: [f32; 3]) -> [f32; 4] {
    [c[0], c[1], c[2], 0.0]
}

fn scalar(v: f32) -> [f32; 4] {
    [v, 0.0, 0.0, 0.0]
}

/// A shader input that becomes a material parameter once the node graphs have taken theirs:
/// until then it is a constant node the end of [`map_surface`] turns into a `Param`.
struct Deferred {
    node: String,
    name: String,
    source: String,
    ty: ValueType,
    value: [f32; 4],
    color: bool,
}

/// One shader input: its connection or value. An unconnected input with `expose` (when the
/// settings expose shader inputs) is a placeholder node queued in `deferred`.
fn shader_input<'d>(
    t: &mut Translator<'d>,
    deferred: &mut Vec<Deferred>,
    shader: &'d NodeElem,
    name: &str,
    ty: ValueType,
    default: [f32; 4],
    expose: bool,
) -> Val {
    let input = shader.input(name);
    let connected = input.is_some_and(super::doc::Input::is_connected);
    if expose && !connected && t.settings.expose_surface_params {
        let source = format!("{}.{name}", shader.name);
        let text = input.and_then(|i| i.value.as_deref());
        let parsed = text.and_then(|text| super::translate::parse_value(text, ty.width()));
        if let (Some(text), None) = (text, parsed) {
            t.warn(
                super::WARN_BAD_VALUE,
                &source,
                "",
                format!("`{text}` is not a {ty:?}; the default is used"),
            );
        }
        let value = match (input, parsed) {
            (Some(i), Some(v)) => super::translate::decode_value_colorspace(i, v),
            _ => default,
        };
        let color = input.map_or(ty == ValueType::Vec3, |i| i.ty.starts_with("color"));
        let node = t.add(&format!("p_{name}"), super::translate::constant_node(ty, value));
        deferred.push(Deferred {
            node: node.clone(),
            name: name.to_string(),
            source,
            ty,
            value,
            color,
        });
        return Val {
            node,
            ty,
            constant: None,
            shape: Shape::Plain,
        };
    }
    t.input(0, shader, name, ty, default)
}

/// A constant shader input for the shading block, or the default with a warning when the
/// document drives it per pixel.
fn constant_input<'d>(
    t: &mut Translator<'d>,
    shader: &'d NodeElem,
    name: &str,
    default: f32,
    what: &str,
) -> f32 {
    let v = t.input(0, shader, name, ValueType::Float, scalar(default));
    match v.constant {
        Some(c) => c[0],
        None => {
            t.lossy(
                &format!("{}.{name}", shader.name),
                format!(
                    "{what} is a constant of the shading block; the varying input is replaced by {default}"
                ),
            );
            default
        }
    }
}

/// Maps `shader` onto the graph's outputs and shading block.
pub(crate) fn map_surface<'d>(t: &mut Translator<'d>, shader: &'d NodeElem) {
    let Some(spec) = spec_for(&shader.category) else {
        return;
    };
    let at = |input: &str| format!("{}.{input}", shader.name);
    let mut deferred = Vec::new();
    let v3 = ValueType::Vec3;
    let f = ValueType::Float;

    // Base colour (times its weight), subsurface as a blend toward its colour.
    let weight = shader_input(
        t,
        &mut deferred,
        shader,
        spec.base_weight.0,
        f,
        scalar(spec.base_weight.1),
        false,
    );
    let color = shader_input(
        t,
        &mut deferred,
        shader,
        spec.base_color.0,
        v3,
        pad3(spec.base_color.1),
        true,
    );
    let mut base = t.mul(color, weight);
    let (sss, sss_color, sss_default) = spec.subsurface;
    if t.non_default(0, shader, sss, [0.0; 4]) {
        let w = t.input(0, shader, sss, f, [0.0; 4]);
        let c = t.input(0, shader, sss_color, v3, pad3(sss_default));
        base = t.lerp(base, c, w);
        t.lossy(
            &at(sss),
            "subsurface is approximated by blending the diffuse colour toward the subsurface colour",
        );
    }
    let base = t.convert(base, v3);
    t.graph.output.base_color = Some(base.node);

    let metal = shader_input(
        t,
        &mut deferred,
        shader,
        spec.metalness.0,
        f,
        scalar(spec.metalness.1),
        true,
    );
    t.graph.output.metallic = Some(metal.node);
    let rough = shader_input(
        t,
        &mut deferred,
        shader,
        spec.roughness.0,
        f,
        scalar(spec.roughness.1),
        true,
    );
    t.graph.output.roughness = Some(rough.node);

    // Normal: a normal map feeds the tangent-space output, anything else the world-space one.
    if let Some(input) = shader.input(spec.normal).filter(|i| i.is_connected())
        && let Some(n) = t.resolve(0, input, v3, &shader.name)
    {
        match n.shape {
            Shape::NormalMap { tangent } => {
                t.graph.output.normal = Some(tangent);
                t.graph.output.normal_space = NormalSpace::Tangent;
            }
            _ => {
                // A tangent-space normal map blended, normalized or otherwise computed on is
                // only its world-normal placeholder here: say so instead of dropping it silently.
                if t.reaches_normal_map(&n.node) {
                    t.lossy(
                        &at(spec.normal),
                        "a tangent-space normal map that passes through other nodes is dropped: \
                         only a normalmap wired straight to the normal is applied",
                    );
                }
                t.graph.output.normal = Some(n.node);
                t.graph.output.normal_space = NormalSpace::World;
            }
        }
    }

    // Emission.
    let (ew, ec) = spec.emission;
    if t.non_default(0, shader, ew, [0.0; 4]) {
        let w = shader_input(t, &mut deferred, shader, ew, f, [0.0; 4], true);
        let c = shader_input(t, &mut deferred, shader, ec, v3, [1.0; 4], true);
        let mut e = t.mul(c, w);
        if !spec.emission_nits && t.settings.emission_scale != 1.0 {
            let k = t.constant(f, scalar(t.settings.emission_scale));
            e = t.mul(e, k);
        }
        let e = t.convert(e, v3);
        t.graph.output.emissive = Some(e.node);
    }

    // Coverage: opacity, then transmission as coverage.
    let mut alpha: Option<Val> = None;
    let (opacity, opacity_ty) = spec.opacity;
    let opaque = if opacity_ty == v3 {
        [1.0, 1.0, 1.0, 0.0]
    } else {
        scalar(1.0)
    };
    // `alpha: Opaque` drops coverage: whether the document sets it is read without resolving a
    // connection, so the dropped inputs declare no textures or parameters (as the `dropped`
    // inputs below, ADR-5571).
    let opaque_policy = t.settings.alpha == AlphaPolicy::Opaque;
    if opaque_policy {
        let set = |t: &mut Translator<'d>, name: &str, default: [f32; 4]| match shader.input(name) {
            Some(input) if input.is_connected() => true,
            _ => t.non_default(0, shader, name, default),
        };
        if set(t, opacity, opaque) || set(t, spec.transmission, [0.0; 4]) {
            t.lossy(
                &at(opacity),
                "opacity and transmission are dropped (alpha: Opaque in the .meta)",
            );
        }
    }
    if !opaque_policy && t.non_default(0, shader, opacity, opaque) {
        let o = shader_input(t, &mut deferred, shader, opacity, opacity_ty, opaque, true);
        let o = if opacity_ty == v3 {
            t.lossy(
                &at(opacity),
                "an opacity colour is averaged to one coverage value",
            );
            let third = t.constant(v3, [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0, 0.0]);
            if let (Some(c), Some(k)) = (o.constant, third.constant) {
                t.constant(f, scalar(c[0] * k[0] + c[1] * k[1] + c[2] * k[2]))
            } else {
                let node = t.add("opacity", regia_matgraph::Node::Dot(o.node, third.node));
                Val {
                    node,
                    ty: f,
                    constant: None,
                    shape: Shape::Plain,
                }
            }
        } else {
            o
        };
        alpha = Some(o);
    }
    if !opaque_policy && t.non_default(0, shader, spec.transmission, [0.0; 4]) {
        let tr = t.input(0, shader, spec.transmission, f, [0.0; 4]);
        let one = t.constant(f, scalar(1.0));
        let cover = t.sub(one, tr);
        alpha = Some(match alpha {
            Some(a) => t.mul(a, cover),
            None => cover,
        });
        t.lossy(
            &at(spec.transmission),
            "transmission is approximated by coverage (alpha blend); there is no refraction",
        );
    }
    match t.settings.alpha {
        AlphaPolicy::Auto => {
            if let Some(a) = alpha {
                t.graph.output.alpha = Some(a.node);
                t.graph.shading.alpha_mode = GraphAlphaMode::Blend;
            }
        }
        // Reported above, before anything was resolved.
        AlphaPolicy::Opaque => {}
        AlphaPolicy::Mask(cutoff) => {
            if let Some(a) = alpha {
                t.graph.output.alpha = Some(a.node);
            }
            t.graph.shading.alpha_mode = GraphAlphaMode::Mask(cutoff);
        }
        AlphaPolicy::Blend => {
            if let Some(a) = alpha {
                t.graph.output.alpha = Some(a.node);
            }
            t.graph.shading.alpha_mode = GraphAlphaMode::Blend;
        }
    }

    // Shading block: clearcoat and anisotropy are constants there.
    let (coat, coat_rough, coat_rough_default) = spec.coat;
    let coat_w = constant_input(t, shader, coat, 0.0, "the clearcoat weight");
    if coat_w > 0.0 {
        t.graph.shading.clearcoat = coat_w.clamp(0.0, 1.0);
        t.graph.shading.clearcoat_roughness = constant_input(
            t,
            shader,
            coat_rough,
            coat_rough_default,
            "the clearcoat roughness",
        )
        .clamp(0.0, 1.0);
    }
    let aniso = constant_input(t, shader, spec.anisotropy, 0.0, "the anisotropy");
    if aniso != 0.0 {
        t.graph.shading.anisotropy = aniso.clamp(-1.0, 1.0);
        if let Some(rotation) = spec.rotation {
            // 0..1 is a half turn: the lobe is symmetric.
            t.graph.shading.anisotropy_rotation =
                constant_input(t, shader, rotation, 0.0, "the anisotropy rotation") * std::f32::consts::PI;
        } else if shader
            .input("geometry_tangent")
            .is_some_and(super::doc::Input::is_connected)
        {
            t.lossy(&at("geometry_tangent"), "the anisotropy follows the mesh tangent");
        }
    }

    for (name, default, what) in spec.dropped {
        // A connected dropped input is reported without resolving it: resolving emits its nodes
        // and declares its textures and parameters, which the material would then cook (and
        // fail on when the file is missing) and bind for an input it never reads.
        let set = match shader.input(name) {
            Some(input) if input.is_connected() => true,
            _ => t.non_default(0, shader, name, *default),
        };
        if set {
            t.lossy(&at(name), *what);
        }
    }
    for name in spec.ignored_connections {
        if shader.input(name).is_some_and(super::doc::Input::is_connected) {
            t.lossy(
                &at(name),
                "this connection is ignored: the mesh's own frame is used",
            );
        }
    }

    // Shader inputs take the parameter slots the node graphs left, in this order.
    for d in deferred {
        if let Some(param) = t.param_decl(&d.name, &d.source, d.ty, d.value, d.color) {
            t.graph.nodes.insert(d.node, regia_matgraph::Node::Param(param));
        }
    }

    if t.settings.shading_model == ShadingModel::Unlit {
        t.graph.shading.unlit = true;
        t.graph.shading.clearcoat = 0.0;
        t.graph.shading.anisotropy = 0.0;
    }
    let category = shader.category.clone();
    t.report.mapped.push(super::MappedNode {
        node: shader.name.clone(),
        category,
        mapped_to: "surface outputs".into(),
    });
}
