// SPDX-License-Identifier: MPL-2.0
//! The contract checks a JSON Schema cannot express, run after a file passes its schema
//! (`docs/formats/jewelry-*.md`, "Checks beyond the schema"). Each returns every problem found,
//! as [`ContractError`]s with stable `contract_*` reasons.
//!
//! The checks index the parsed document with `get`/`as_*` and treat a missing value as "nothing
//! to check": the schema has already refused a wrong shape, and a check must never panic on input
//! (a schema published by another tool may be looser than Regia's).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::hull::{HalfSpace, Solid};
use crate::{ContractError, Interface, jcs, sha256};

/// Most facet planes one part kit may carry (bounds the convexity work on hostile input).
pub const MAX_KIT_FACETS: usize = 32_768;

/// Unit-length tolerance for normals, axes and quaternions.
const UNIT_EPS: f64 = 1e-3;

/// Runs the checks of `interface` on a schema-valid document.
pub fn run(interface: Interface, doc: &Value) -> Vec<ContractError> {
    let mut out = Vec::new();
    match interface {
        Interface::PartKit => part_kit(doc, &mut out),
        Interface::MaterialLibrary => material_library(doc, &mut out),
        Interface::Build => build(doc, &mut out),
        Interface::ProductPackage => product_package(doc, &mut out),
        // The kit builder owns these; its own checker enforces their rules.
        Interface::OptionGraph | Interface::PieceRecipe => {}
    }
    out
}

// ------------------------------------------------------------------ helpers

fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn f(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// A byte count or level: a non-negative integer, which a schema-valid document may also write as
/// a float with a zero fractional part (`10.0`).
fn uint(v: Option<&Value>) -> Option<u64> {
    let n = v?.as_number()?;
    n.as_u64().or_else(|| {
        let f = n.as_f64()?;
        (f >= 0.0 && f.fract() == 0.0 && f < 18_446_744_073_709_551_616.0).then_some(f as u64)
    })
}

fn vec_n<const N: usize>(v: Option<&Value>) -> Option<[f64; N]> {
    let a = v?.as_array()?;
    if a.len() != N {
        return None;
    }
    let mut out = [0.0; N];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x.as_f64()?;
    }
    Some(out)
}

fn norm<const N: usize>(a: [f64; N]) -> f64 {
    a.iter().map(|x| x * x).sum::<f64>().sqrt()
}

/// Reports every value of `ids` (with its pointer) that appears more than once.
fn unique<'a>(out: &mut Vec<ContractError>, what: &str, ids: impl IntoIterator<Item = (String, &'a str)>) {
    let mut seen = BTreeSet::new();
    for (path, id) in ids {
        if !seen.insert(id) {
            out.push(ContractError::invalid(
                "contract_duplicate_id",
                path,
                format!("{what} `{id}` appears twice"),
            ));
        }
    }
}

/// `sha256:<hex>` of the RFC 8785 text of `value`.
pub fn sha256_ref(value: &Value) -> Option<String> {
    let text = jcs::canonical(value)?;
    Some(format!(
        "sha256:{}",
        sha256::hex(&sha256::Sha256::digest(text.as_bytes()))
    ))
}

/// The `build_hash` of a build: sha256 over the RFC 8785 text of
/// `{camera, picks, product_key, recipe_hash, snapshot_id, wear}` (`docs/formats/jewelry-build.md`).
pub fn build_hash(
    camera: &Value,
    picks: &Value,
    product_key: &str,
    recipe_hash: &str,
    snapshot_id: &str,
    wear: &Value,
) -> Option<String> {
    sha256_ref(&serde_json::json!({
        "camera": camera,
        "picks": picks,
        "product_key": product_key,
        "recipe_hash": recipe_hash,
        "snapshot_id": snapshot_id,
        "wear": wear,
    }))
}

/// The `package_hash` of a product package: lowercase hex BLAKE3 of the RFC 8785 text of its
/// `files` array (`docs/formats/jewelry-product-package.md`).
pub fn package_hash(files: &Value) -> Option<String> {
    Some(
        blake3::hash(jcs::canonical(files)?.as_bytes())
            .to_hex()
            .to_string(),
    )
}

fn house_of(key: &str) -> &str {
    key.split_once('/').map_or(key, |(h, _)| h)
}

/// A `house:<slug>` tenant holds only its own house (plan `interfaces.md` § 0.3; `shop:` tenants
/// list theirs in a record outside these files).
fn tenant_allows_house(doc: &Value, out: &mut Vec<ContractError>) {
    if let (Some(tenant), Some(house)) = (s(doc, "tenant_id"), s(doc, "house"))
        && tenant.strip_prefix("house:").is_some_and(|t| t != house)
    {
        out.push(ContractError::invalid(
            "contract_house_not_allowed",
            "/house",
            format!("tenant `{tenant}` may not load house `{house}`"),
        ));
    }
}

