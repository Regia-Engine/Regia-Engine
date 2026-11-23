// SPDX-License-Identifier: MPL-2.0
//! Offline baking of a material library: fills every `anodized` entry's `fresnel_lut` from its
//! substrate's n, k, the oxide and the thickness (the anodize baker, `crate::anodize`). The
//! built-in library (`data/library-looks.json`) is the bake of `data/library-looks.src.json`; a test
//! keeps the two in step.

use serde_json::{Value, json};

use crate::anodize::{AnodizeInputs, MetalNk, Oxide, bake_anodize_lut};
use crate::material::LookError;

/// The hand-written source of the built-in library (no anodize tables).
pub const BUILTIN_LIBRARY_SOURCE: &str = include_str!("../data/library-looks.src.json");

fn num_array(v: Option<&Value>) -> Option<Vec<f64>> {
    v?.as_array()?.iter().map(Value::as_f64).collect()
}

/// FNV-1a 64 of a string (the `inputs_hash` of a table: it changes when any input changes).
fn fnv1a64(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Bakes every anodize table of a library and returns the library JSON (pretty, sorted keys).
pub fn bake_library(source: &str) -> Result<String, LookError> {
    let _span = tracing::debug_span!("bake_library").entered();
    let mut lib: Value = serde_json::from_str(source).map_err(|e| LookError::Parse(e.to_string()))?;
    let mats = lib
        .get("materials")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let find = |id: &str| {
        mats.iter()
            .find(|m| m.get("material_id").and_then(Value::as_str) == Some(id))
            .cloned()
    };
    let Some(list) = lib.get_mut("materials").and_then(Value::as_array_mut) else {
        return Err(LookError::MissingField {
            material: String::new(),
            field: "materials".into(),
        });
    };
    for m in list.iter_mut() {
        if m.get("kind").and_then(Value::as_str) != Some("anodized") {
            continue;
        }
        let id = m
            .get("material_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let missing = |f: &str| LookError::MissingField {
            material: id.clone(),
            field: f.to_string(),
        };
        let a = m.get("anodized").ok_or_else(|| missing("anodized"))?;
        let sub_id = a
            .get("substrate_material_id")
            .and_then(Value::as_str)
            .ok_or_else(|| missing("anodized.substrate_material_id"))?;
        let sub = find(sub_id).ok_or_else(|| LookError::Unknown(sub_id.to_string()))?;
        let nk = sub
            .pointer("/conductor/nk")
            .ok_or_else(|| missing("substrate conductor.nk"))?;
        let metal = MetalNk {
            wavelength_nm: num_array(nk.get("wavelength_nm"))
                .ok_or_else(|| missing("conductor.nk.wavelength_nm"))?,
            n: num_array(nk.get("n")).ok_or_else(|| missing("conductor.nk.n"))?,
            k: num_array(nk.get("k")).ok_or_else(|| missing("conductor.nk.k"))?,
        };
        if metal.wavelength_nm.is_empty()
            || metal.n.len() != metal.wavelength_nm.len()
            || metal.k.len() != metal.wavelength_nm.len()
        {
            return Err(missing("conductor.nk (one n and k per wavelength)"));
        }
        // `partial_cmp` treats NaN as unordered, so a non-finite sample fails this check too.
        if metal
            .wavelength_nm
            .windows(2)
            .any(|p| p[0].partial_cmp(&p[1]) != Some(std::cmp::Ordering::Less))
            || metal
                .wavelength_nm
                .iter()
                .chain(metal.n.iter())
                .chain(metal.k.iter())
                .any(|x| !x.is_finite())
        {
            return Err(missing("conductor.nk (strictly increasing finite wavelengths)"));
        }
        let coeff = num_array(a.pointer("/oxide/coefficients"))
            .ok_or_else(|| missing("anodized.oxide.coefficients"))?;
        let oxide = Oxide {
            a: coeff.first().copied().unwrap_or(2.2),
            b: coeff.get(1).copied().unwrap_or(0.0),
        };
        let thickness_nm = a
            .get("thickness_nm")
            .and_then(Value::as_f64)
            .ok_or_else(|| missing("anodized.thickness_nm"))?;
        let jitter_nm = a
            .get("thickness_jitter_nm")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if !thickness_nm.is_finite() || !jitter_nm.is_finite() || !oxide.a.is_finite() || !oxide.b.is_finite()
        {
            return Err(missing("anodized.thickness_nm (finite)"));
        }
        let inputs = AnodizeInputs {
            metal,
            oxide,
            thickness_nm,
            jitter_nm,
        };
        let lut = bake_anodize_lut(&inputs);
        let round = |x: f32| (f64::from(x) * 1e5).round() / 1e5;
        let entries: Vec<Value> = lut
            .iter()
            .map(|c| json!([round(c[0]), round(c[1]), round(c[2])]))
            .collect();
        let hash = fnv1a64(&format!("{inputs:?}"));
        if let Some(block) = m.get_mut("anodized").and_then(Value::as_object_mut) {
            block.insert(
                "fresnel_lut".into(),
                json!({ "cos_theta": "uniform_0_1", "entries": entries, "inputs_hash": format!("fnv1a64:{hash:016x}") }),
            );
        }
    }
    let mut text =
        serde_json::to_string_pretty(&sort_keys(lib)).map_err(|e| LookError::Parse(e.to_string()))?;
    text.push('\n');
    Ok(text)
}

/// Rebuilds every object with sorted keys (the plan's JSON rule), whatever map type serde_json uses.
fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut pairs: Vec<(String, Value)> = map.into_iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(pairs.into_iter().map(|(k, v)| (k, sort_keys(v))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// Compares two JSON values with a tolerance on numbers (libm differs in the last bits between
/// platforms). Returns the first differing path.
pub fn json_close(a: &Value, b: &Value, tol: f64) -> Result<(), String> {
    fn walk(a: &Value, b: &Value, tol: f64, path: &mut String) -> Result<(), String> {
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => {
                let (x, y) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
                if (x - y).abs() <= tol {
                    Ok(())
                } else {
                    Err(format!("{path}: {x} vs {y}"))
                }
            }
            (Value::Array(x), Value::Array(y)) => {
                if x.len() != y.len() {
                    return Err(format!("{path}: length {} vs {}", x.len(), y.len()));
                }
                for (i, (p, q)) in x.iter().zip(y).enumerate() {
                    let len = path.len();
                    path.push_str(&format!("[{i}]"));
                    walk(p, q, tol, path)?;
                    path.truncate(len);
                }
                Ok(())
            }
            (Value::Object(x), Value::Object(y)) => {
                if x.len() != y.len() {
                    return Err(format!("{path}: keys {} vs {}", x.len(), y.len()));
                }
                for (k, p) in x {
                    let q = y.get(k).ok_or_else(|| format!("{path}.{k}: missing"))?;
                    let len = path.len();
                    path.push('.');
                    path.push_str(k);
                    walk(p, q, tol, path)?;
                    path.truncate(len);
                }
                Ok(())
            }
            _ if a == b => Ok(()),
            _ => Err(format!("{path}: {a} vs {b}")),
        }
    }
    walk(a, b, tol, &mut String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped library is the bake of its source. `REGIA_LOOKS_BLESS=1` rewrites the file.
    #[test]
    fn builtin_library_is_the_bake_of_its_source() {
        let baked = bake_library(BUILTIN_LIBRARY_SOURCE).expect("bake");
        if std::env::var_os("REGIA_LOOKS_BLESS").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/library-looks.json");
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, &baked).expect("write");
            std::fs::rename(&tmp, &path).expect("rename");
            return;
        }
        let a: Value = serde_json::from_str(&baked).expect("baked parses");
        let b: Value = serde_json::from_str(crate::material::BUILTIN_LIBRARY).expect("builtin parses");
        json_close(&a, &b, 5e-5).expect("data/library-looks.json is stale: rerun with REGIA_LOOKS_BLESS=1");
    }

    #[test]
    fn bake_refuses_a_missing_substrate() {
        let src = r#"{"schema":"jewelry.material_library/1","materials":[{"material_id":"a","kind":"anodized",
            "anodized":{"substrate_material_id":"nope","oxide":{"coefficients":[2.2,0.0]},"thickness_nm":40.0}}]}"#;
        assert_eq!(
            bake_library(src).err().map(|e| e.code()).unwrap_or("ok"),
            "material_unknown"
        );
    }

    #[test]
    fn bake_refuses_duplicate_wavelengths() {
        let src = r#"{"schema":"jewelry.material_library/1","materials":[
            {"material_id":"m","kind":"conductor","conductor":{"nk":{
                "wavelength_nm":[450.0,450.0,650.0],"n":[2.0,2.0,2.0],"k":[3.0,3.0,3.0]
            }}},
            {"material_id":"a","kind":"anodized","anodized":{
                "substrate_material_id":"m",
                "oxide":{"coefficients":[2.2,0.0]},
                "thickness_nm":40.0
            }}
        ]}"#;
        assert_eq!(
            bake_library(src).err().map(|e| e.code()).unwrap_or("ok"),
            "material_missing_field"
        );
    }

    #[test]
    fn json_close_reports_the_path() {
        let a = json!({"x": [1.0, 2.0]});
        let b = json!({"x": [1.0, 2.5]});
        assert_eq!(
            json_close(&a, &b, 1e-6).err().unwrap_or_default(),
            ".x[1]: 2 vs 2.5"
        );
        assert!(json_close(&a, &a, 0.0).is_ok());
    }
}
