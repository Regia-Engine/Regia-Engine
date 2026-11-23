// SPDX-License-Identifier: MPL-2.0
//! The agent-protocol surface of the looks (`looks.*`), as plain JSON in and out, so any host
//! (the desktop viewer sample, the Android viewer, the web player's JS bridge) can register it on
//! its agent transport without this crate depending on one. Methods and error codes are
//! documented in `book/agents/product-looks.md`.

use serde_json::{Value, json};

use crate::cuts::{CutType, FacetRole, GemCut, polytope_faces};
use crate::material::{Library, LookMaterial};

/// The methods this surface answers.
pub const METHODS: [&str; 3] = ["looks.list", "looks.material", "looks.cut"];

/// An agent error: a stable lowercase code and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    /// Lowercase ascii, digits and `_`.
    pub code: &'static str,
    /// Human-readable detail.
    pub message: String,
}

fn err(code: &'static str, message: impl Into<String>) -> AgentError {
    AgentError {
        code,
        message: message.into(),
    }
}

/// Answers one `looks.*` call against `library` (the built-in one when the host has no other).
pub fn handle(library: &Library, method: &str, params: &Value) -> Result<Value, AgentError> {
    match method {
        "looks.list" => Ok(list(library)),
        "looks.material" => material(library, params),
        "looks.cut" => cut(params),
        other => Err(err("unknown_method", other)),
    }
}

/// An optional boolean parameter; another type is refused, not read as the default.
fn flag(params: &Value, key: &str, fallback: bool) -> Result<bool, AgentError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(fallback),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| err("bad_value", format!("`{key}` must be true or false"))),
    }
}

fn list(library: &Library) -> Value {
    let materials: Vec<Value> = library
        .ids()
        .filter_map(|id| {
            let e = library.entry(id)?;
            let look = library.look(id, None).ok();
            Some(json!({
                "material_id": id,
                "kind": e.get("kind"),
                "look": look.as_ref().map(|l| l.kind().as_str()),
                "display_name": e.get("display_name"),
                "catalogue_facets": e.get("catalogue_facets"),
                "status": e.get("status"),
            }))
        })
        .collect();
    let cuts: Vec<&str> = CutType::ALL.iter().map(|c| c.as_str()).collect();
    json!({ "materials": materials, "cuts": cuts, "kinds": ["metal", "gem", "cabochon", "opal", "frosted"] })
}

fn material(library: &Library, params: &Value) -> Result<Value, AgentError> {
    let id = params
        .get("material_id")
        .and_then(Value::as_str)
        .ok_or_else(|| err("missing_param", "material_id"))?;
    let finish = match params.get("finish") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| err("bad_value", "`finish` must be a string"))?,
        ),
    };
    let faceted = flag(params, "faceted", true)?;
    let look = library
        .look(id, finish)
        .map_err(|e| err(e.code(), e.to_string()))?
        .for_cut(faceted);
    let p = look.params(
        crate::material::StoneSize::round(2.0, 1.2),
        look.anodize_lut().map(|_| 0),
    );
    let detail = match &look {
        LookMaterial::Metal(m) => json!({
            "f0": m.f0, "f82_tint": m.f82_tint, "roughness": m.roughness, "grain_mm": m.grain_mm,
            "anodize_entries": m.anodize_lut.as_ref().map(Vec::len),
            "anodize_normal": m.anodize_lut.as_ref().and_then(|l| l.last().copied()),
        }),
        LookMaterial::Gem(g) | LookMaterial::Cabochon(g) | LookMaterial::Frosted(g) => json!({
            "n_d": g.n_d, "abbe": g.abbe, "attenuation_per_mm": g.attenuation_per_mm,
            "roughness": g.roughness, "fire": g.fire,
        }),
        LookMaterial::Opal(o) => json!({ "body": o.body, "patch_mm": o.patch_mm, "play": o.play }),
    };
    Ok(json!({
        "material_id": id,
        "kind": look.kind().as_str(),
        "fragment_entry": look.kind().fragment_entry(),
        "params": [p.p0, p.p1, p.p2, p.p3],
        "detail": detail,
    }))
}