// ------------------------------------------------------------------ part kit

fn part_kit(doc: &Value, out: &mut Vec<ContractError>) {
    let parts = arr(doc, "parts");
    let cuts = arr(doc, "gem_cuts");
    unique(
        out,
        "part_id",
        parts
            .iter()
            .enumerate()
            .filter_map(|(i, p)| Some((format!("/parts/{i}/part_id"), s(p, "part_id")?))),
    );
    unique(
        out,
        "cut_id",
        cuts.iter()
            .enumerate()
            .filter_map(|(i, c)| Some((format!("/gem_cuts/{i}/cut_id"), s(c, "cut_id")?))),
    );
    let files = arr(doc, "files");
    unique(
        out,
        "file",
        files
            .iter()
            .enumerate()
            .filter_map(|(i, x)| Some((format!("/files/{i}/path"), s(x, "path")?))),
    );
    let listed: BTreeSet<&str> = files.iter().filter_map(|x| s(x, "path")).collect();
    for (i, p) in parts.iter().enumerate() {
        part(p, &format!("/parts/{i}"), &listed, out);
    }
    for (i, c) in cuts.iter().enumerate() {
        if let Some(mesh) = c.get("mesh_ref").and_then(|m| s(m, "glb"))
            && !listed.contains(mesh)
        {
            out.push(ContractError::missing(
                "contract_unknown_reference",
                format!("/gem_cuts/{i}/mesh_ref/glb"),
                format!("`{mesh}` is not listed in `files` with its sha256"),
            ));
        }
    }
    let total: usize = cuts.iter().map(|c| arr(c, "facets").len()).sum();
    if total > MAX_KIT_FACETS {
        out.push(ContractError::invalid(
            "contract_too_large",
            "/gem_cuts",
            format!("{total} facets in the kit, at most {MAX_KIT_FACETS}"),
        ));
        return;
    }
    for (i, c) in cuts.iter().enumerate() {
        gem_cut(c, &format!("/gem_cuts/{i}"), out);
    }
}

fn part(p: &Value, at: &str, files: &BTreeSet<&str>, out: &mut Vec<ContractError>) {
    if let Some(glb) = s(p, "glb")
        && !files.contains(glb)
    {
        out.push(ContractError::missing(
            "contract_unknown_reference",
            format!("{at}/glb"),
            format!("`{glb}` is not listed in `files` with its sha256"),
        ));
    }
    let sockets = arr(p, "sockets");
    unique(
        out,
        "socket_id",
        sockets
            .iter()
            .enumerate()
            .filter_map(|(i, x)| Some((format!("{at}/sockets/{i}/socket_id"), s(x, "socket_id")?))),
    );
    let slots = arr(p, "material_slots");
    unique(
        out,
        "slot_id",
        slots
            .iter()
            .enumerate()
            .filter_map(|(i, x)| Some((format!("{at}/material_slots/{i}/slot_id"), s(x, "slot_id")?))),
    );
    let params = arr(p, "params");
    unique(
        out,
        "param",
        params
            .iter()
            .enumerate()
            .filter_map(|(i, x)| Some((format!("{at}/params/{i}/name"), s(x, "name")?))),
    );
    if slots.iter().any(|x| s(x, "role") == Some("metal")) && p.get("edge_radius_min_mm").is_none() {
        out.push(ContractError::invalid(
            "contract_missing_field",
            format!("{at}/edge_radius_min_mm"),
            "a part with a metal slot needs `edge_radius_min_mm`",
        ));
    }
    for (i, x) in params.iter().enumerate() {
        param(x, &format!("{at}/params/{i}"), out);
    }
    for (i, x) in sockets.iter().enumerate() {
        let q = vec_n::<4>(x.get("transform").and_then(|t| t.get("rotation_quat")));
        if q.is_some_and(|q| (norm(q) - 1.0).abs() > UNIT_EPS) {
            out.push(ContractError::invalid(
                "contract_bad_transform",
                format!("{at}/sockets/{i}/transform/rotation_quat"),
                "the rotation quaternion is not unit length",
            ));
        }
    }
    if vec_n::<3>(p.get("front_axis")).is_some_and(|a| (norm(a) - 1.0).abs() > UNIT_EPS) {
        out.push(ContractError::invalid(
            "contract_bad_transform",
            format!("{at}/front_axis"),
            "`front_axis` is not unit length",
        ));
    }
    if let Some(b) = p.get("bbox_mm") {
        let (lo, hi) = (vec_n::<3>(b.get("min")), vec_n::<3>(b.get("max")));
        if let (Some(lo), Some(hi)) = (lo, hi)
            && lo.iter().zip(hi).any(|(a, b)| *a > b)
        {
            out.push(ContractError::invalid(
                "contract_bad_range",
                format!("{at}/bbox_mm"),
                "`min` is above `max`",
            ));
        }
    }
    let lods = arr(p, "lods");
    for (i, w) in lods.windows(2).enumerate() {
        let level = |v: &Value| uint(v.get("level"));
        let size = |v: &Value| f(v, "screen_size");
        // Complete pairs only: comparing `Option`s made a missing field a violation on one side of
        // a pair and fine on the other. The schema reports the missing field itself.
        let levels_rise = match (level(&w[0]), level(&w[1])) {
            (Some(a), Some(b)) => b > a,
            _ => true,
        };
        let sizes_fall = match (size(&w[0]), size(&w[1])) {
            (Some(a), Some(b)) => b < a,
            _ => true,
        };
        if !levels_rise || !sizes_fall {
            out.push(ContractError::invalid(
                "contract_bad_lods",
                format!("{at}/lods/{}", i + 1),
                "LOD levels must increase and screen sizes decrease",
            ));
        }
    }
}

