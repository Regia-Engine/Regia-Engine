// SPDX-License-Identifier: MPL-2.0
//! The `.matgraph.ron` format (v4): parameters, textures, static switches, material functions,
//! a node map and the surface outputs; and the `.matfunc.ron` format (v3) of a material
//! function kept in its own file.
//!
//! Frozen-format rules (`docs/specs/12-assets.md` § 4a) apply from v1: the root carries
//! `format_version`, a loader rejects a newer version before deserialising the body, older
//! versions run the registered migrator chain ([`builtin_migrations`]), and
//! [`matgraph_schema_fingerprint`] is hashed by `cargo xtask check-formats` so a shape change
//! without a version bump fails CI. The format is documented in `docs/formats/matgraph-ron.md`.
//!
//! v2 added (all optional, so a v1 file reads unchanged and the `1 → 2` step only bumps the
//! version): `switches`, `functions`, the `TexCoord1`, `ObjectPosition`, `Sin`, `Cos`, `Frac`,
//! `Switch`, `Call` and `Input` nodes, the `world_position_offset` output and
//! `shading.max_vertex_offset`.
//!
//! v3 added the `Wind` node (the scene wind the renderer writes beside the material clock). It
//! only adds a node kind, so a v2 file reads unchanged and the `2 → 3` step only bumps the
//! version. The node set is part of the `.matfunc.ron` shape too, so that format moved to v2 in
//! the same change (its `1 → 2` step only bumps the version).
//!
//! v4 (`.matfunc.ron` v3) added the `Noise` node (ADR-2318). Nothing else changed, so the
//! `3 → 4` (and `.matfunc.ron` `2 → 3`) steps only bump the version.
//!
//! v5 (`.matfunc.ron` v4) adds the `Code` node (ADR-7852): typed inputs, declared texture inputs,
//! one declared output type and a WGSL function body ([`crate::code`]). It only adds a node kind
//! and its [`CodeInput`] record, so the `4 → 5` (and `.matfunc.ron` `3 → 4`) steps only bump the
//! version.
//!
//! v5 also folds in the parameter metadata (ADR-8105 (b)): [`ParamDecl::category`],
//! [`ParamDecl::tooltip`] and [`ParamDecl::range`], all optional with the v4 meaning as their
//! default, so the `4 → 5` step is unchanged; compile ignores them.

use std::collections::BTreeMap;

use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};

/// Current `.matgraph.ron` version.
pub const MATGRAPH_FORMAT_VERSION: u32 = 5;

/// Current `.matfunc.ron` version.
pub const MATFUNC_FORMAT_VERSION: u32 = 4;

/// Errors reading or writing a material graph file.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    /// The file is newer than this engine.
    #[error("material graph format v{found} is newer than this engine (v{expected}); upgrade the engine")]
    NewerVersion {
        /// Version in the file.
        found: u32,
        /// Newest version this build reads.
        expected: u32,
    },
    /// The RON did not parse.
    #[error("material graph RON: {0}")]
    Ron(String),
    /// The migrator chain failed.
    #[error("material graph migration: {0}")]
    Migration(String),
    /// A material function file could not be resolved.
    #[error("material function {0}")]
    Function(String),
    /// A parameter's metadata (v5 fold: its range) is invalid.
    #[error("material graph {0}")]
    Param(String),
}

/// A material graph: the asset an artist or agent authors.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialGraph {
    /// Format version ([`MATGRAPH_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Parameters exposed to instances, in slot order (at most 15).
    #[serde(default)]
    pub params: Vec<ParamDecl>,
    /// Textures the graph samples, in slot order (at most 7).
    #[serde(default)]
    pub textures: Vec<TextureDecl>,
    /// Static switches (v2): compile-time booleans that pick a branch of each `Switch` node.
    /// An instance may override them; each combination compiles to its own code.
    #[serde(default)]
    pub switches: Vec<SwitchDecl>,
    /// Material functions (v2): sub-graphs that `Call` nodes expand, by name. Inline, or a
    /// `.matfunc.ron` file the loader resolves ([`MaterialGraph::resolve_function_files`]).
    #[serde(default)]
    pub functions: BTreeMap<String, FunctionSource>,
    /// Nodes by name. Inputs refer to other nodes by name.
    pub nodes: BTreeMap<String, Node>,
    /// Which node drives each surface output. Unset outputs keep the contract default.
    pub output: SurfaceOutputs,
    /// Lighting response and render state (not part of the generated code).
    #[serde(default)]
    pub shading: Shading,
}

/// A parameter: name and default value (its type is the value's type), plus the editor-only
/// metadata of the v5 fold (ADR-8105 (b)): a category, a tooltip and a range. Compile ignores
/// the metadata, so the generated WGSL and the cooked material are the same with or without it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamDecl {
    /// Name used by `Param` nodes and instance overrides.
    pub name: String,
    /// Default value.
    pub value: ParamValue,
    /// Category the parameters panel groups this parameter under (v5 fold). Empty: ungrouped,
    /// listed before every category. Categories show in order of first appearance.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub category: String,
    /// Tooltip text the parameters panel and the instance editor show (v5 fold).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tooltip: String,
    /// Range the editor clamps edits and instance overrides to (v5 fold); `None` = unbounded.
    /// The runtime material API does not clamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ParamRange>,
}

impl ParamDecl {
    /// A parameter with no category, tooltip or range.
    #[must_use]
    pub fn new(name: impl Into<String>, value: ParamValue) -> Self {
        Self {
            name: name.into(),
            value,
            category: String::new(),
            tooltip: String::new(),
            range: None,
        }
    }

    /// Why this parameter's metadata is invalid, or `None`: a range must have finite bounds,
    /// `min < max`, a step in `0..=max - min`, and the default inside it (per component).
    #[must_use]
    pub fn metadata_error(&self) -> Option<String> {
        let range = self.range?;
        if let Some(message) = range.error() {
            return Some(message);
        }
        if !range.contains_value(self.value) {
            return Some(format!(
                "default is outside the range {}..={}",
                range.min, range.max
            ));
        }
        None
    }

    /// `value` clamped (and snapped to the step) by [`ParamDecl::range`]; unchanged without a
    /// range.
    #[must_use]
    pub fn clamp(&self, value: ParamValue) -> ParamValue {
        match self.range {
            Some(range) => range.clamp_value(value),
            None => value,
        }
    }
}

/// The range of a parameter (v5 fold): the editor clamps edits to `min..=max` and snaps them to
/// `step` from `min` (`step` 0 = continuous). Applies to every component of a vector or colour.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamRange {
    /// Lowest value.
    pub min: f32,
    /// Highest value.
    pub max: f32,
    /// Step from `min` the editor snaps to; 0 = continuous.
    #[serde(default)]
    pub step: f32,
}

impl ParamRange {
    /// Why the range is invalid, or `None`.
    #[must_use]
    pub fn error(self) -> Option<String> {
        if !(self.min.is_finite() && self.max.is_finite() && self.step.is_finite()) {
            return Some("range bounds and step must be finite".to_string());
        }
        if self.min >= self.max {
            return Some(format!("range min {} is not below max {}", self.min, self.max));
        }
        if self.step < 0.0 || self.step > self.max - self.min {
            return Some(format!(
                "range step {} must be in 0..={}",
                self.step,
                self.max - self.min
            ));
        }
        None
    }

    /// One component clamped to the range and snapped to the step (a non-finite input lands on
    /// `min`). An invalid range ([`ParamRange::error`]) leaves `x` as it is.
    #[must_use]
    pub fn clamp(self, x: f32) -> f32 {
        if self.error().is_some() {
            return x;
        }
        if !x.is_finite() {
            return self.min;
        }
        let mut v = x.clamp(self.min, self.max);
        if self.step > 0.0 {
            v = (self.min + ((v - self.min) / self.step).round() * self.step).clamp(self.min, self.max);
        }
        v
    }

