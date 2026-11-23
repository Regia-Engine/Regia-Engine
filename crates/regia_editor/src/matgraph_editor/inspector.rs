// SPDX-License-Identifier: MPL-2.0
//! The typed node inspector's model (spec 80 § 12, ADR-9700): every node kind's editable
//! values as typed rows ([`fields`]) and one row's edit ([`set_field`]). The side panel draws
//! the rows (drag values, color pickers, dropdowns, the texture and function pickers) and the
//! agent's `editor.matgraph_node_fields` / `editor.matgraph_node_set` read and write the same
//! rows. Wires are not rows: they stay on the canvas (and `editor.matgraph_edit` connect).

use regia_matgraph::{Channel, Node, NormalBlend};
use serde_json::{Value, json};

/// `Noise` octaves are clamped to this range (the format's 1–8).
pub const NOISE_OCTAVES: std::ops::RangeInclusive<u32> = 1..=8;

/// What a row edits, which picks its widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// One number (a drag value).
    Float,
    /// `n` numbers (drag values; three or four also get a color picker).
    Floats(usize),
    /// A whole number in a range (a drag value, clamped).
    Int {
        /// Smallest value.
        min: u32,
        /// Largest value.
        max: u32,
    },
    /// A declared parameter's name (a dropdown; the panel also edits its default).
    Param,
    /// A declared texture's name (the texture picker with a thumbnail).
    Texture,
    /// A declared static switch's name (a dropdown).
    Switch,
    /// A function input's name (a dropdown of the function's inputs).
    Input,
    /// A function's name (the function picker).
    Function,
    /// A `Call`'s output name (a dropdown of the function's outputs; empty = the only one).
    CallOutput,
    /// A texture channel (`R`, `G`, `B`, `A`).
    Channel,
    /// A normal blend mode (`Rnm`, `Udn`, `Whiteout`).
    NormalBlend,
    /// Swizzle letters (1–4 of one alphabet: `xyzw` or `rgba`).
    Channels,
}

/// A row's value.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    /// One number.
    Float(f32),
    /// Several numbers.
    Floats(Vec<f32>),
    /// A whole number.
    Int(u32),
    /// A name or an enum variant.
    Text(String),
    /// An optional name (`None`: unset).
    OptText(Option<String>),
}

impl FieldValue {
    /// The value as JSON (`editor.matgraph_node_fields`).
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Float(v) => json!(v),
            Self::Floats(v) => json!(v),
            Self::Int(v) => json!(v),
            Self::Text(v) => json!(v),
            Self::OptText(v) => json!(v),
        }
    }

    /// A JSON value read as the value of a row of `kind` (`editor.matgraph_node_set`).
    ///
    /// # Errors
    ///
    /// The JSON does not fit the row.
    pub fn from_json(kind: FieldKind, value: &Value) -> Result<Self, String> {
        let number = |v: &Value| v.as_f64().map(|f| f as f32);
        match kind {
            FieldKind::Float => number(value)
                .map(Self::Float)
                .ok_or_else(|| "expected a number".to_string()),
            FieldKind::Floats(n) => {
                let list: Option<Vec<f32>> = value.as_array().map(|a| a.iter().filter_map(number).collect());
                match list {
                    Some(list) if list.len() == n => Ok(Self::Floats(list)),
                    _ => Err(format!("expected an array of {n} numbers")),
                }
            }
            FieldKind::Int { .. } => value
                .as_u64()
                .map(|v| Self::Int(u32::try_from(v).unwrap_or(u32::MAX)))
                .ok_or_else(|| "expected a whole number".to_string()),
            FieldKind::CallOutput => match value {
                Value::Null => Ok(Self::OptText(None)),
                Value::String(s) if s.is_empty() => Ok(Self::OptText(None)),
                Value::String(s) => Ok(Self::OptText(Some(s.clone()))),
                _ => Err("expected a string or null".to_string()),
            },
            _ => value
                .as_str()
                .map(|s| Self::Text(s.to_string()))
                .ok_or_else(|| "expected a string".to_string()),
        }
    }
}

/// One typed row of the inspector.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// The row's name (the node's field).
    pub name: &'static str,
    /// What it edits.
    pub kind: FieldKind,
    /// Its value now.
    pub value: FieldValue,
}

impl Field {
    fn new(name: &'static str, kind: FieldKind, value: FieldValue) -> Self {
        Self { name, kind, value }
    }
}