fn param(x: &Value, at: &str, out: &mut Vec<ContractError>) {
    let (Some(min), Some(max), Some(step), Some(def)) =
        (f(x, "min"), f(x, "max"), f(x, "step"), f(x, "default"))
    else {
        return;
    };
    if min > max {
        out.push(ContractError::invalid(
            "contract_bad_range",
            format!("{at}/min"),
            "`min` is above `max`",
        ));
        return;
    }
    if def < min || def > max {
        out.push(ContractError::invalid(
            "contract_bad_range",
            format!("{at}/default"),
            "`default` is outside min..max",
        ));
    }
    if max > min && step > max - min {
        out.push(ContractError::invalid(
            "contract_bad_range",
            format!("{at}/step"),
            "`step` is larger than the range",
        ));
    }
    match s(x, "realized_by") {
        Some("variant_mesh") => {
            let variants = arr(x, "variants");
            if variants.is_empty() {
                out.push(ContractError::invalid(
                    "contract_missing_field",
                    format!("{at}/variants"),
                    "a `variant_mesh` parameter lists its variants",
                ));
            }
            for (i, v) in variants.iter().enumerate() {
                if f(v, "value").is_some_and(|value| value < min || value > max) {
                    out.push(ContractError::invalid(
                        "contract_bad_range",
                        format!("{at}/variants/{i}/value"),
                        "a variant value is outside min..max",
                    ));
                }
            }
        }
        Some("morph" | "bone_scale") if s(x, "target").is_none() => {
            out.push(ContractError::invalid(
                "contract_missing_field",
                format!("{at}/target"),
                "a `morph` or `bone_scale` parameter names its target",
            ));
        }
        _ => {}
    }
}

/// Relative tolerance of the cut's size against `size_mm` (1 %, at least 0.01 mm).
const SIZE_TOL: f64 = 0.01;
/// Tolerance of the measured crown height and pavilion depth (percentage points).
const PROPORTION_TOL: f64 = 1.0;
/// The most facets a convex cut may list (`contract_bad_count` above it). The solid check clips every
/// plane against the solid, so its work grows with the square of the count; real cuts have tens to a
/// few hundred facets, and a cut with more is a mesh (`mesh_ref`).
pub const MAX_CUT_FACETS: usize = 512;

