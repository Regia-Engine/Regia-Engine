// SPDX-License-Identifier: MPL-2.0
//! The pin model the material graph editor wires (`docs/specs/80-matgraph-remainder.md` § 3):
//! the named input slots of each node kind, rewiring one slot, a fresh node of a kind, and the
//! surface outputs by slot name.
//!
//! Nothing here changes the format: a disconnected required input holds the empty name `""`,
//! which the compiler reports on that node ("an input is not connected").

use crate::format::{Channel, Node, NormalBlend, SurfaceOutputs};

impl Node {
    /// Every node kind [`Node::default_of`] accepts, in the order the editor's add menu lists
    /// them (inputs, textures, math, shading). Equal to the [`Node::kind`] names.
    pub const KINDS: &'static [&'static str] = &[
        "TexCoord",
        "TexCoord1",
        "VertexColor",
        "WorldPosition",
        "WorldNormal",
        "ViewDirection",
        "CameraDistance",
        "ObjectPosition",
        "Time",
        "Wind",
        "Param",
        "Float",
        "Vec2",
        "Vec3",
        "Vec4",
        "Sample",
        "Triplanar",
        "UvTransform",
        "Parallax",
        "Add",
        "Sub",
        "Mul",
        "Div",
        "Min",
        "Max",
        "Pow",
        "Dot",
        "Lerp",
        "Smoothstep",
        "Saturate",
        "OneMinus",
        "Abs",
        "Normalize",
        "Length",
        "Sin",
        "Cos",
        "Frac",
        "Noise",
        "Swizzle",
        "Append",
        "Fresnel",
        "HeightBlend",
        "UnpackNormal",
        "NormalStrength",
        "BlendNormals",
        "Switch",
        "Call",
        "Input",
        "Code",
    ];

    /// A new node of `kind` with every input unwired (`""`, optional inputs `None`), empty
    /// names (`Param("")`, a texture `""`) and the format's defaults for the rest. `None` for a
    /// name not in [`Node::KINDS`].
    #[must_use]
    pub fn default_of(kind: &str) -> Option<Node> {
        let e = String::new;
        Some(match kind {
            "TexCoord" => Node::TexCoord,
            "TexCoord1" => Node::TexCoord1,
            "VertexColor" => Node::VertexColor,
            "WorldPosition" => Node::WorldPosition,
            "WorldNormal" => Node::WorldNormal,
            "ViewDirection" => Node::ViewDirection,
            "CameraDistance" => Node::CameraDistance,
            "ObjectPosition" => Node::ObjectPosition,
            "Time" => Node::Time,
            "Wind" => Node::Wind,
            "Param" => Node::Param(e()),
            "Float" => Node::Float(0.0),
            "Vec2" => Node::Vec2([0.0; 2]),
            "Vec3" => Node::Vec3([0.0; 3]),
            "Vec4" => Node::Vec4([0.0; 4]),
            "Sample" => Node::Sample {
                texture: e(),
                uv: e(),
                lod: None,
            },
            "Triplanar" => Node::Triplanar {
                texture: e(),
                scale: e(),
                sharpness: 4.0,
            },
            "UvTransform" => Node::UvTransform {
                uv: e(),
                scale: [1.0, 1.0],
                offset: [0.0, 0.0],
                rotation: 0.0,
            },
            "Parallax" => Node::Parallax {
                texture: e(),
                channel: Channel::R,
                uv: e(),
                scale: e(),
                min_steps: 8.0,
                max_steps: 16.0,
            },
            "Add" => Node::Add(e(), e()),
            "Sub" => Node::Sub(e(), e()),
            "Mul" => Node::Mul(e(), e()),
            "Div" => Node::Div(e(), e()),
            "Min" => Node::Min(e(), e()),
            "Max" => Node::Max(e(), e()),
            "Pow" => Node::Pow(e(), e()),
            "Dot" => Node::Dot(e(), e()),
            "Lerp" => Node::Lerp {
                a: e(),
                b: e(),
                t: e(),
            },
            "Smoothstep" => Node::Smoothstep {
                edge0: e(),
                edge1: e(),
                x: e(),
            },
            "Saturate" => Node::Saturate(e()),
            "OneMinus" => Node::OneMinus(e()),
            "Abs" => Node::Abs(e()),
            "Normalize" => Node::Normalize(e()),
            "Length" => Node::Length(e()),
            "Sin" => Node::Sin(e()),
            "Cos" => Node::Cos(e()),
            "Frac" => Node::Frac(e()),
            "Swizzle" => Node::Swizzle {
                input: e(),
                channels: "x".into(),
            },
            "Append" => Node::Append(e(), e()),
            "Fresnel" => Node::Fresnel { power: e() },
            "HeightBlend" => Node::HeightBlend {
                a: e(),
                b: e(),
                height_a: e(),
                height_b: e(),
                t: e(),
                depth: 0.2,
            },
            "UnpackNormal" => Node::UnpackNormal(e()),
            "NormalStrength" => Node::NormalStrength {
                normal: e(),
                strength: e(),
            },
            "BlendNormals" => Node::BlendNormals {
                base: e(),
                detail: e(),
                mode: NormalBlend::Rnm,
            },
            "Switch" => Node::Switch {
                switch: e(),
                on: e(),
                off: e(),
            },
            "Call" => Node::Call {
                function: e(),
                args: std::collections::BTreeMap::new(),
                output: None,
            },
            "Input" => Node::Input(e()),
            "Noise" => Node::Noise {
                input: e(),
                octaves: 1,
            },
            // One Float input passed through: the smallest body that compiles (ADR-7852).
            "Code" => Node::Code {
                inputs: vec![crate::format::CodeInput {
                    name: "a".into(),
                    ty: crate::format::ValueType::Float,
                    node: e(),
                }],
                textures: Vec::new(),
                output: crate::format::ValueType::Float,
                code: "return a;".into(),
            },
            _ => return None,
        })
    }

    /// The node's input slots in input order: `(slot name, wired node)`. The wired node is
    /// `None` for an unset optional input (`Sample`'s `lod`) and for an empty name. A `Call`
    /// lists its argument names (the function's other inputs are not known here).
    #[must_use]
    pub fn input_slots(&self) -> Vec<(String, Option<&str>)> {
        fn s<'a>(name: &str, node: &'a str) -> (String, Option<&'a str>) {
            (name.to_string(), (!node.is_empty()).then_some(node))
        }
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
            Self::Sample { uv, lod, .. } => vec![
                s("uv", uv),
                ("lod".into(), lod.as_deref().filter(|l| !l.is_empty())),
            ],
            Self::Triplanar { scale, .. } => vec![s("scale", scale)],
            Self::UvTransform { uv, .. } => vec![s("uv", uv)],
            Self::Parallax { uv, scale, .. } => vec![s("uv", uv), s("scale", scale)],
            Self::Add(a, b)
            | Self::Sub(a, b)
            | Self::Mul(a, b)
            | Self::Div(a, b)
            | Self::Min(a, b)
            | Self::Max(a, b)
            | Self::Pow(a, b)
            | Self::Dot(a, b)
            | Self::Append(a, b) => vec![s("a", a), s("b", b)],
            Self::Lerp { a, b, t } => vec![s("a", a), s("b", b), s("t", t)],
            Self::Smoothstep { edge0, edge1, x } => vec![s("edge0", edge0), s("edge1", edge1), s("x", x)],
            Self::Saturate(x)
            | Self::OneMinus(x)
            | Self::Abs(x)
            | Self::Normalize(x)
            | Self::Length(x)
            | Self::Sin(x)
            | Self::Cos(x)
            | Self::Frac(x)
            | Self::UnpackNormal(x) => vec![s("x", x)],
            Self::Swizzle { input, .. } | Self::Noise { input, .. } => vec![s("input", input)],
            Self::Fresnel { power } => vec![s("power", power)],
            Self::HeightBlend {
                a,
                b,
                height_a,
                height_b,
                t,
                ..
            } => vec![
                s("a", a),
                s("b", b),
                s("height_a", height_a),
                s("height_b", height_b),
                s("t", t),
            ],
            Self::NormalStrength { normal, strength } => vec![s("normal", normal), s("strength", strength)],
            Self::BlendNormals { base, detail, .. } => vec![s("base", base), s("detail", detail)],
            Self::Switch { on, off, .. } => vec![s("on", on), s("off", off)],
            Self::Call { args, .. } => args.iter().map(|(k, v)| s(k, v)).collect(),
            Self::Code { inputs, .. } => inputs.iter().map(|i| s(&i.name, &i.node)).collect(),
        }
    }

    /// Wires input `slot` to `node`, or unwires it (`None`: a required input becomes `""`,
    /// `Sample`'s `lod` becomes unset, a `Call` argument is removed). A `Call` accepts any
    /// argument name.
    ///
    /// # Errors
    ///
    /// The kind has no input called `slot` (the message lists the slots it has).
    pub fn set_input(&mut self, slot: &str, node: Option<String>) -> Result<(), String> {
        if let Self::Call { args, .. } = self {
            match node {
                Some(node) => {
                    args.insert(slot.to_string(), node);
                }
                None => {
                    args.remove(slot);
                }
            }
            return Ok(());
        }
        if let Self::Sample { lod, .. } = self
            && slot == "lod"
        {
            *lod = node.filter(|n| !n.is_empty());
            return Ok(());
        }
        let names: Vec<String> = self.input_slots().into_iter().map(|(name, _)| name).collect();
        let Some(index) = names.iter().position(|n| n == slot) else {
            return Err(format!(
                "`{}` has no input `{slot}` ({})",
                self.kind(),
                if names.is_empty() {
                    "it has no inputs".to_string()
                } else {
                    names.join(", ")
                }
            ));
        };
        // `map_inputs` visits the wired inputs in `inputs()` order; for every kind but `Sample`
        // (handled above) and `Call` that is the slot order.
        let value = node.unwrap_or_default();
        let mut i = 0;
        let mut value = Some(value);
        self.map_inputs(|input| {
            if i == index
                && let Some(v) = value.take()
            {
                *input = v;
            }
            i += 1;
        });
        Ok(())
    }
}