    /// Every component of `value` clamped ([`ParamRange::clamp`]).
    #[must_use]
    pub fn clamp_value(self, value: ParamValue) -> ParamValue {
        let c = |x: f32| self.clamp(x);
        match value {
            ParamValue::Float(x) => ParamValue::Float(c(x)),
            ParamValue::Vec2(v) => ParamValue::Vec2(v.map(c)),
            ParamValue::Vec3(v) => ParamValue::Vec3(v.map(c)),
            ParamValue::Vec4(v) => ParamValue::Vec4(v.map(c)),
            ParamValue::Color(v) => ParamValue::Color(v.map(c)),
        }
    }

    /// Whether every component of `value` lies in `min..=max`.
    #[must_use]
    pub fn contains_value(self, value: ParamValue) -> bool {
        let n = match value {
            ParamValue::Float(_) => 1,
            ParamValue::Vec2(_) => 2,
            ParamValue::Vec3(_) | ParamValue::Color(_) => 3,
            ParamValue::Vec4(_) => 4,
        };
        value.to_vec4()[..n]
            .iter()
            .all(|x| (self.min..=self.max).contains(x))
    }
}

/// A typed parameter value.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ParamValue {
    /// One float.
    Float(f32),
    /// Two floats.
    Vec2([f32; 2]),
    /// Three floats.
    Vec3([f32; 3]),
    /// Four floats.
    Vec4([f32; 4]),
    /// Linear RGB (a `Vec3` an editor shows as a colour).
    Color([f32; 3]),
}

impl ParamValue {
    /// The value type a `Param` node of this value produces.
    #[must_use]
    pub fn value_type(self) -> ValueType {
        match self {
            Self::Float(_) => ValueType::Float,
            Self::Vec2(_) => ValueType::Vec2,
            Self::Vec3(_) | Self::Color(_) => ValueType::Vec3,
            Self::Vec4(_) => ValueType::Vec4,
        }
    }

    /// The value as a zero-padded `vec4`.
    #[must_use]
    pub fn to_vec4(self) -> [f32; 4] {
        match self {
            Self::Float(x) => [x, 0.0, 0.0, 0.0],
            Self::Vec2([x, y]) => [x, y, 0.0, 0.0],
            Self::Vec3([x, y, z]) | Self::Color([x, y, z]) => [x, y, z, 0.0],
            Self::Vec4(v) => v,
        }
    }

    /// Whether every component is finite.
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.to_vec4().iter().all(|c| c.is_finite())
    }
}

/// A static switch: a compile-time boolean.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchDecl {
    /// Name used by `Switch` nodes and instance overrides.
    pub name: String,
    /// Value when no instance overrides it.
    pub default: bool,
}

/// Where a material function's definition lives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FunctionSource {
    /// Defined in the graph file.
    Inline(MaterialFunction),
    /// A `.matfunc.ron` file (project-relative path), resolved by the loader or the cook.
    File(String),
}

/// One input of a material function.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionInput {
    /// Name used by `Input` nodes inside the function and by `Call` arguments.
    pub name: String,
    /// The argument's type (checked at each call).
    pub ty: ValueType,
}

/// A material function (sub-graph): named inputs, a node map and named outputs. A `Call`
/// node expands it in place, so it costs nothing at run time. Inside it, `Input("x")` reads an
/// argument; `Param` and texture names refer to the calling graph's declarations; `Call` may
/// use the calling graph's other functions (no recursion).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialFunction {
    /// Format version ([`MATFUNC_FORMAT_VERSION`]); required in a `.matfunc.ron` file, optional
    /// inline.
    #[serde(default = "matfunc_version")]
    pub format_version: u32,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Inputs, in order.
    #[serde(default)]
    pub inputs: Vec<FunctionInput>,
    /// Nodes by name.
    pub nodes: BTreeMap<String, Node>,
    /// Outputs: output name → node name.
    pub outputs: BTreeMap<String, String>,
}

fn matfunc_version() -> u32 {
    MATFUNC_FORMAT_VERSION
}

impl MaterialFunction {
    /// Parses a `.matfunc.ron` document, rejecting a newer version.
    ///
    /// # Errors
    ///
    /// [`FormatError`] for a newer version or bad RON.
    pub fn from_ron(text: &str) -> Result<Self, FormatError> {
        let found = version::peek_ron_version(text).map_err(FormatError::Ron)?;
        if found > MATFUNC_FORMAT_VERSION {
            return Err(FormatError::NewerVersion {
                found,
                expected: MATFUNC_FORMAT_VERSION,
            });
        }
        let mut function: Self = ron::from_str(text).map_err(|e| FormatError::Ron(e.to_string()))?;
        if function.format_version < MATFUNC_FORMAT_VERSION {
            function.format_version = version::migrate(
                &mut function,
                found,
                builtin_function_migrations(),
                MATFUNC_FORMAT_VERSION,
            )
            .map_err(|e| FormatError::Migration(e.to_string()))?;
        }
        Ok(function)
    }

    /// Serialises to pretty RON.
    ///
    /// # Errors
    ///
    /// [`FormatError::Ron`] if serialisation fails.
    pub fn to_ron(&self) -> Result<String, FormatError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| FormatError::Ron(e.to_string()))
    }
}

/// `.matfunc.ron` v1 → v2: v2 only adds the `Wind` node kind (matgraph v3), so a v1 function
/// reads unchanged and the step only moves the version.
fn matfunc_v1_to_v2(_function: &mut MaterialFunction) -> Result<(), String> {
    Ok(())
}

/// The built-in migrator chain for `.matfunc.ron` (the matching row lives in
/// `regia upgrade-project`).
#[must_use]
pub fn builtin_function_migrations() -> &'static [VersionStep<MaterialFunction>] {
    &[
        VersionStep {
            from: 1,
            to: 2,
            migrate: matfunc_v1_to_v2,
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: matfunc_v2_to_v3,
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: matfunc_v3_to_v4,
        },
    ]
}

/// `.matfunc.ron` v2 → v3: v3 only adds the `Noise` node (matgraph v4, ADR-2318); a v2
/// function reads unchanged.
fn matfunc_v2_to_v3(_function: &mut MaterialFunction) -> Result<(), String> {
    Ok(())
}

/// `.matfunc.ron` v3 → v4: v4 only adds the `Code` node (matgraph v5, ADR-7852); a v3 function
/// reads unchanged.
fn matfunc_v3_to_v4(_function: &mut MaterialFunction) -> Result<(), String> {
    Ok(())
}

/// The `from` versions of [`builtin_function_migrations`] (checked by `cargo xtask check-formats`).
#[must_use]
pub fn matfunc_migration_sources() -> Vec<u32> {
    builtin_function_migrations().iter().map(|s| s.from).collect()
}

/// The serde fingerprint of `.matfunc.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn matfunc_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<MaterialFunction>(),
        serde_field_names::<FunctionInput>(),
        serde_field_names::<CodeInput>(),
        serde_variant_names::<ValueType>(),
        serde_variant_names::<Node>(),
        serde_variant_names::<Channel>(),
        serde_variant_names::<NormalBlend>(),
    ])
}

/// A texture the graph samples.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextureDecl {
    /// Name used by sampling nodes and instance overrides.
    pub name: String,
    /// Default image (project-relative path; the host resolves it to pixels).
    #[serde(default)]
    pub path: String,
    /// How the bytes are decoded.
    pub color_space: ColorSpace,
}

/// Texture colour space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorSpace {
    /// sRGB-encoded colour (base colour, emissive): the sampler decodes to linear.
    Srgb,
    /// Linear data (normals, masks, heights, roughness).
    Linear,
}

/// The value type flowing along a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValueType {
    /// `f32`.
    Float,
    /// `vec2<f32>`.
    Vec2,
    /// `vec3<f32>`.
    Vec3,
    /// `vec4<f32>`.
    Vec4,
}

impl ValueType {
    /// Component count.
    #[must_use]
    pub fn width(self) -> usize {
        match self {
            Self::Float => 1,
            Self::Vec2 => 2,
            Self::Vec3 => 3,
            Self::Vec4 => 4,
        }
    }