fn gem_cut(c: &Value, at: &str, out: &mut Vec<ContractError>) {
    let facets = arr(c, "facets");
    let convex = c.get("convex").and_then(Value::as_bool).unwrap_or(false);
    let mesh = c.get("mesh_ref").is_some_and(|m| !m.is_null());
    if !convex && !mesh {
        out.push(ContractError::invalid(
            "contract_missing_field",
            format!("{at}/mesh_ref"),
            "a concave cut needs `mesh_ref` (the convex trace cannot render it)",
        ));
    }
    if facets.is_empty() {
        if !mesh {
            out.push(ContractError::invalid(
                "contract_cut_no_facets",
                format!("{at}/facets"),
                "a cut needs facets or a mesh",
            ));
        }
        return;
    }
    if facets.len() > MAX_CUT_FACETS {
        out.push(ContractError::invalid(
            "contract_bad_count",
            format!("{at}/facets"),
            format!(
                "{} facets; a cut lists at most {MAX_CUT_FACETS} (a finer shape is a mesh)",
                facets.len()
            ),
        ));
        return;
    }
    let mut planes = Vec::with_capacity(facets.len());
    let mut normals_ok = true;
    let mut planes_complete = true;
    for (i, x) in facets.iter().enumerate() {
        let (Some(n), Some(d)) = (vec_n::<3>(x.get("normal")), f(x, "offset_mm")) else {
            // Returning here used to skip the closure proof for every facet of the cut, so a
            // convex cut with one plane left out was accepted by the contract checks (the path a
            // build without the schema validator still runs).
            planes_complete = false;
            out.push(ContractError::invalid(
                "contract_missing_field",
                format!("{at}/facets/{i}"),
                "a facet needs a unit `normal` and `offset_mm`",
            ));
            continue;
        };
        if (norm(n) - 1.0).abs() > UNIT_EPS {
            normals_ok = false;
            out.push(ContractError::invalid(
                "contract_bad_normal",
                format!("{at}/facets/{i}/normal"),
                format!("|normal| = {:.4}, facet normals are unit length", norm(n)),
            ));
        }
        planes.push(HalfSpace { normal: n, offset: d });
    }
    if !convex || !normals_ok || !planes_complete {
        return;
    }
    let size: Vec<f64> = arr(c, "size_mm").iter().filter_map(Value::as_f64).collect();
    let length = size.first().copied().unwrap_or(1.0);
    let width = size.get(1).copied().unwrap_or(length);
    let half = 4.0
        * length
            .max(width)
            .max(planes.iter().map(|p| p.offset.abs()).fold(0.0, f64::max));
    let mut solid = Solid::cube(half.max(1.0));
    for (i, p) in planes.iter().enumerate() {
        solid.clip(*p, i);
    }
    if solid.faces.is_empty() || solid.volume() <= 0.0 {
        out.push(ContractError::invalid(
            "contract_cut_empty",
            format!("{at}/facets"),
            "the facets leave no solid",
        ));
        return;
    }
    if solid.touches_box() {
        out.push(ContractError::invalid(
            "contract_cut_unbounded",
            format!("{at}/facets"),
            "the facets do not close a solid (convex is true)",
        ));
        return;
    }
    for i in 0..planes.len() {
        if solid.facet_area(i) <= 1e-9 {
            out.push(ContractError::invalid(
                "contract_cut_redundant_facet",
                format!("{at}/facets/{i}"),
                "the facet touches the solid in no face",
            ));
        }
    }
    let Some((lo, hi)) = solid.bounds() else {
        return;
    };
    // Gem-local frame: +y along the length, +x across the width, +z toward the table.
    let (got_l, got_w) = (hi[1] - lo[1], hi[0] - lo[0]);
    let off = |got: f64, want: f64| (got - want).abs() > (want * SIZE_TOL).max(0.01);
    if off(got_l, length) || off(got_w, width) {
        out.push(ContractError::invalid(
            "contract_cut_size_mismatch",
            format!("{at}/size_mm"),
            format!("the facets give {got_l:.3} x {got_w:.3} mm, size_mm says {length} x {width}"),
        ));
    }
    let girdle: Vec<usize> = facets
        .iter()
        .enumerate()
        .filter(|(_, x)| s(x, "kind") == Some("girdle"))
        .map(|(i, _)| i)
        .collect();
    let Some((g_lo, g_hi)) = solid.z_range_of(|i| girdle.contains(&i)) else {
        return;
    };
    let props = c.get("proportions");
    let measured = [
        ("crown_height_pct", (hi[2] - g_hi) / width * 100.0),
        ("pavilion_depth_pct", (g_lo - lo[2]) / width * 100.0),
    ];
    for (key, got) in measured {
        if let Some(want) = props.and_then(|p| f(p, key))
            && (got - want).abs() > PROPORTION_TOL
        {
            out.push(ContractError::invalid(
                "contract_cut_proportions_mismatch",
                format!("{at}/proportions/{key}"),
                format!("the facets give {got:.2} %, proportions say {want} %"),
            ));
        }
    }
}

// ------------------------------------------------------------------ material library