impl SurfaceOutputs {
    /// The surface output slots, in the contract's order (`specs/13` § 7).
    pub const SLOTS: [&'static str; 8] = [
        "base_color",
        "alpha",
        "metallic",
        "roughness",
        "normal",
        "occlusion",
        "emissive",
        "world_position_offset",
    ];

    fn slot_mut(&mut self, slot: &str) -> Option<&mut Option<String>> {
        Some(match slot {
            "base_color" => &mut self.base_color,
            "alpha" => &mut self.alpha,
            "metallic" => &mut self.metallic,
            "roughness" => &mut self.roughness,
            "normal" => &mut self.normal,
            "occlusion" => &mut self.occlusion,
            "emissive" => &mut self.emissive,
            "world_position_offset" => &mut self.world_position_offset,
            _ => return None,
        })
    }

    /// The node driving `slot`, if any (`None` also for an unknown slot).
    #[must_use]
    pub fn get(&self, slot: &str) -> Option<&str> {
        match slot {
            "base_color" => self.base_color.as_deref(),
            "alpha" => self.alpha.as_deref(),
            "metallic" => self.metallic.as_deref(),
            "roughness" => self.roughness.as_deref(),
            "normal" => self.normal.as_deref(),
            "occlusion" => self.occlusion.as_deref(),
            "emissive" => self.emissive.as_deref(),
            "world_position_offset" => self.world_position_offset.as_deref(),
            _ => None,
        }
    }