    /// The type with `width` components.
    #[must_use]
    pub fn of_width(width: usize) -> Option<Self> {
        Some(match width {
            1 => Self::Float,
            2 => Self::Vec2,
            3 => Self::Vec3,
            4 => Self::Vec4,
            _ => return None,
        })
    }

    /// WGSL type name.
    #[must_use]
    pub fn wgsl(self) -> &'static str {
        match self {
            Self::Float => "f32",
            Self::Vec2 => "vec2<f32>",
            Self::Vec3 => "vec3<f32>",
            Self::Vec4 => "vec4<f32>",
        }
    }
}

/// One texture channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Channel {
    /// Red.
    R,
    /// Green.
    G,
    /// Blue.
    B,
    /// Alpha.
    A,
}

impl Channel {
    /// WGSL swizzle letter.
    #[must_use]
    pub fn letter(self) -> char {
        match self {
            Self::R => 'r',
            Self::G => 'g',
            Self::B => 'b',
            Self::A => 'a',
        }
    }
}

/// How [`Node::BlendNormals`] combines two tangent-space normals (Barré-Brisebois & Hill,
/// "Blending in Detail", 2012).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NormalBlend {
    /// Reoriented normal mapping: the detail is rotated onto the base normal.
    Rnm,
    /// Partial-derivative normal blend: add `xy` and preserve the base `z`.
    Udn,
    /// Whiteout: `xy` add, `z` multiply.
    Whiteout,
}

/// One typed input of a [`Node::Code`] node (v5): the name the code reads, its type and the node
/// wired to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeInput {
    /// The WGSL parameter name the code reads (an identifier, not starting with `mg_`).
    pub name: String,
    /// The parameter's type; the wired node must have exactly this type.
    pub ty: ValueType,
    /// The node wired to it (`""` while unwired, which the compiler reports on the node).
    #[serde(default)]
    pub node: String,
}

/// One node. Inputs are node names; parameters and constants are nodes too.
///
/// RON: unit variants are bare (`TexCoord`), single-input ops are tuples (`Saturate("x")`),
/// two-input ops are pairs (`Mul("a", "b")`), the rest are structs (`Lerp(a: "x", b: "y", t: "w")`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Node {
    // -- Inputs -------------------------------------------------------------
    /// Mesh UV 0 (`Vec2`).
    TexCoord,
    /// Vertex colour (`Vec4`).
    VertexColor,
    /// World-space position (`Vec3`).
    WorldPosition,
    /// Geometric world normal, facing the viewer (`Vec3`).
    WorldNormal,
    /// Unit vector toward the eye (`Vec3`).
    ViewDirection,
    /// Eye distance in metres (`Float`).
    CameraDistance,
    /// Seconds, written by the host into the built-in parameter slot (`Float`).
    Time,
    /// A declared parameter (its declared type).
    Param(String),
    /// Constant `Float`.
    Float(f32),
    /// Constant `Vec2`.
    Vec2([f32; 2]),
    /// Constant `Vec3`.
    Vec3([f32; 3]),
    /// Constant `Vec4`.
    Vec4([f32; 4]),

    // -- Textures -----------------------------------------------------------
    /// Sample a declared texture at `uv` (`Vec4`). With `lod`, sample that mip explicitly.
    Sample {
        /// Texture name.
        texture: String,
        /// `Vec2` UV node.
        uv: String,
        /// Optional `Float` mip level node.
        #[serde(default)]
        lod: Option<String>,
    },
    /// Triplanar projection of a texture in world space (`Vec4`), blended by the geometric
    /// normal raised to `sharpness`.
    Triplanar {
        /// Texture name.
        texture: String,
        /// `Float` node: tiles per metre.
        scale: String,
        /// Blend sharpness exponent (≥ 1).
        #[serde(default = "default_sharpness")]
        sharpness: f32,
    },
    /// glTF `KHR_texture_transform` on a UV (`Vec2`): scale, then rotate, then offset.
    UvTransform {
        /// `Vec2` UV node.
        uv: String,
        /// Scale.
        #[serde(default = "unit2")]
        scale: [f32; 2],
        /// Offset.
        #[serde(default)]
        offset: [f32; 2],
        /// Rotation in radians.
        #[serde(default)]
        rotation: f32,
    },
    /// Parallax occlusion mapping (`Vec2`): marches a height channel along the view ray.
    /// Height 1 is the geometric surface. Honours the profile's parallax gate.
    Parallax {
        /// Height texture name.
        texture: String,
        /// Height channel.
        channel: Channel,
        /// `Vec2` UV node.
        uv: String,
        /// `Float` node: depth in UV units.
        scale: String,
        /// Steps when the view faces the surface.
        min_steps: f32,
        /// Steps at grazing angles (capped at 16).
        max_steps: f32,
    },

    // -- Math ---------------------------------------------------------------
    /// `a + b` (a `Float` broadcasts to the other side's width).
    Add(String, String),
    /// `a - b`.
    Sub(String, String),
    /// `a * b`.
    Mul(String, String),
    /// `a / b`.
    Div(String, String),
    /// Component-wise minimum.
    Min(String, String),
    /// Component-wise maximum.
    Max(String, String),
    /// `pow(a, b)`.
    Pow(String, String),
    /// Dot product of two equal-width vectors (`Float`).
    Dot(String, String),
    /// `mix(a, b, t)`; `t` is a `Float` or the width of `a`.
    Lerp {
        /// Value at `t = 0`.
        a: String,
        /// Value at `t = 1`.
        b: String,
        /// Blend factor.
        t: String,
    },
    /// Hermite step between two edges.
    Smoothstep {
        /// Lower edge.
        edge0: String,
        /// Upper edge.
        edge1: String,
        /// Input.
        x: String,
    },
    /// Clamp to `0..1`.
    Saturate(String),
    /// `1 - x`.
    OneMinus(String),
    /// Absolute value.
    Abs(String),
    /// Unit vector.
    Normalize(String),
    /// Vector length (`Float`).
    Length(String),
    /// Pick or reorder components: `"rgb"`, `"a"`, `"xy"`, `"zyx"` (one alphabet per swizzle).
    Swizzle {
        /// Input node.
        input: String,
        /// Component letters.
        channels: String,
    },
    /// Concatenate components (`Vec2` from two floats, `Vec4` from `Vec3` + `Float`, ...).
    Append(String, String),

    // -- Shading ------------------------------------------------------------
    /// Schlick-style facing ratio `(1 - saturate(N·V))^power` on the geometric normal (`Float`).
    Fresnel {
        /// `Float` exponent node (5 is Schlick's).
        power: String,
    },
    /// Height-aware blend of two layers (Mishkinis 2013): the higher surface wins near the
    /// transition, `depth` widens the overlap.
    HeightBlend {
        /// Layer at `t = 0`.
        a: String,
        /// Layer at `t = 1`.
        b: String,
        /// `Float` height of `a` (0..1).
        height_a: String,
        /// `Float` height of `b` (0..1).
        height_b: String,
        /// `Float` blend weight toward `b`.
        t: String,
        /// Transition depth (> 0).
        #[serde(default = "default_blend_depth")]
        depth: f32,
    },
    /// Decode a normal-map sample: `xyz * 2 - 1` (`Vec3`).
    UnpackNormal(String),
    /// Scale a tangent-space normal's `xy` by a `Float` strength (`Vec3`).
    NormalStrength {
        /// `Vec3` tangent-space normal.
        normal: String,
        /// `Float` strength.
        strength: String,
    },
    /// Combine a base and a detail tangent-space normal (`Vec3`, unit length).
    BlendNormals {
        /// Base normal (unpacked).
        base: String,
        /// Detail normal (unpacked).
        detail: String,
        /// Blend method.
        mode: NormalBlend,
    },

    // -- v2 -----------------------------------------------------------------
    /// Mesh UV 1 (`Vec2`): the second UV set (the first when the mesh has none).
    TexCoord1,
    /// World-space origin of the object (`Vec3`): per-instance variation, wind phase.
    ObjectPosition,
    /// Sine (radians).
    Sin(String),
    /// Cosine (radians).
    Cos(String),
    /// Fractional part, `x - floor(x)`.
    Frac(String),
    /// Static switch: `on` when the switch is true, else `off`. Resolved at compile time; only
    /// the chosen branch is emitted.
    Switch {
        /// Switch name (declared in `switches`).
        switch: String,
        /// Node when true.
        on: String,
        /// Node when false.
        off: String,
    },
    /// Material function call: expands the function with `args` (function input → node) and
    /// yields `output` (optional when the function has one output).
    Call {
        /// Function name (a key of `functions`).
        function: String,
        /// Arguments: input name → node name.
        #[serde(default)]
        args: BTreeMap<String, String>,
        /// Output name.
        #[serde(default)]
        output: Option<String>,
    },
    /// A function input (only inside a material function).
    Input(String),

    // -- v3 -----------------------------------------------------------------
    /// The scene wind in meters per second on world X and Z (`Vec2`), written by the host
    /// beside the material clock; zero when the host sets none. Reading it makes the graph
    /// time-reading, so the renderer writes the clock and the wind into it.
    Wind,

    // -- v4 -----------------------------------------------------------------
    /// Fractal gradient noise (`Float`, 0..1): `octaves` layers of 3D gradient noise at
    /// doubling frequency and halving amplitude, sampled at `input` (`Float`, `Vec2` or `Vec3`;
    /// narrower positions are padded with zeros). One unit of `input` is one noise cell.
    /// The lattice hash is integer arithmetic, so every device picks the same gradients (the
    /// blend is float math, within the usual float tolerance).
    Noise {
        /// Position node (a UV, a world position, ...).
        input: String,
        /// Layers summed, 1–8.
        #[serde(default = "default_octaves")]
        octaves: u32,
    },

    // -- v5 -----------------------------------------------------------------
    /// A WGSL code node (ADR-7852, [`crate::code`]): the body of a function whose parameters are
    /// `inputs` (in order), then each of `textures` as a `texture_2d<f32>` of its name and the
    /// graph's sampler as `material_sampler`, returning `output`. The compiler inlines it as a
    /// uniquely named function. The body may not declare bindings, module-scope variables or
    /// entry points; `#include "name.wgsl"` lines pull in the project's `shaders/include/` files.
    /// naga validates it before any pipeline is built; a node that fails shades magenta in the
    /// editor and refuses to cook.
    Code {
        /// Typed inputs, in parameter order.
        #[serde(default)]
        inputs: Vec<CodeInput>,
        /// Declared graph textures the code may sample, in parameter order.
        #[serde(default)]
        textures: Vec<String>,
        /// The value type the code returns.
        output: ValueType,
        /// The function body (WGSL statements ending in a `return`).
        code: String,
    },
}