fn material_library(doc: &Value, out: &mut Vec<ContractError>) {
    let entries = arr(doc, "materials");
    unique(
        out,
        "material_id",
        entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| Some((format!("/materials/{i}/material_id"), s(e, "material_id")?))),
    );
    let kinds: BTreeMap<&str, &str> = entries
        .iter()
        .filter_map(|e| Some((s(e, "material_id")?, s(e, "kind")?)))
        .collect();
    let mut facets: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        let at = format!("/materials/{i}");
        for (j, label) in arr(e, "catalogue_facets")
            .iter()
            .filter_map(Value::as_str)
            .enumerate()
        {
            // The first claimant keeps the facet: `insert` would hand it to each later one, and a
            // third claim was then reported against the second material.
            let first = *facets.entry(label).or_insert(i);
            if first != i {
                out.push(ContractError::invalid(
                    "contract_duplicate_facet",
                    format!("{at}/catalogue_facets/{j}"),
                    format!("`{label}` is already claimed by /materials/{first}"),
                ));
            }
        }
        if let Some(nk) = e
            .get("conductor")
            .and_then(|c| c.get("nk"))
            .filter(|v| !v.is_null())
        {
            spectrum(nk, &["n", "k"], &format!("{at}/conductor/nk"), out);
        }
        if let Some(a) = e.get("anodized")
            && let Some(sub) = s(a, "substrate_material_id")
        {
            match kinds.get(sub) {
                None => out.push(ContractError::missing(
                    "contract_unknown_reference",
                    format!("{at}/anodized/substrate_material_id"),
                    format!("no material `{sub}` in the library"),
                )),
                Some(&"conductor") => {}
                Some(kind) => out.push(ContractError::invalid(
                    "contract_wrong_kind",
                    format!("{at}/anodized/substrate_material_id"),
                    format!("`{sub}` is a {kind}, an anodize substrate is a conductor"),
                )),
            }
        }
        if let Some(g) = e.get("gem_dielectric") {
            let given = |k: &str| g.get(k).is_some_and(|v| !v.is_null());
            if !given("tint_rgb_linear") && !given("attenuation_rgb_per_mm") && !given("absorption_spectrum")
            {
                out.push(ContractError::invalid(
                    "contract_missing_field",
                    format!("{at}/gem_dielectric/tint_rgb_linear"),
                    "a gem names its color: `tint_rgb_linear`, `attenuation_rgb_per_mm` or `absorption_spectrum`",
                ));
            }
            if let Some(sp) = g.get("absorption_spectrum").filter(|v| !v.is_null()) {
                spectrum(
                    sp,
                    &["per_mm"],
                    &format!("{at}/gem_dielectric/absorption_spectrum"),
                    out,
                );
            }
        }
    }
}

fn spectrum(v: &Value, columns: &[&str], at: &str, out: &mut Vec<ContractError>) {
    let wl: Vec<f64> = arr(v, "wavelength_nm").iter().filter_map(Value::as_f64).collect();
    if wl.windows(2).any(|w| w[1] <= w[0]) {
        out.push(ContractError::invalid(
            "contract_bad_spectrum",
            format!("{at}/wavelength_nm"),
            "wavelengths must strictly increase",
        ));
    }
    for c in columns {
        if arr(v, c).len() != wl.len() {
            out.push(ContractError::invalid(
                "contract_bad_spectrum",
                format!("{at}/{c}"),
                format!(
                    "`{c}` has {} values for {} wavelengths",
                    arr(v, c).len(),
                    wl.len()
                ),
            ));
        }
    }
}

// ------------------------------------------------------------------ build

fn build(b: &Value, out: &mut Vec<ContractError>) {
    tenant_allows_house(b, out);
    let house = s(b, "house").unwrap_or_default();
    for key in ["product_key", "design_key"] {
        if let Some(k) = s(b, key)
            && house_of(k) != house
        {
            out.push(ContractError::invalid(
                "contract_key_mismatch",
                format!("/{key}"),
                format!("`{k}` is not a key of house `{house}`"),
            ));
        }
    }
    let recipe = b.get("recipe");
    if let (Some(d), Some(r)) = (s(b, "design_key"), recipe.and_then(|r| s(r, "design_key")))
        && d != r
    {
        out.push(ContractError::invalid(
            "contract_key_mismatch",
            "/recipe/design_key",
            format!("the recipe is for `{r}`, the build for `{d}`"),
        ));
    }
    if let Some(r) = b.get("resolution") {
        resolution(r, "/resolution", out);
        for (key, of) in [("sku", "sku"), ("house_variant_id", "variant_key")] {
            if b.get(key).is_some_and(|given| Some(given) != r.get(of)) {
                out.push(ContractError::invalid(
                    "contract_inconsistent_resolution",
                    format!("/{key}"),
                    format!("differs from `resolution.{of}`"),
                ));
            }
        }
    }
    let (Some(camera), Some(picks), Some(pk), Some(rh), Some(snap), Some(wear)) = (
        b.get("camera"),
        b.get("picks"),
        s(b, "product_key"),
        recipe.and_then(|r| s(r, "hash")),
        s(b, "snapshot_id"),
        b.get("wear"),
    ) else {
        return;
    };
    hash_matches(
        s(b, "build_hash"),
        build_hash(camera, picks, pk, rh, snap, wear),
        "/build_hash",
        out,
    );
}

fn hash_matches(given: Option<&str>, want: Option<String>, at: &str, out: &mut Vec<ContractError>) {
    if let (Some(given), Some(want)) = (given, want)
        && given != want
    {
        out.push(ContractError::invalid(
            "contract_hash_mismatch",
            at,
            format!("is {given}, the covered fields hash to {want}"),
        ));
    }
}