    /// Drives `slot` with `node`, or clears it (`None`).
    ///
    /// # Errors
    ///
    /// `slot` is not one of [`SurfaceOutputs::SLOTS`].
    pub fn set(&mut self, slot: &str, node: Option<String>) -> Result<(), String> {
        let target = self
            .slot_mut(slot)
            .ok_or_else(|| format!("no surface output `{slot}` ({})", Self::SLOTS.join(", ")))?;
        *target = node.filter(|n| !n.is_empty());
        Ok(())
    }

    /// Rewrites every slot that names `from` to name `to` (`None` clears them). Returns how
    /// many slots changed.
    pub fn rename(&mut self, from: &str, to: Option<&str>) -> usize {
        let mut changed = 0;
        for slot in Self::SLOTS {
            if let Some(target) = self.slot_mut(slot)
                && target.as_deref() == Some(from)
            {
                *target = to.map(str::to_string);
                changed += 1;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_default_whose_kind_matches() {
        for kind in Node::KINDS {
            let node = Node::default_of(kind).unwrap_or_else(|| panic!("no default for {kind}"));
            assert_eq!(node.kind(), *kind);
        }
        assert!(Node::default_of("Nope").is_none());
        let unique: std::collections::BTreeSet<_> = Node::KINDS.iter().collect();
        assert_eq!(unique.len(), Node::KINDS.len(), "KINDS has a duplicate");
    }

    #[test]
    fn defaults_round_trip_through_ron() {
        for kind in Node::KINDS {
            let node = Node::default_of(kind).unwrap_or_else(|| panic!("{kind}"));
            let text = ron::to_string(&node).unwrap_or_else(|e| panic!("{kind}: {e}"));
            let back: Node = ron::from_str(&text).unwrap_or_else(|e| panic!("{kind}: {e} in {text}"));
            assert_eq!(back, node, "{kind}");
        }
    }

    #[test]
    fn defaults_are_unwired() {
        for kind in Node::KINDS {
            let node = Node::default_of(kind).unwrap_or_else(|| panic!("{kind}"));
            assert!(
                node.input_slots().iter().all(|(_, wired)| wired.is_none()),
                "{kind}: {:?}",
                node.input_slots()
            );
        }
    }

    /// Wiring every slot to a distinct name and reading `inputs()` back gives those names in
    /// slot order: the slot model and the compiler's input order agree for every kind.
    #[test]
    fn set_input_agrees_with_inputs_for_every_kind() {
        for kind in Node::KINDS {
            let mut node = Node::default_of(kind).unwrap_or_else(|| panic!("{kind}"));
            let slots: Vec<String> = node.input_slots().into_iter().map(|(n, _)| n).collect();
            let mut want = Vec::new();
            for slot in &slots {
                let name = format!("src_{slot}");
                node.set_input(slot, Some(name.clone()))
                    .unwrap_or_else(|e| panic!("{kind}.{slot}: {e}"));
                want.push(name);
            }
            assert_eq!(node.inputs(), want, "{kind}");
            let wired: Vec<Option<&str>> = node.input_slots().into_iter().map(|(_, w)| w).collect();
            assert!(wired.iter().all(Option::is_some), "{kind}: {wired:?}");
            // Unwire each slot again.
            for slot in &slots {
                node.set_input(slot, None)
                    .unwrap_or_else(|e| panic!("{kind}.{slot}: {e}"));
            }
            assert!(
                node.input_slots().iter().all(|(_, w)| w.is_none()),
                "{kind} after unwiring"
            );
        }
    }

    #[test]
    fn set_input_on_one_slot_leaves_the_others() {
        let mut node = Node::default_of("Lerp").unwrap_or(Node::Time);
        node.set_input("a", Some("x".into())).ok();
        node.set_input("t", Some("w".into())).ok();
        assert_eq!(
            node,
            Node::Lerp {
                a: "x".into(),
                b: String::new(),
                t: "w".into()
            }
        );
    }

    #[test]
    fn unknown_slots_name_the_kinds_slots() {
        let mut lerp = Node::default_of("Lerp").unwrap_or(Node::Time);
        let err = lerp.set_input("q", Some("x".into())).err().unwrap_or_default();
        assert!(err.contains("a, b, t"), "{err}");
        let mut time = Node::Time;
        let err = time.set_input("x", Some("y".into())).err().unwrap_or_default();
        assert!(err.contains("no inputs"), "{err}");
    }

    #[test]
    fn call_arguments_are_slots() {
        let mut call = Node::default_of("Call").unwrap_or(Node::Time);
        call.set_input("c", Some("tint".into())).ok();
        assert_eq!(call.input_slots(), vec![("c".to_string(), Some("tint"))]);
        call.set_input("c", None).ok();
        assert!(call.input_slots().is_empty());
    }

    #[test]
    fn an_unwired_input_is_an_error_on_its_node() {
        let mut graph = crate::MaterialGraph::new("g");
        graph
            .nodes
            .insert("sat".into(), Node::default_of("Saturate").unwrap_or(Node::Time));
        graph.output.roughness = Some("sat".into());
        let errors = crate::compile_unvalidated(&graph)
            .err()
            .map(|e| e.0)
            .unwrap_or_default();
        assert!(
            errors
                .iter()
                .any(|e| e.node.as_deref() == Some("sat") && e.message == "an input is not connected"),
            "{errors:?}"
        );
    }

    #[test]
    fn surface_slots_get_set_and_rename() {
        let mut out = SurfaceOutputs::default();
        for slot in SurfaceOutputs::SLOTS {
            assert!(out.get(slot).is_none());
            out.set(slot, Some(format!("n_{slot}")))
                .unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(out.get(slot), Some(format!("n_{slot}").as_str()));
        }
        assert!(out.set("nope", None).is_err());
        out.set("alpha", Some("shared".into())).ok();
        out.set("metallic", Some("shared".into())).ok();
        assert_eq!(out.rename("shared", Some("renamed")), 2);
        assert_eq!(out.get("alpha"), Some("renamed"));
        assert_eq!(out.rename("renamed", None), 2);
        assert_eq!(out.get("metallic"), None);
        out.set("normal", Some(String::new())).ok();
        assert_eq!(out.get("normal"), None, "an empty name clears the slot");
    }
}