fn default_octaves() -> u32 {
    1
}

fn default_sharpness() -> f32 {
    4.0
}

fn default_blend_depth() -> f32 {
    0.2
}

fn unit2() -> [f32; 2] {
    [1.0, 1.0]
}

impl Node {
    /// Names of the nodes this node reads, in input order.
    #[must_use]
    pub fn inputs(&self) -> Vec<&str> {
        match self {
            Self::TexCoord
            | Self::TexCoord1
            | Self::ObjectPosition
            | Self::VertexColor
            | Self::WorldPosition
            | Self::WorldNormal
            | Self::ViewDirection
            | Self::CameraDistance
            | Self::Time
            | Self::Wind
            | Self::Param(_)
            | Self::Input(_)
            | Self::Float(_)
            | Self::Vec2(_)
            | Self::Vec3(_)
            | Self::Vec4(_) => Vec::new(),
            Self::Sample { uv, lod, .. } => {
                let mut out = vec![uv.as_str()];
                if let Some(lod) = lod {
                    out.push(lod.as_str());
                }
                out
            }
            Self::Triplanar { scale, .. } => vec![scale.as_str()],
            Self::UvTransform { uv, .. } => vec![uv.as_str()],
            Self::Parallax { uv, scale, .. } => vec![uv.as_str(), scale.as_str()],
            Self::Add(a, b)
            | Self::Sub(a, b)
            | Self::Mul(a, b)
            | Self::Div(a, b)
            | Self::Min(a, b)
            | Self::Max(a, b)
            | Self::Pow(a, b)
            | Self::Dot(a, b)
            | Self::Append(a, b) => vec![a.as_str(), b.as_str()],
            Self::Lerp { a, b, t } => vec![a.as_str(), b.as_str(), t.as_str()],
            Self::Smoothstep { edge0, edge1, x } => vec![edge0.as_str(), edge1.as_str(), x.as_str()],
            Self::Saturate(x)
            | Self::OneMinus(x)
            | Self::Abs(x)
            | Self::Normalize(x)
            | Self::Length(x)
            | Self::Sin(x)
            | Self::Cos(x)
            | Self::Frac(x)
            | Self::UnpackNormal(x) => vec![x.as_str()],
            Self::Swizzle { input, .. } | Self::Noise { input, .. } => vec![input.as_str()],
            Self::Switch { on, off, .. } => vec![on.as_str(), off.as_str()],
            Self::Call { args, .. } => args.values().map(String::as_str).collect(),
            Self::Code { inputs, .. } => inputs.iter().map(|i| i.node.as_str()).collect(),
            Self::Fresnel { power } => vec![power.as_str()],
            Self::HeightBlend {
                a,
                b,
                height_a,
                height_b,
                t,
                ..
            } => vec![
                a.as_str(),
                b.as_str(),
                height_a.as_str(),
                height_b.as_str(),
                t.as_str(),
            ],
            Self::NormalStrength { normal, strength } => vec![normal.as_str(), strength.as_str()],
            Self::BlendNormals { base, detail, .. } => vec![base.as_str(), detail.as_str()],
        }
    }

    /// Rewrites every input name in place (lowering of switches and functions).
    pub fn map_inputs(&mut self, mut f: impl FnMut(&mut String)) {
        match self {
            Self::TexCoord
            | Self::TexCoord1
            | Self::ObjectPosition
            | Self::VertexColor
            | Self::WorldPosition
            | Self::WorldNormal
            | Self::ViewDirection
            | Self::CameraDistance
            | Self::Time
            | Self::Wind
            | Self::Param(_)
            | Self::Input(_)
            | Self::Float(_)
            | Self::Vec2(_)
            | Self::Vec3(_)
            | Self::Vec4(_) => {}
            Self::Sample { uv, lod, .. } => {
                f(uv);
                if let Some(lod) = lod {
                    f(lod);
                }
            }
            Self::Triplanar { scale, .. } => f(scale),
            Self::UvTransform { uv, .. } => f(uv),
            Self::Parallax { uv, scale, .. } => {
                f(uv);
                f(scale);
            }
            Self::Add(a, b)
            | Self::Sub(a, b)
            | Self::Mul(a, b)
            | Self::Div(a, b)
            | Self::Min(a, b)
            | Self::Max(a, b)
            | Self::Pow(a, b)
            | Self::Dot(a, b)
            | Self::Append(a, b) => {
                f(a);
                f(b);
            }
            Self::Lerp { a, b, t } => {
                f(a);
                f(b);
                f(t);
            }
            Self::Smoothstep { edge0, edge1, x } => {
                f(edge0);
                f(edge1);
                f(x);
            }
            Self::Saturate(x)
            | Self::OneMinus(x)
            | Self::Abs(x)
            | Self::Normalize(x)
            | Self::Length(x)
            | Self::Sin(x)
            | Self::Cos(x)
            | Self::Frac(x)
            | Self::UnpackNormal(x) => f(x),
            Self::Swizzle { input, .. } | Self::Noise { input, .. } => f(input),
            Self::Fresnel { power } => f(power),
            Self::HeightBlend {
                a,
                b,
                height_a,
                height_b,
                t,
                ..
            } => {
                f(a);
                f(b);
                f(height_a);
                f(height_b);
                f(t);
            }
            Self::NormalStrength { normal, strength } => {
                f(normal);
                f(strength);
            }
            Self::BlendNormals { base, detail, .. } => {
                f(base);
                f(detail);
            }
            Self::Switch { on, off, .. } => {
                f(on);
                f(off);
            }
            Self::Call { args, .. } => args.values_mut().for_each(f),
            Self::Code { inputs, .. } => inputs.iter_mut().for_each(|i| f(&mut i.node)),
        }
    }