/// The kind name of a [`FieldKind`] for the protocol.
#[must_use]
pub fn kind_name(kind: FieldKind) -> &'static str {
    match kind {
        FieldKind::Float => "float",
        FieldKind::Floats(2) => "vec2",
        FieldKind::Floats(3) => "vec3",
        FieldKind::Floats(_) => "vec4",
        FieldKind::Int { .. } => "int",
        FieldKind::Param => "param",
        FieldKind::Texture => "texture",
        FieldKind::Switch => "switch",
        FieldKind::Input => "input",
        FieldKind::Function => "function",
        FieldKind::CallOutput => "call_output",
        FieldKind::Channel => "channel",
        FieldKind::NormalBlend => "normal_blend",
        FieldKind::Channels => "channels",
    }
}

fn channel_name(c: Channel) -> &'static str {
    match c {
        Channel::R => "R",
        Channel::G => "G",
        Channel::B => "B",
        Channel::A => "A",
    }
}

/// The channel names a [`FieldKind::Channel`] row offers.
pub const CHANNELS: [&str; 4] = ["R", "G", "B", "A"];
/// The blend names a [`FieldKind::NormalBlend`] row offers.
pub const NORMAL_BLENDS: [&str; 3] = ["Rnm", "Udn", "Whiteout"];

fn blend_name(b: NormalBlend) -> &'static str {
    match b {
        NormalBlend::Rnm => "Rnm",
        NormalBlend::Udn => "Udn",
        NormalBlend::Whiteout => "Whiteout",
    }
}

/// The typed rows of `node`, in the order the panel draws them (no rows for wires).
#[must_use]
pub fn fields(node: &Node) -> Vec<Field> {
    use FieldKind as K;
    use FieldValue as V;
    match node {
        Node::Param(p) => vec![Field::new("param", K::Param, V::Text(p.clone()))],
        Node::Input(i) => vec![Field::new("input", K::Input, V::Text(i.clone()))],
        Node::Float(v) => vec![Field::new("value", K::Float, V::Float(*v))],
        Node::Vec2(v) => vec![Field::new("value", K::Floats(2), V::Floats(v.to_vec()))],
        Node::Vec3(v) => vec![Field::new("value", K::Floats(3), V::Floats(v.to_vec()))],
        Node::Vec4(v) => vec![Field::new("value", K::Floats(4), V::Floats(v.to_vec()))],
        Node::Sample { texture, .. } => vec![Field::new("texture", K::Texture, V::Text(texture.clone()))],
        Node::Triplanar {
            texture, sharpness, ..
        } => vec![
            Field::new("texture", K::Texture, V::Text(texture.clone())),
            Field::new("sharpness", K::Float, V::Float(*sharpness)),
        ],
        Node::UvTransform {
            scale,
            offset,
            rotation,
            ..
        } => vec![
            Field::new("scale", K::Floats(2), V::Floats(scale.to_vec())),
            Field::new("offset", K::Floats(2), V::Floats(offset.to_vec())),
            Field::new("rotation", K::Float, V::Float(*rotation)),
        ],
        Node::Parallax {
            texture,
            channel,
            min_steps,
            max_steps,
            ..
        } => vec![
            Field::new("texture", K::Texture, V::Text(texture.clone())),
            Field::new("channel", K::Channel, V::Text(channel_name(*channel).into())),
            Field::new("min_steps", K::Float, V::Float(*min_steps)),
            Field::new("max_steps", K::Float, V::Float(*max_steps)),
        ],
        Node::Noise { octaves, .. } => vec![Field::new(
            "octaves",
            K::Int {
                min: *NOISE_OCTAVES.start(),
                max: *NOISE_OCTAVES.end(),
            },
            V::Int(*octaves),
        )],
        Node::Swizzle { channels, .. } => {
            vec![Field::new("channels", K::Channels, V::Text(channels.clone()))]
        }
        Node::HeightBlend { depth, .. } => vec![Field::new("depth", K::Float, V::Float(*depth))],
        Node::BlendNormals { mode, .. } => {
            vec![Field::new(
                "mode",
                K::NormalBlend,
                V::Text(blend_name(*mode).into()),
            )]
        }
        Node::Switch { switch, .. } => vec![Field::new("switch", K::Switch, V::Text(switch.clone()))],
        Node::Call { function, output, .. } => vec![
            Field::new("function", K::Function, V::Text(function.clone())),
            Field::new("output", K::CallOutput, V::OptText(output.clone())),
        ],
        _ => Vec::new(),
    }
}

fn float(value: &FieldValue) -> Result<f32, String> {
    match value {
        FieldValue::Float(v) if v.is_finite() => Ok(*v),
        FieldValue::Float(_) => Err("the number must be finite".into()),
        _ => Err("expected a number".into()),
    }
}