fn cut(params: &Value) -> Result<Value, AgentError> {
    let name = params
        .get("cut_type")
        .and_then(Value::as_str)
        .ok_or_else(|| err("missing_param", "cut_type"))?;
    let cut_type = CutType::parse(name).ok_or_else(|| err("unknown_cut", name))?;
    let width = match params.get("width_mm") {
        None | Some(Value::Null) => 3.0,
        Some(v) => v
            .as_f64()
            .ok_or_else(|| err("bad_size", "`width_mm` must be a number"))? as f32,
    };
    if !(0.5..=40.0).contains(&width) {
        return Err(err("bad_size", format!("width_mm {width} outside 0.5..40")));
    }
    let cut = GemCut::new(cut_type, [width * cut_type.length_to_width(), width]);
    let mut counts = serde_json::Map::new();
    for piece in &cut.pieces {
        for face in polytope_faces(piece) {
            let role = match piece[face.plane].role {
                FacetRole::Table => "table",
                FacetRole::Crown => "crown",
                FacetRole::Girdle => "girdle",
                FacetRole::Pavilion => "pavilion",
                FacetRole::Cut => continue,
            };
            let n = counts.get(role).and_then(Value::as_u64).unwrap_or(0) + 1;
            counts.insert(role.into(), json!(n));
        }
    }
    let mesh = cut.mesh();
    let (lo, hi) = mesh.bounds();
    let kit = flag(params, "kit", false)?;
    let kit_entry = if kit {
        cut.to_kit_entry(&format!("standin-{}-{width}", cut_type.as_str()))
    } else {
        None
    };
    Ok(json!({
        "cut_type": cut_type.as_str(),
        "size_mm": cut.size_mm,
        "depth_mm": cut.depth_mm,
        "facet_family": cut.facet_family().as_str(),
        "pieces": cut.pieces.len(),
        "facets": counts,
        "triangles": mesh.triangle_count(),
        "bounds_mm": [lo.to_array(), hi.to_array()],
        "kit_entry": kit_entry,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> Library {
        Library::builtin().expect("builtin")
    }

    #[test]
    fn list_names_every_material_and_cut() {
        let v = handle(&lib(), "looks.list", &json!({})).expect("list");
        assert!(v["materials"].as_array().is_some_and(|m| m.len() > 20));
        assert_eq!(v["cuts"].as_array().map(Vec::len), Some(5));
    }

    #[test]
    fn material_reports_kind_and_params() {
        let v = handle(&lib(), "looks.material", &json!({"material_id": "diamond-white"})).expect("diamond");
        assert_eq!(v["kind"], "gem");
        assert_eq!(v["fragment_entry"], "fs_gem");
        let cab = handle(
            &lib(),
            "looks.material",
            &json!({"material_id": "garnet-red", "faceted": false}),
        )
        .expect("cab");
        assert_eq!(cab["kind"], "cabochon");
        let ano = handle(
            &lib(),
            "looks.material",
            &json!({"material_id": "titanium-anodized-blue"}),
        )
        .expect("blue");
        assert_eq!(ano["detail"]["anodize_entries"], 32);
    }

    #[test]
    fn cut_reports_facets() {
        let v = handle(
            &lib(),
            "looks.cut",
            &json!({"cut_type": "round_brilliant", "width_mm": 2.0}),
        )
        .expect("cut");
        assert_eq!(v["facets"]["table"], 1);
        assert_eq!(v["facets"]["crown"], 32);
        assert_eq!(v["facets"]["pavilion"], 24);
    }

    #[test]
    fn cut_reports_its_facet_family_and_material_its_grain() {
        let family = |c: &str| {
            handle(&lib(), "looks.cut", &json!({"cut_type": c})).expect("cut")["facet_family"].clone()
        };
        assert_eq!(family("princess"), "step");
        assert_eq!(family("round_brilliant"), "brilliant");
        let grain = |finish: &str| {
            let v = handle(
                &lib(),
                "looks.material",
                &json!({"material_id": "steel", "finish": finish}),
            );
            v.expect("steel")["detail"]["grain_mm"].as_f64().expect("grain")
        };
        assert!(grain("finish-sandblast") > 0.0);
        assert_eq!(grain("finish-polish"), 0.0);
    }

    #[test]
    fn errors_use_stable_codes() {
        let l = lib();
        let code = |m: &str, p: Value| handle(&l, m, &p).err().map_or("ok", |e| e.code);
        assert_eq!(code("looks.nope", json!({})), "unknown_method");
        assert_eq!(code("looks.material", json!({})), "missing_param");
        assert_eq!(
            code("looks.material", json!({"material_id": "x"})),
            "material_unknown"
        );
        assert_eq!(code("looks.cut", json!({"cut_type": "rose"})), "unknown_cut");
        assert_eq!(
            code("looks.cut", json!({"cut_type": "pear", "width_mm": 100.0})),
            "bad_size"
        );
        for c in [
            "unknown_method",
            "missing_param",
            "unknown_cut",
            "bad_size",
            "bad_value",
        ] {
            assert!(c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }

    #[test]
    fn a_parameter_of_the_wrong_type_is_refused_not_defaulted() {
        let l = lib();
        let code = |m: &str, p: Value| handle(&l, m, &p).err().map_or("ok", |e| e.code);
        let id = "diamond-white";
        assert_eq!(
            code("looks.material", json!({"material_id": id, "finish": 5})),
            "bad_value"
        );
        assert_eq!(
            code("looks.material", json!({"material_id": id, "faceted": "no"})),
            "bad_value"
        );
        assert_eq!(
            code("looks.cut", json!({"cut_type": "pear", "width_mm": "3"})),
            "bad_size"
        );
        assert_eq!(
            code("looks.cut", json!({"cut_type": "pear", "kit": 1})),
            "bad_value"
        );
        // Absent and null keep meaning "the default".
        assert_eq!(
            code(
                "looks.material",
                json!({"material_id": id, "finish": null, "faceted": null})
            ),
            "ok"
        );
        assert_eq!(
            code(
                "looks.cut",
                json!({"cut_type": "pear", "width_mm": null, "kit": null})
            ),
            "ok"
        );
    }
}