    /// The texture this node samples, if any.
    #[must_use]
    pub fn texture(&self) -> Option<&str> {
        match self {
            Self::Sample { texture, .. }
            | Self::Triplanar { texture, .. }
            | Self::Parallax { texture, .. } => Some(texture),
            _ => None,
        }
    }

    /// Short kind name (diagnostics, the agent node list).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TexCoord => "TexCoord",
            Self::VertexColor => "VertexColor",
            Self::WorldPosition => "WorldPosition",
            Self::WorldNormal => "WorldNormal",
            Self::ViewDirection => "ViewDirection",
            Self::CameraDistance => "CameraDistance",
            Self::Time => "Time",
            Self::Param(_) => "Param",
            Self::Float(_) => "Float",
            Self::Vec2(_) => "Vec2",
            Self::Vec3(_) => "Vec3",
            Self::Vec4(_) => "Vec4",
            Self::Sample { .. } => "Sample",
            Self::Triplanar { .. } => "Triplanar",
            Self::UvTransform { .. } => "UvTransform",
            Self::Parallax { .. } => "Parallax",
            Self::Add(..) => "Add",
            Self::Sub(..) => "Sub",
            Self::Mul(..) => "Mul",
            Self::Div(..) => "Div",
            Self::Min(..) => "Min",
            Self::Max(..) => "Max",
            Self::Pow(..) => "Pow",
            Self::Dot(..) => "Dot",
            Self::Lerp { .. } => "Lerp",
            Self::Smoothstep { .. } => "Smoothstep",
            Self::Saturate(_) => "Saturate",
            Self::OneMinus(_) => "OneMinus",
            Self::Abs(_) => "Abs",
            Self::Normalize(_) => "Normalize",
            Self::Length(_) => "Length",
            Self::Swizzle { .. } => "Swizzle",
            Self::Append(..) => "Append",
            Self::Fresnel { .. } => "Fresnel",
            Self::HeightBlend { .. } => "HeightBlend",
            Self::UnpackNormal(_) => "UnpackNormal",
            Self::NormalStrength { .. } => "NormalStrength",
            Self::BlendNormals { .. } => "BlendNormals",
            Self::TexCoord1 => "TexCoord1",
            Self::ObjectPosition => "ObjectPosition",
            Self::Sin(_) => "Sin",
            Self::Cos(_) => "Cos",
            Self::Frac(_) => "Frac",
            Self::Switch { .. } => "Switch",
            Self::Call { .. } => "Call",
            Self::Input(_) => "Input",
            Self::Wind => "Wind",
            Self::Noise { .. } => "Noise",
            Self::Code { .. } => "Code",
        }
    }
}

/// The surface outputs (`material_surface` contract, `specs/13` § 7).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceOutputs {
    /// `Vec3` linear base colour (default 0.8 grey).
    #[serde(default)]
    pub base_color: Option<String>,
    /// `Float` coverage (default 1).
    #[serde(default)]
    pub alpha: Option<String>,
    /// `Float` metallic (default 0).
    #[serde(default)]
    pub metallic: Option<String>,
    /// `Float` perceptual roughness (default 0.5).
    #[serde(default)]
    pub roughness: Option<String>,
    /// `Vec3` normal in [`Self::normal_space`] (default: the geometric normal).
    #[serde(default)]
    pub normal: Option<String>,
    /// Space of [`Self::normal`].
    #[serde(default)]
    pub normal_space: NormalSpace,
    /// `Float` ambient occlusion (default 1).
    #[serde(default)]
    pub occlusion: Option<String>,
    /// `Vec3` emitted radiance, nits (default 0).
    #[serde(default)]
    pub emissive: Option<String>,
    /// `Vec3` world position offset in metres (v2, default none): evaluated per vertex and
    /// applied in every pass (forward, depth, shadow, motion vectors). Eye-dependent inputs
    /// and parallax are not available to it. Set `shading.max_vertex_offset` to the largest
    /// offset so culling keeps the displaced mesh.
    #[serde(default)]
    pub world_position_offset: Option<String>,
}

/// Space of the normal output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NormalSpace {
    /// Tangent space (`+Z` = the geometric normal); needs mesh tangents.
    #[default]
    Tangent,
    /// World space.
    World,
}

/// Alpha mode of a graph material (`specs/13` § 6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum GraphAlphaMode {
    /// Opaque.
    #[default]
    Opaque,
    /// Alpha test against a cutoff.
    Mask(f32),
    /// Alpha blend.
    Blend,
    /// Premultiplied blend.
    Premultiplied,
    /// Additive.
    Additive,
}

/// Lighting response and render state. These drive the shared lighting code, not the graph.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Shading {
    /// Alpha mode.
    pub alpha_mode: GraphAlphaMode,
    /// Skip lighting. The base colour is linear. The forward pass sRGB-encodes it on a linear
    /// framebuffer (`encode_srgb`, approximately `pow(c, 1/2.2)`); an sRGB framebuffer encodes in
    /// hardware (`skip_srgb_encode`). No exposure and no tonemap.
    pub unlit: bool,
    /// Clearcoat lobe weight.
    pub clearcoat: f32,
    /// Clearcoat perceptual roughness.
    pub clearcoat_roughness: f32,
    /// Anisotropy (−1..1).
    pub anisotropy: f32,
    /// Anisotropy rotation, radians.
    pub anisotropy_rotation: f32,
    /// AO-driven specular occlusion on the indirect specular.
    pub specular_occlusion: bool,
    /// Multi-scatter energy compensation.
    pub multi_scatter: bool,
    /// Largest world position offset in metres (v2): culling and shadow bounds grow by this.
    pub max_vertex_offset: f32,
}

impl Default for Shading {
    fn default() -> Self {
        Self {
            alpha_mode: GraphAlphaMode::Opaque,
            unlit: false,
            clearcoat: 0.0,
            clearcoat_roughness: 0.0,
            anisotropy: 0.0,
            anisotropy_rotation: 0.0,
            specular_occlusion: true,
            multi_scatter: true,
            max_vertex_offset: 0.0,
        }
    }
}