fn floats<const N: usize>(value: &FieldValue) -> Result<[f32; N], String> {
    match value {
        FieldValue::Floats(v) if v.len() == N && v.iter().all(|x| x.is_finite()) => {
            let mut out = [0.0; N];
            out.copy_from_slice(v);
            Ok(out)
        }
        FieldValue::Floats(_) => Err(format!("expected {N} finite numbers")),
        _ => Err(format!("expected {N} numbers")),
    }
}

fn text(value: &FieldValue) -> Result<String, String> {
    match value {
        FieldValue::Text(t) => Ok(t.clone()),
        _ => Err("expected a name".into()),
    }
}

/// Whether `letters` is a valid swizzle: 1–4 letters of one alphabet (`xyzw` or `rgba`).
#[must_use]
pub fn valid_channels(letters: &str) -> bool {
    !letters.is_empty()
        && letters.len() <= 4
        && (letters.chars().all(|c| "xyzw".contains(c)) || letters.chars().all(|c| "rgba".contains(c)))
}

/// `node` with row `field` set to `value`. `Noise` octaves are clamped to 1..8, sharpness to
/// ≥ 1, a blend depth and the parallax steps to > 0.
///
/// # Errors
///
/// The node has no such row, or the value does not fit it (not finite, an unknown enum name,
/// a bad swizzle).
pub fn set_field(node: &Node, field: &str, value: FieldValue) -> Result<Node, String> {
    let mut out = node.clone();
    let bad = || format!("`{}` has no typed field `{field}`", node.kind());
    match (&mut out, field) {
        (Node::Param(p), "param") | (Node::Input(p), "input") => *p = text(&value)?,
        (Node::Float(v), "value") => *v = float(&value)?,
        (Node::Vec2(v), "value") => *v = floats::<2>(&value)?,
        (Node::Vec3(v), "value") => *v = floats::<3>(&value)?,
        (Node::Vec4(v), "value") => *v = floats::<4>(&value)?,
        (
            Node::Sample { texture, .. } | Node::Triplanar { texture, .. } | Node::Parallax { texture, .. },
            "texture",
        ) => *texture = text(&value)?,
        (Node::Triplanar { sharpness, .. }, "sharpness") => *sharpness = float(&value)?.max(1.0),
        (Node::UvTransform { scale, .. }, "scale") => *scale = floats::<2>(&value)?,
        (Node::UvTransform { offset, .. }, "offset") => *offset = floats::<2>(&value)?,
        (Node::UvTransform { rotation, .. }, "rotation") => *rotation = float(&value)?,
        (Node::Parallax { channel, .. }, "channel") => {
            *channel = match text(&value)?.as_str() {
                "R" => Channel::R,
                "G" => Channel::G,
                "B" => Channel::B,
                "A" => Channel::A,
                other => return Err(format!("unknown channel `{other}` (R, G, B, A)")),
            };
        }
        (Node::Parallax { min_steps, .. }, "min_steps") => *min_steps = float(&value)?.max(1.0),
        (Node::Parallax { max_steps, .. }, "max_steps") => *max_steps = float(&value)?.max(1.0),
        (Node::Noise { octaves, .. }, "octaves") => {
            let FieldValue::Int(n) = value else {
                return Err("expected a whole number".into());
            };
            *octaves = n.clamp(*NOISE_OCTAVES.start(), *NOISE_OCTAVES.end());
        }
        (Node::Swizzle { channels, .. }, "channels") => {
            let letters = text(&value)?;
            if !valid_channels(&letters) {
                return Err(format!("bad swizzle `{letters}`: 1–4 letters of xyzw or of rgba"));
            }
            *channels = letters;
        }
        (Node::HeightBlend { depth, .. }, "depth") => {
            let d = float(&value)?;
            if d <= 0.0 {
                return Err("the blend depth must be above 0".into());
            }
            *depth = d;
        }
        (Node::BlendNormals { mode, .. }, "mode") => {
            *mode = match text(&value)?.as_str() {
                "Rnm" => NormalBlend::Rnm,
                "Udn" => NormalBlend::Udn,
                "Whiteout" => NormalBlend::Whiteout,
                other => return Err(format!("unknown blend `{other}` (Rnm, Udn, Whiteout)")),
            };
        }
        (Node::Switch { switch, .. }, "switch") => *switch = text(&value)?,
        (Node::Call { function, .. }, "function") => *function = text(&value)?,
        (Node::Call { output, .. }, "output") => {
            *output = match value {
                FieldValue::OptText(o) => o.filter(|s| !s.is_empty()),
                FieldValue::Text(s) if s.is_empty() => None,
                FieldValue::Text(s) => Some(s),
                _ => return Err("expected an output name or none".into()),
            };
        }
        _ => return Err(bad()),
    }
    Ok(out)
}