fn resolution(r: &Value, at: &str, out: &mut Vec<ContractError>) {
    let orderable = r.get("orderable").and_then(Value::as_bool).unwrap_or(false);
    let null = |k: &str| r.get(k).is_none_or(Value::is_null);
    let lines = arr(r, "cart_lines");
    let mut bad = |key: &str, why: &str| {
        out.push(ContractError::invalid(
            "contract_inconsistent_resolution",
            format!("{at}/{key}"),
            why,
        ));
    };
    if orderable {
        if null("variant_key") {
            bad("variant_key", "an orderable resolution names its variant");
        }
        if null("sku") {
            bad("sku", "an orderable resolution names its SKU");
        }
        let reasons = arr(r, "reasons");
        // An incomplete listing carries `incomplete` as a caveat next to an orderable
        // resolution (spec 42 § 3.2); it is the only reason such a build may list.
        let only_incomplete =
            reasons.len() == 1 && reasons[0].get("code").and_then(Value::as_str) == Some("incomplete");
        if !reasons.is_empty() && !only_incomplete {
            bad("reasons", "an orderable resolution has no refusal reasons");
        }
        match s(r, "cart_mode") {
            Some("shopify_lines") if lines.is_empty() => {
                bad("cart_lines", "`shopify_lines` needs cart lines")
            }
            Some("manual_line") if null("made_to_order_line") => {
                bad("made_to_order_line", "`manual_line` needs the made-to-order line")
            }
            _ => {}
        }
    } else if arr(r, "reasons").is_empty() {
        bad("reasons", "a resolution that is not orderable says why");
    }
    if s(r, "cart_mode") == Some("manual_line") && !lines.is_empty() {
        bad("cart_lines", "`manual_line` carries no cart lines");
    }
}

// ------------------------------------------------------------------ product package

/// The file kinds a package holds exactly one of.
const PACKAGE_SINGLETONS: [&str; 3] = ["recipe", "options", "materials"];

/// Field names that never leave the shop in a package (`interfaces.md` § 0.4), wherever they sit.
const COST_FIELDS: [&str; 8] = [
    "cost",
    "dealer_cost",
    "dealer_price",
    "markup",
    "markup_rules",
    "price",
    "price_delta",
    "retail",
];

fn product_package(doc: &Value, out: &mut Vec<ContractError>) {
    tenant_allows_house(doc, out);
    let house = s(doc, "house").unwrap_or_default();
    for key in ["product_key", "design_key"] {
        if let Some(k) = s(doc, key)
            && house_of(k) != house
        {
            out.push(ContractError::invalid(
                "contract_key_mismatch",
                format!("/{key}"),
                format!("`{k}` is not a key of house `{house}`"),
            ));
        }
    }
    if let (Some(d), Some(r)) = (
        s(doc, "design_key"),
        doc.get("recipe").and_then(|r| s(r, "design_key")),
    ) && d != r
    {
        out.push(ContractError::invalid(
            "contract_key_mismatch",
            "/recipe/design_key",
            format!("the recipe is for `{r}`, the package for `{d}`"),
        ));
    }
    let files = arr(doc, "files");
    unique(
        out,
        "file",
        files
            .iter()
            .enumerate()
            .filter_map(|(i, x)| Some((format!("/files/{i}/name"), s(x, "name")?))),
    );
    let count = |kind: &str| files.iter().filter(|x| s(x, "kind") == Some(kind)).count();
    for kind in PACKAGE_SINGLETONS {
        if count(kind) != 1 {
            out.push(ContractError::invalid(
                "contract_package_incomplete",
                "/files",
                format!(
                    "{} files of kind `{kind}`, a package holds exactly one",
                    count(kind)
                ),
            ));
        }
    }
    if count("mesh") == 0 {
        out.push(ContractError::invalid(
            "contract_package_incomplete",
            "/files",
            "a package holds at least one file of kind `mesh`",
        ));
    }
    // `size` is a number, not an allocation, so two `u64::MAX` entries overflow a wrapping sum
    // and panic a debug build. The total then cannot match.
    let mut sum: u64 = 0;
    let mut overflow = false;
    for size in files.iter().filter_map(|x| uint(x.get("size"))) {
        match sum.checked_add(size) {
            Some(n) => sum = n,
            None => {
                overflow = true;
                break;
            }
        }
    }
    let listed = uint(doc.get("sizes").and_then(|x| x.get("total")));
    if overflow || listed != Some(sum) {
        let message = if overflow {
            "the listed file sizes overflow a byte count".to_owned()
        } else {
            format!("the listed files sum to {sum} bytes")
        };
        out.push(ContractError::invalid(
            "contract_package_size_mismatch",
            "/sizes/total",
            message,
        ));
    }
    if let Some(list) = doc.get("files") {
        hash_matches(s(doc, "package_hash"), package_hash(list), "/package_hash", out);
    }
}