impl MaterialGraph {
    /// An empty graph at the current version.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            format_version: MATGRAPH_FORMAT_VERSION,
            name: name.into(),
            params: Vec::new(),
            textures: Vec::new(),
            switches: Vec::new(),
            functions: BTreeMap::new(),
            nodes: BTreeMap::new(),
            output: SurfaceOutputs::default(),
            shading: Shading::default(),
        }
    }

    /// Parses a `.matgraph.ron` document, rejecting a newer version and migrating an older one.
    ///
    /// # Errors
    ///
    /// [`FormatError`] for a newer version, bad RON, or a failed migration.
    pub fn from_ron(text: &str) -> Result<Self, FormatError> {
        let found = version::peek_ron_version(text).map_err(FormatError::Ron)?;
        if found > MATGRAPH_FORMAT_VERSION {
            return Err(FormatError::NewerVersion {
                found,
                expected: MATGRAPH_FORMAT_VERSION,
            });
        }
        let mut graph: Self = ron::from_str(text).map_err(|e| FormatError::Ron(e.to_string()))?;
        graph.migrate_to_latest()?;
        if let Some(error) = graph.param_metadata_error() {
            return Err(FormatError::Param(error));
        }
        Ok(graph)
    }

    /// The first parameter whose metadata is invalid ([`ParamDecl::metadata_error`]), as
    /// "`name`: why", or `None`. The loader refuses such a file.
    #[must_use]
    pub fn param_metadata_error(&self) -> Option<String> {
        self.params
            .iter()
            .find_map(|p| p.metadata_error().map(|e| format!("parameter `{}`: {e}", p.name)))
    }

    /// The parameter categories in order of first appearance (the empty, ungrouped category
    /// first when any parameter has none), each with the indices of its parameters in
    /// `params` order: the parameters panel's grouping.
    #[must_use]
    pub fn param_groups(&self) -> Vec<(String, Vec<usize>)> {
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        if self.params.iter().any(|p| p.category.is_empty()) {
            groups.push((String::new(), Vec::new()));
        }
        for (i, p) in self.params.iter().enumerate() {
            match groups.iter_mut().find(|(c, _)| *c == p.category) {
                Some((_, members)) => members.push(i),
                None => groups.push((p.category.clone(), vec![i])),
            }
        }
        groups
    }

    /// Serialises to pretty RON.
    ///
    /// # Errors
    ///
    /// [`FormatError::Ron`] if serialisation fails.
    pub fn to_ron(&self) -> Result<String, FormatError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| FormatError::Ron(e.to_string()))
    }

    /// Replaces every [`FunctionSource::File`] with the parsed file (`read` maps a path to its
    /// text). Returns the paths read, in name order (the cook hashes them for invalidation).
    ///
    /// # Errors
    ///
    /// [`FormatError::Function`] naming the function and path when a file cannot be read or
    /// parsed.
    pub fn resolve_function_files(
        &mut self,
        read: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Result<Vec<String>, FormatError> {
        let mut resolved = Vec::new();
        for (name, source) in &self.functions {
            let FunctionSource::File(path) = source else {
                continue;
            };
            let text = read(path).map_err(|e| FormatError::Function(format!("`{name}` ({path}): {e}")))?;
            let function = MaterialFunction::from_ron(&text)
                .map_err(|e| FormatError::Function(format!("`{name}` ({path}): {e}")))?;
            resolved.push((name.clone(), path.clone(), function));
        }
        // Commit only after every file read: a failure leaves the graph untouched, so a retry
        // reads the same paths again.
        let mut paths = Vec::new();
        for (name, path, function) in resolved {
            if let Some(source) = self.functions.get_mut(&name) {
                *source = FunctionSource::Inline(function);
            }
            paths.push(path);
        }
        Ok(paths)
    }

    /// Runs the migrator chain up to [`MATGRAPH_FORMAT_VERSION`].
    ///
    /// # Errors
    ///
    /// [`FormatError`] for a newer version or a failed step.
    pub fn migrate_to_latest(&mut self) -> Result<u32, FormatError> {
        self.migrate_with(builtin_migrations())
    }

    /// Runs `steps` up to [`MATGRAPH_FORMAT_VERSION`] (the loader passes [`builtin_migrations`]).
    ///
    /// # Errors
    ///
    /// [`FormatError`] for a newer version or a failed step.
    pub fn migrate_with(&mut self, steps: &[VersionStep<Self>]) -> Result<u32, FormatError> {
        if self.format_version > MATGRAPH_FORMAT_VERSION {
            return Err(FormatError::NewerVersion {
                found: self.format_version,
                expected: MATGRAPH_FORMAT_VERSION,
            });
        }
        if self.format_version < MATGRAPH_FORMAT_VERSION {
            let to = version::migrate(self, self.format_version, steps, MATGRAPH_FORMAT_VERSION)
                .map_err(|e| FormatError::Migration(e.to_string()))?;
            self.format_version = to;
        }
        Ok(self.format_version)
    }
}

/// v1 → v2: every v2 addition is optional with a v1-equivalent default, so the step only
/// moves the version.
fn v1_to_v2(_graph: &mut MaterialGraph) -> Result<(), String> {
    Ok(())
}

/// v2 → v3: v3 only adds the `Wind` node kind, so a v2 graph reads unchanged and the step only
/// moves the version.
fn v2_to_v3(_graph: &mut MaterialGraph) -> Result<(), String> {
    Ok(())
}

/// v3 → v4: v4 only adds the `Noise` node; a v3 file reads unchanged.
fn v3_to_v4(_graph: &mut MaterialGraph) -> Result<(), String> {
    Ok(())
}

/// v4 → v5: v5 only adds the `Code` node (ADR-7852); a v4 file reads unchanged.
fn v4_to_v5(_graph: &mut MaterialGraph) -> Result<(), String> {
    Ok(())
}

/// The built-in migrator chain for `.matgraph.ron` (the matching rows live in
/// `regia upgrade-project`).
#[must_use]
pub fn builtin_migrations() -> &'static [VersionStep<MaterialGraph>] {
    &[
        VersionStep {
            from: 1,
            to: 2,
            migrate: v1_to_v2,
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: v2_to_v3,
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: v3_to_v4,
        },
        VersionStep {
            from: 4,
            to: 5,
            migrate: v4_to_v5,
        },
    ]
}

/// The `from` versions of [`builtin_migrations`] (checked by `cargo xtask check-formats`).
#[must_use]
pub fn matgraph_migration_sources() -> Vec<u32> {
    builtin_migrations().iter().map(|s| s.from).collect()
}