/// The first cost, price or markup field in `value`, as a JSON pointer. Only field names count:
/// the parser's nesting limit bounds the recursion.
pub fn find_cost_field(value: &Value) -> Option<String> {
    fn walk(value: &Value, at: &str) -> Option<String> {
        match value {
            Value::Object(map) => map.iter().find_map(|(key, v)| {
                let here = format!("{at}/{key}");
                if COST_FIELDS.contains(&key.to_ascii_lowercase().as_str()) {
                    Some(here)
                } else {
                    walk(v, &here)
                }
            }),
            Value::Array(items) => items
                .iter()
                .enumerate()
                .find_map(|(i, v)| walk(v, &format!("{at}/{i}"))),
            _ => None,
        }
    }
    walk(value, "")
}

/// The JSON text of `bytes` when it may be a document that can hold a field: after a UTF-8 byte
/// order mark (which the parser refuses, so it is cut off) and any JSON whitespace, the first byte
/// opens an object or an array. A cheap test, so a mesh or a texture is never handed to the parser.
fn json_body(bytes: &[u8]) -> Option<&[u8]> {
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    matches!(
        body.iter().find(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r')),
        Some(b'{' | b'[')
    )
    .then_some(body)
}

/// Checks a package's files against its (schema-valid) manifest: each listed file exists with its
/// size and BLAKE3 hash, and no JSON file holds a cost, price or markup field. `read` returns a
/// file's bytes by its manifest name, `None` when absent, so the same check runs over a folder, a
/// fetch or a test's map.
pub fn package_files(manifest: &Value, read: &mut dyn FnMut(&str) -> Option<Vec<u8>>) -> Vec<ContractError> {
    let mut out = Vec::new();
    for (i, file) in arr(manifest, "files").iter().enumerate() {
        let at = format!("/files/{i}");
        let Some(name) = s(file, "name") else {
            continue;
        };
        let Some(bytes) = read(name) else {
            out.push(ContractError::missing(
                "contract_package_file_missing",
                format!("{at}/name"),
                format!("`{name}` is absent"),
            ));
            continue;
        };
        if uint(file.get("size")) != Some(bytes.len() as u64) {
            out.push(ContractError::invalid(
                "contract_package_file_mismatch",
                format!("{at}/size"),
                format!("`{name}` is {} bytes", bytes.len()),
            ));
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        if s(file, "blake3") != Some(hash.as_str()) {
            out.push(ContractError::invalid(
                "contract_package_file_mismatch",
                format!("{at}/blake3"),
                format!("`{name}` hashes to {hash}"),
            ));
        }
        // Every file whose bytes are a JSON object or array, whatever its name: the manifest's
        // name pattern allows any case and extension, so `options.JSON` (or a `.txt`) holding a
        // price used to pass unscanned.
        if let Some(body) = json_body(&bytes)
            && let Ok(value) = serde_json::from_slice::<Value>(body)
            && let Some(field) = find_cost_field(&value)
        {
            out.push(ContractError::invalid(
                "contract_package_has_cost",
                at,
                format!("`{name}` holds {field}: cost and prices never ship in a package"),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn manifest_of(files: &[(&str, &[u8])]) -> Value {
        let rows: Vec<Value> = files
            .iter()
            .map(|(name, bytes)| {
                json!({
                    "name": name,
                    "size": bytes.len(),
                    "blake3": blake3::hash(bytes).to_hex().to_string(),
                })
            })
            .collect();
        json!({ "files": rows })
    }

    /// Audit (sprint-corr-configurator): only names ending in lowercase `.json` were scanned for
    /// costs, but the manifest's name pattern allows any case and extension, so a price shipped
    /// under `options.JSON` passed. Every JSON body is scanned now; binary files are not parsed.
    #[test]
    fn a_cost_is_found_whatever_the_file_is_called() {
        let costed = br#"{"variants": [{"variant_key": "1", "price": 12}]}"#;
        let with_bom = [b"\xEF\xBB\xBF".as_slice(), b" \n[{\"cost\": 3}]"].concat();
        for (name, bytes) in [
            ("options.JSON", costed.as_slice()),
            ("options.Json", costed.as_slice()),
            ("notes.txt", costed.as_slice()),
            ("recipe", costed.as_slice()),
            ("materials.json", with_bom.as_slice()),
        ] {
            let manifest = manifest_of(&[(name, bytes)]);
            let errs = package_files(&manifest, &mut |_| Some(bytes.to_vec()));
            assert!(
                errs.iter().any(|e| e.reason == "contract_package_has_cost"),
                "{name}: {errs:?}"
            );
        }
        let mesh: &[u8] = b"\x00\x01price\x02";
        let manifest = manifest_of(&[("mesh.rvm", mesh), ("clean.JSON", b"{\"a\": 1}")]);
        let errs = package_files(&manifest, &mut |n| {
            Some(if n == "mesh.rvm" {
                mesh.to_vec()
            } else {
                b"{\"a\": 1}".to_vec()
            })
        });
        assert_eq!(errs, Vec::new());
    }

    /// Guard (sprint-corr-configurator audit): `offset_mm` has no bound in the schema, and an offset
    /// of 1e308 makes the start box infinite. The cut must still be refused (today as
    /// `contract_cut_empty` or `contract_cut_redundant_facet`), never passed.
    #[test]
    fn a_huge_facet_offset_is_refused_not_passed() {
        let kit: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/jewelry-part-kit/kit-shared-basic.json"
        ))
        .expect("fixture");
        assert_eq!(run(Interface::PartKit, &kit), Vec::new(), "the fixture is clean");
        for offset in [1e308, -1e308, 1e200] {
            let mut bad = kit.clone();
            bad["gem_cuts"][0]["facets"][0]["offset_mm"] = json!(offset);
            let errs = run(Interface::PartKit, &bad);
            assert!(!errs.is_empty(), "offset {offset:e} passed");
        }
    }

    /// Audit LOW (sprint-corr-configurator): `BTreeMap::insert` returned the previous claimant and
    /// replaced it, so a third claim of one facet was reported against the second material, not
    /// the first that owns it.
    #[test]
    fn every_later_claim_of_a_facet_names_the_first_claimant() {
        let doc = json!({"materials": [
            {"material_id": "a", "kind": "dielectric", "catalogue_facets": ["Metal=Gold"]},
            {"material_id": "b", "kind": "dielectric", "catalogue_facets": ["Metal=Gold"]},
            {"material_id": "c", "kind": "dielectric", "catalogue_facets": ["Metal=Gold"]},
        ]});
        let mut out = Vec::new();
        material_library(&doc, &mut out);
        let dup: Vec<&ContractError> = out
            .iter()
            .filter(|e| e.reason == "contract_duplicate_facet")
            .collect();
        assert_eq!(dup.len(), 2, "{out:?}");
        assert_eq!(dup[0].path, "/materials/1/catalogue_facets/0");
        assert_eq!(dup[1].path, "/materials/2/catalogue_facets/0");
        for e in dup {
            assert!(e.message.contains("/materials/0"), "{}", e.message);
        }
    }

    /// Audit LOW (sprint-corr-configurator): a schema-valid integer written `10.0` was read by
    /// `as_u64` as absent, so the size total and the per-file size reported a spurious mismatch.
    #[test]
    fn an_integral_float_size_counts_as_its_integer() {
        let doc = json!({
            "files": [
                {"name": "a.rvm", "kind": "mesh", "size": 10.0},
                {"name": "b.rvm", "kind": "mesh", "size": 6},
            ],
            "sizes": {"total": 16.0},
        });
        let mut out = Vec::new();
        product_package(&doc, &mut out);
        assert!(
            !out.iter().any(|e| e.reason == "contract_package_size_mismatch"),
            "{out:?}"
        );
        let body = b"0123456789";
        let mut manifest = manifest_of(&[("a.rvm", body)]);
        manifest["files"][0]["size"] = json!(10.0);
        let errs = package_files(&manifest, &mut |_| Some(body.to_vec()));
        assert!(errs.is_empty(), "{errs:?}");
        // A fractional or negative size is still no byte count.
        manifest["files"][0]["size"] = json!(10.5);
        let errs = package_files(&manifest, &mut |_| Some(body.to_vec()));
        assert!(errs.iter().any(|e| e.path == "/files/0/size"), "{errs:?}");
    }

    /// Audit LOW (sprint-corr-configurator): the LOD order check compared `Option`s, so a missing
    /// `screen_size` or `level` was a violation on one side of a pair and silently fine on the
    /// other. The schema reports a missing field; the order check judges complete pairs only.
    #[test]
    fn the_lod_order_check_is_symmetric_about_missing_fields() {
        let run_lods = |lods: Value| {
            let mut out = Vec::new();
            part(&json!({"lods": lods}), "/parts/0", &BTreeSet::new(), &mut out);
            out.into_iter()
                .filter(|e| e.reason == "contract_bad_lods")
                .count()
        };
        let first_missing = json!([{"level": 0}, {"level": 1, "screen_size": 0.5}]);
        let second_missing = json!([{"level": 0, "screen_size": 0.5}, {"level": 1}]);
        assert_eq!(run_lods(first_missing), run_lods(second_missing));
        assert_eq!(
            run_lods(json!([{"screen_size": 0.5}, {"level": 1, "screen_size": 0.2}])),
            0
        );
        assert_eq!(
            run_lods(json!([{"level": 0, "screen_size": 0.5}, {"screen_size": 0.2}])),
            0
        );
        // Complete pairs are still judged.
        assert_eq!(
            run_lods(json!([{"level": 1, "screen_size": 0.5}, {"level": 0, "screen_size": 0.2}])),
            1
        );
        assert_eq!(
            run_lods(json!([{"level": 0, "screen_size": 0.2}, {"level": 1, "screen_size": 0.5}])),
            1
        );
    }
}