/// The serde fingerprint of `.matgraph.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn matgraph_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<MaterialGraph>(),
        serde_field_names::<ParamDecl>(),
        serde_field_names::<ParamRange>(),
        serde_field_names::<TextureDecl>(),
        serde_field_names::<SwitchDecl>(),
        serde_field_names::<MaterialFunction>(),
        serde_field_names::<FunctionInput>(),
        serde_field_names::<CodeInput>(),
        serde_field_names::<SurfaceOutputs>(),
        serde_field_names::<Shading>(),
        serde_variant_names::<FunctionSource>(),
        serde_variant_names::<ValueType>(),
        serde_variant_names::<ParamValue>(),
        serde_variant_names::<ColorSpace>(),
        serde_variant_names::<Channel>(),
        serde_variant_names::<NormalBlend>(),
        serde_variant_names::<Node>(),
        serde_variant_names::<NormalSpace>(),
        serde_variant_names::<GraphAlphaMode>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"(
        format_version: 2,
        nodes: { "c": Vec3((0.2, 0.3, 0.4)) },
        output: (base_color: Some("c")),
    )"#;

    #[test]
    fn a_minimal_graph_round_trips() {
        let graph = MaterialGraph::from_ron(MINIMAL).expect("parse");
        assert_eq!(graph.format_version, MATGRAPH_FORMAT_VERSION);
        assert_eq!(graph.shading, Shading::default());
        let text = graph.to_ron().expect("write");
        assert_eq!(MaterialGraph::from_ron(&text).expect("reparse"), graph);
    }

    #[test]
    fn a_newer_version_is_rejected_before_the_body() {
        let err = MaterialGraph::from_ron("(format_version: 9, something_new: 1)").expect_err("should fail");
        assert!(matches!(err, FormatError::NewerVersion { found: 9, .. }), "{err}");
    }

    #[test]
    fn unknown_fields_are_errors() {
        let text = MINIMAL.replace("format_version: 2,", "format_version: 2, colour: 3,");
        assert!(matches!(MaterialGraph::from_ron(&text), Err(FormatError::Ron(_))));
    }

    #[test]
    fn the_migrator_chain_runs_older_files() {
        // A synthetic v0 → v1 step in front of the real v1 → v2 → v3 steps.
        fn rename(graph: &mut MaterialGraph) -> Result<(), String> {
            graph.name = format!("migrated {}", graph.name);
            Ok(())
        }
        let mut steps = vec![VersionStep {
            from: 0,
            to: 1,
            migrate: rename,
        }];
        steps.extend_from_slice(builtin_migrations());
        let mut graph = MaterialGraph::from_ron(MINIMAL).expect("parse");
        graph.format_version = 0;
        graph.name = "old".into();
        assert_eq!(
            graph.migrate_with(&steps).expect("migrate"),
            MATGRAPH_FORMAT_VERSION
        );
        assert_eq!(graph.name, "migrated old");
        let mut gap = graph.clone();
        gap.format_version = 0;
        assert!(matches!(gap.migrate_to_latest(), Err(FormatError::Migration(_))));
    }

    #[test]
    fn a_v1_file_reads_as_the_current_version_unchanged() {
        let v1 = MINIMAL.replace("format_version: 2,", "format_version: 1,");
        let graph = MaterialGraph::from_ron(&v1).expect("v1 parses");
        assert_eq!(graph.format_version, MATGRAPH_FORMAT_VERSION);
        assert!(graph.switches.is_empty() && graph.functions.is_empty());
        assert_eq!(graph.output.world_position_offset, None);
        assert_eq!(graph.shading.max_vertex_offset, 0.0);
        assert_eq!(graph, MaterialGraph::from_ron(MINIMAL).expect("v2"));
    }

    #[test]
    fn a_v2_file_reads_as_v3_unchanged() {
        let v2 = MaterialGraph::from_ron(MINIMAL).expect("v2 parses");
        assert_eq!(v2.format_version, MATGRAPH_FORMAT_VERSION);
        let v3 = MINIMAL.replace("format_version: 2,", "format_version: 3,");
        assert_eq!(v2, MaterialGraph::from_ron(&v3).expect("v3 parses"));
    }

    #[test]
    fn a_v1_graph_runs_the_whole_chain() {
        let mut graph = MaterialGraph::from_ron(MINIMAL).expect("parse");
        let current = graph.clone();
        graph.format_version = 1;
        assert_eq!(
            graph.migrate_to_latest().expect("migrate"),
            MATGRAPH_FORMAT_VERSION
        );
        assert_eq!(graph, current, "every step only moves the version");
        // Without the `2 → 3` step the chain stops at v2: the step is what makes v3 reachable.
        let mut stuck = graph.clone();
        stuck.format_version = 1;
        let v1_only = &builtin_migrations()[..1];
        assert!(matches!(
            stuck.migrate_with(v1_only),
            Err(FormatError::Migration(_))
        ));
    }

    #[test]
    fn a_v1_matfunc_file_reads_as_v2_unchanged() {
        let v1 = r#"(
            format_version: 1,
            inputs: [(name: "x", ty: Vec3)],
            nodes: { "x": Input("x"), "out": OneMinus("x") },
            outputs: { "color": "out" },
        )"#;
        let function = MaterialFunction::from_ron(v1).expect("v1 parses");
        assert_eq!(function.format_version, MATFUNC_FORMAT_VERSION);
        let v2 = v1.replace("format_version: 1", "format_version: 2");
        assert_eq!(function, MaterialFunction::from_ron(&v2).expect("v2 parses"));
        assert_eq!(matfunc_migration_sources(), vec![1, 2, 3]);
        // A v2 function may read the wind; a v5 file is newer than this engine.
        let windy = r#"(
            format_version: 2,
            nodes: { "w": Wind, "speed": Length("w") },
            outputs: { "speed": "speed" },
        )"#;
        let windy = MaterialFunction::from_ron(windy).expect("v2 with Wind parses");
        assert_eq!(windy.nodes["w"], Node::Wind);
        let newer = v2.replace("format_version: 2", "format_version: 5");
        assert!(matches!(
            MaterialFunction::from_ron(&newer),
            Err(FormatError::NewerVersion { found: 5, .. })
        ));
    }

    #[test]
    fn a_graph_using_wind_round_trips() {
        let text = r#"(
            format_version: 3,
            nodes: {
                "w": Wind,
                "wx": Swizzle(input: "w", channels: "x"),
                "zero": Float(0.0),
                "wz": Swizzle(input: "w", channels: "y"),
                "xy": Append("wx", "zero"),
                "dir": Append("xy", "wz"),
                "k": Float(0.01),
                "offset": Mul("dir", "k"),
            },
            output: (world_position_offset: Some("offset")),
            shading: (max_vertex_offset: 0.4),
        )"#;
        let graph = MaterialGraph::from_ron(text).expect("parse");
        assert_eq!(graph.nodes["w"], Node::Wind);
        assert_eq!(Node::Wind.kind(), "Wind");
        assert!(Node::Wind.inputs().is_empty());
        let written = graph.to_ron().expect("write");
        assert!(written.contains("Wind"), "{written}");
        assert_eq!(MaterialGraph::from_ron(&written).expect("reparse"), graph);
    }

    #[test]
    fn function_files_resolve_and_report_missing_ones() {
        let text = r#"(
            format_version: 2,
            functions: { "wear": File("assets/wear.matfunc.ron") },
            nodes: { "c": Vec3((0.2, 0.3, 0.4)) },
            output: (base_color: Some("c")),
        )"#;
        let function = r#"(
            format_version: 1,
            inputs: [(name: "x", ty: Vec3)],
            nodes: { "x": Input("x"), "out": OneMinus("x") },
            outputs: { "color": "out" },
        )"#;
        let mut graph = MaterialGraph::from_ron(text).expect("parse");
        let mut missing = graph.clone();
        let read = graph
            .resolve_function_files(&mut |path| {
                assert_eq!(path, "assets/wear.matfunc.ron");
                Ok(function.to_string())
            })
            .expect("resolve");
        assert_eq!(read, vec!["assets/wear.matfunc.ron".to_string()]);
        assert!(matches!(graph.functions["wear"], FunctionSource::Inline(_)));
        let err = missing
            .resolve_function_files(&mut |_| Err("no such file".into()))
            .expect_err("missing");
        assert!(err.to_string().contains("wear"), "{err}");
        let newer = function.replace("format_version: 1", "format_version: 7");
        assert!(matches!(
            MaterialFunction::from_ron(&newer),
            Err(FormatError::NewerVersion { found: 7, .. })
        ));
    }

    #[test]
    fn a_failed_function_resolve_leaves_every_file_reference_in_place() {
        let text = r#"(
            format_version: 2,
            functions: { "a": File("assets/a.matfunc.ron"), "b": File("assets/b.matfunc.ron") },
            nodes: { "c": Vec3((0.2, 0.3, 0.4)) },
            output: (base_color: Some("c")),
        )"#;
        let function = r#"(
            format_version: 1,
            inputs: [(name: "x", ty: Vec3)],
            nodes: { "x": Input("x"), "out": OneMinus("x") },
            outputs: { "color": "out" },
        )"#;
        let mut graph = MaterialGraph::from_ron(text).expect("parse");
        let before = graph.clone();
        let err = graph
            .resolve_function_files(&mut |path| {
                if path.contains("b.") {
                    Err("gone".into())
                } else {
                    Ok(function.to_string())
                }
            })
            .expect_err("b is missing");
        assert!(err.to_string().contains('b'), "{err}");
        assert_eq!(graph, before, "no function was detached from its file");
    }

    #[test]
    fn the_fingerprint_names_every_node_kind() {
        let fingerprint = matgraph_schema_fingerprint();
        for kind in [
            "TexCoord",
            "Triplanar",
            "HeightBlend",
            "BlendNormals",
            "Parallax",
            "Whiteout",
            "Wind",
        ] {
            assert!(fingerprint.contains(kind), "{kind} missing from {fingerprint}");
        }
        assert_eq!(matgraph_migration_sources(), vec![1, 2, 3, 4]);
        assert_eq!(matfunc_migration_sources(), vec![1, 2, 3]);
        assert!(matfunc_schema_fingerprint().contains("Input"));
        assert!(
            matfunc_schema_fingerprint().contains("Wind"),
            "the node set is part of the .matfunc.ron shape, so v3's node moved matfunc to v2"
        );
        assert!(
            matgraph_schema_fingerprint().contains("Noise") && matfunc_schema_fingerprint().contains("Noise")
        );
    }

    /// v4 only adds the `Noise` node: a v2 graph and a v1 function read unchanged at the new
    /// versions, and a v4 graph with noise round-trips.
    #[test]
    fn v2_graphs_and_v1_functions_read_unchanged_and_noise_round_trips() {
        assert_eq!(MATGRAPH_FORMAT_VERSION, 5);
        assert_eq!(MATFUNC_FORMAT_VERSION, 4);
        let v2 = MaterialGraph::from_ron(MINIMAL).expect("v2 parses");
        assert_eq!(v2.format_version, MATGRAPH_FORMAT_VERSION);
        let v3 = MaterialGraph::from_ron(&MINIMAL.replace("format_version: 2,", "format_version: 3,"))
            .expect("v3");
        assert_eq!(v2, v3);
        let f = MaterialFunction::from_ron(
            r#"(format_version: 1, inputs: [(name: "x", ty: Float)], nodes: { "x": Input("x") }, outputs: { "o": "x" })"#,
        )
        .expect("v1 function");
        assert_eq!(f.format_version, MATFUNC_FORMAT_VERSION);
        let noisy = MaterialGraph::from_ron(
            r#"(
                format_version: 4,
                nodes: { "uv": TexCoord, "n": Noise(input: "uv", octaves: 4), "m": Noise(input: "uv") },
                output: (roughness: Some("n"), metallic: Some("m")),
            )"#,
        )
        .expect("noise parses");
        assert_eq!(
            noisy.nodes["n"],
            Node::Noise {
                input: "uv".into(),
                octaves: 4
            }
        );
        assert_eq!(
            noisy.nodes["m"],
            Node::Noise {
                input: "uv".into(),
                octaves: 1
            },
            "octaves default to 1"
        );
        assert_eq!(noisy.nodes["n"].inputs(), vec!["uv"]);
        let text = noisy.to_ron().expect("write");
        assert_eq!(MaterialGraph::from_ron(&text).expect("reparse"), noisy);
        // A v3 reader refuses it; this engine refuses a v6.
        let err = MaterialGraph::from_ron("(format_version: 6, nodes: {}, output: ())").expect_err("newer");
        assert!(matches!(err, FormatError::NewerVersion { found: 6, .. }), "{err}");
    }

    /// v5 only adds the `Code` node (ADR-7852): a v4 graph and a v3 function read unchanged,
    /// the chain reaches v5 from every older version, and a Code node round-trips with its
    /// typed inputs, texture inputs, output type and body.
    #[test]
    fn v4_graphs_read_unchanged_and_a_code_node_round_trips() {
        let v4 = MINIMAL.replace("format_version: 2,", "format_version: 4,");
        let read = MaterialGraph::from_ron(&v4).expect("v4 parses");
        assert_eq!(read.format_version, 5);
        assert_eq!(read, MaterialGraph::from_ron(MINIMAL).expect("v2"));
        let f = MaterialFunction::from_ron(
            r#"(format_version: 3, inputs: [(name: "x", ty: Float)], nodes: { "x": Input("x") }, outputs: { "o": "x" })"#,
        )
        .expect("v3 function");
        assert_eq!(f.format_version, 4);
        let text = r#"(
            format_version: 5,
            textures: [(name: "albedo", color_space: Srgb)],
            nodes: {
                "uv": TexCoord,
                "u": Swizzle(input: "uv", channels: "x"),
                "mad": Code(
                    inputs: [(name: "a", ty: Float, node: "u"), (name: "b", ty: Float, node: "u"), (name: "c", ty: Float)],
                    textures: ["albedo"],
                    output: Float,
                    code: "return a * b + c;",
                ),
            },
            output: (roughness: Some("mad")),
        )"#;
        let graph = MaterialGraph::from_ron(text).expect("v5 with Code parses");
        let Node::Code {
            inputs,
            textures,
            output,
            code,
        } = &graph.nodes["mad"]
        else {
            panic!("not a code node: {:?}", graph.nodes["mad"]);
        };
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[2].node, "", "an unwired input defaults to the empty name");
        assert_eq!(textures, &vec!["albedo".to_string()]);
        assert_eq!(*output, ValueType::Float);
        assert_eq!(code, "return a * b + c;");
        assert_eq!(graph.nodes["mad"].kind(), "Code");
        assert_eq!(graph.nodes["mad"].inputs(), vec!["u", "u", ""]);
        let written = graph.to_ron().expect("write");
        assert_eq!(MaterialGraph::from_ron(&written).expect("reparse"), graph);
        assert!(matgraph_schema_fingerprint().contains("Code"));
        assert!(matfunc_schema_fingerprint().contains("Code"));
    }

    /// ADR-8105 (b): a v4 graph upgrades to v5 with the default parameter metadata (no
    /// category, no tooltip, no range) and writes back without the new fields; a v5 file with
    /// metadata reads and round-trips; a bad range is refused naming the parameter; compile
    /// ignores the metadata (the same WGSL with and without it).
    #[test]
    fn a_v4_graph_upgrades_with_default_param_metadata() {
        let v4 = r#"(
            format_version: 4,
            params: [(name: "rough", value: Float(0.5)), (name: "tint", value: Color((1.0, 0.5, 0.25)))],
            nodes: { "r": Param("rough"), "t": Param("tint") },
            output: (base_color: Some("t"), roughness: Some("r")),
        )"#;
        let graph = MaterialGraph::from_ron(v4).expect("v4 reads");
        assert_eq!(graph.format_version, MATGRAPH_FORMAT_VERSION);
        for p in &graph.params {
            assert!(
                p.category.is_empty() && p.tooltip.is_empty() && p.range.is_none(),
                "{p:?}"
            );
        }
        let written = graph.to_ron().expect("write");
        for field in ["category", "tooltip", "range"] {
            assert!(
                !written.contains(field),
                "no metadata written by default: {written}"
            );
        }
        let mut with_meta = graph.clone();
        with_meta.params[0].category = "Surface".into();
        with_meta.params[0].tooltip = "How rough".into();
        with_meta.params[0].range = Some(ParamRange {
            min: 0.0,
            max: 1.0,
            step: 0.25,
        });
        with_meta.params[1].range = Some(ParamRange {
            min: 0.0,
            max: 1.0,
            step: 0.0,
        });
        let text = with_meta.to_ron().expect("write v5");
        assert_eq!(MaterialGraph::from_ron(&text).expect("reparse"), with_meta);
        assert_eq!(
            with_meta.param_groups(),
            vec![(String::new(), vec![1]), ("Surface".to_string(), vec![0])]
        );
        let plain = crate::compile(&graph).expect("compiles");
        let meta = crate::compile(&with_meta).expect("compiles with metadata");
        assert_eq!(plain.wgsl, meta.wgsl, "compile ignores the metadata");

        for (range, why) in [
            ("(min: 1.0, max: 0.0)", "not below max"),
            ("(min: 0.0, max: 1.0, step: 2.0)", "step"),
            ("(min: 0.6, max: 1.0)", "outside the range"),
        ] {
            let bad = format!(
                r#"(format_version: 5, params: [(name: "rough", value: Float(0.5), range: Some({range}))], nodes: {{ "r": Param("rough") }}, output: (roughness: Some("r")))"#
            );
            match MaterialGraph::from_ron(&bad) {
                Err(FormatError::Param(message)) => {
                    assert!(message.contains("rough") && message.contains(why), "{message}");
                }
                other => panic!("{range} must be refused: {other:?}"),
            }
        }
        let range = ParamRange {
            min: -1.0,
            max: 1.0,
            step: 0.5,
        };
        assert_eq!(range.clamp(0.3), 0.5);
        assert_eq!(range.clamp(-7.0), -1.0);
        assert_eq!(range.clamp(f32::NAN), -1.0);
        assert_eq!(
            range.clamp_value(ParamValue::Vec3([2.0, 0.2, -0.8])),
            ParamValue::Vec3([1.0, 0.0, -1.0])
        );
        assert!(matgraph_schema_fingerprint().contains("step"));
    }
}
