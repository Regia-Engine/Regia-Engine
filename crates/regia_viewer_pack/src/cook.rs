// SPDX-License-Identifier: MPL-2.0
//! The cook: `regia viewer pack <kit> <recipe> -o <dir>` (ADR-2540).
//!
//! Reads a part kit folder (`kit.json` of `jewelry.part_kit/1` plus its GLB) and a piece recipe
//! (`jewelry.piece_recipe/1`), and writes a product package: every part the recipe or its part
//! swaps can show, every gem cut at every size its value drives offer, the option data and the
//! material entries, a manifest with BLAKE3 hashes, and a size report. Hidden standard parts
//! (posts, pins, end bases) take the kit's cheapest LOD. Output is byte-identical for identical
//! inputs (no timestamps).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::geom::{self, Facet};
use crate::mesh::{self, MeshData};
use crate::{
    FileEntry, MANIFEST_FILE, MATERIALS_FILE, MESH_FILE, OPTIONS_FILE, PackError, RECIPE_FILE, SCHEMA,
    SizeReport, SizeRow, blake3_hex, canonical_json, package_hash, safe_path, strip_cost,
};

/// Largest `kit.json`, recipe, option record or material library read.
pub const MAX_JSON_BYTES: u64 = 16 << 20;
/// Largest texture source read.
pub const MAX_TEXTURE_BYTES: u64 = 64 << 20;
/// Most gem sizes cooked per slot.
pub const MAX_SIZES_PER_SLOT: usize = 64;
/// Part kinds that sit behind the piece and take the kit's cheapest LOD.
pub const HIDDEN_KINDS: [&str; 3] = ["post", "threadless_pin", "end_base"];

/// Cook options.
#[derive(Clone, Debug)]
pub struct CookOptions {
    /// More kit folders (a design kit plus the shared kit, for example).
    pub extra_kits: Vec<PathBuf>,
    /// The product's option record (Tincal's option graph product file); copied without cost.
    pub option_graph: Option<PathBuf>,
    /// A material library (`jewelry.material_library/1`); the entries the drives can pick are kept.
    pub materials: Option<PathBuf>,
    /// Longest texture side written; larger sources are scaled down.
    pub max_texture_px: u32,
    /// Measure gzip sizes in the report.
    pub gzip: bool,
}

impl Default for CookOptions {
    fn default() -> Self {
        CookOptions {
            extra_kits: Vec::new(),
            option_graph: None,
            materials: None,
            max_texture_px: 512,
            gzip: true,
        }
    }
}

/// What a cook produced.
#[derive(Clone, Debug)]
pub struct CookResult {
    /// The manifest written.
    pub manifest: Value,
    /// The size report.
    pub report: SizeReport,
    /// Things that did not stop the cook (a generated cut, a missing LOD, an unmatched material).
    pub warnings: Vec<String>,
}

fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, PackError> {
    let meta =
        std::fs::metadata(path).map_err(|e| PackError::new("pack_io", format!("{}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(PackError::new(
            "pack_io",
            format!("{} is not a file", path.display()),
        ));
    }
    if meta.len() > cap {
        return Err(PackError::new(
            "pack_too_large",
            format!("{} is {} bytes", path.display(), meta.len()),
        ));
    }
    std::fs::read(path).map_err(|e| PackError::new("pack_io", format!("{}: {e}", path.display())))
}

fn read_json(path: &Path, code: &'static str) -> Result<Value, PackError> {
    let b = read_capped(path, MAX_JSON_BYTES)?;
    serde_json::from_slice(&b).map_err(|e| PackError::new(code, format!("{}: {e}", path.display())))
}

/// A file named inside a kit folder: a plain relative name that stays inside it.
fn kit_file(dir: &Path, name: &str) -> Result<PathBuf, PackError> {
    let ok = !name.is_empty()
        && name.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
        && !name.contains(['\\', ':', '\0']);
    if !ok {
        return Err(PackError::new(
            "pack_bad_kit",
            format!("kit file name {name:?} leaves the kit"),
        ));
    }
    let p = dir.join(name);
    let canon = p
        .canonicalize()
        .map_err(|e| PackError::new("pack_bad_kit", format!("{}: {e}", p.display())))?;
    let root = dir
        .canonicalize()
        .map_err(|e| PackError::new("pack_io", format!("{}: {e}", dir.display())))?;
    if !canon.starts_with(&root) {
        return Err(PackError::new(
            "pack_bad_kit",
            format!("{name:?} links outside the kit"),
        ));
    }
    Ok(canon)
}

struct Kit {
    dir: PathBuf,
    doc: Value,
    house: String,
    id: String,
    version: u64,
}

impl Kit {
    fn open(dir: &Path) -> Result<Kit, PackError> {
        let doc = read_json(&dir.join("kit.json"), "pack_bad_kit")?;
        let bad = |m: &str| PackError::new("pack_bad_kit", format!("{}: {m}", dir.display()));
        if doc.get("schema").and_then(Value::as_str) != Some("jewelry.part_kit/1") {
            return Err(bad("kit.json schema is not jewelry.part_kit/1"));
        }
        let s = |k: &str| {
            doc.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| bad(&format!("kit.json has no {k}")))
        };
        let (house, id) = (s("house")?, s("kit_id")?);
        let version = doc.get("kit_version").and_then(Value::as_u64).unwrap_or(1);
        if !doc.get("parts").is_some_and(Value::is_array) {
            return Err(bad("kit.json has no parts"));
        }
        Ok(Kit {
            dir: dir.to_path_buf(),
            doc,
            house,
            id,
            version,
        })
    }

    fn part(&self, part_id: &str) -> Option<&Value> {
        self.doc["parts"]
            .as_array()?
            .iter()
            .find(|p| p["part_id"] == part_id)
    }

    fn cut(&self, cut_type: &str, l: f32, w: f32) -> Option<&Value> {
        self.doc.get("gem_cuts")?.as_array()?.iter().find(|c| {
            let size = sizes_of(&c["size_mm"]);
            c["cut_type"] == cut_type
                && size.is_some_and(|(a, b)| (a - l).abs() < 0.005 && (b - w).abs() < 0.005)
        })
    }
}

fn sizes_of(v: &Value) -> Option<(f32, f32)> {
    let pair = match v {
        Value::Number(n) => n.as_f64().map(|d| (d as f32, d as f32)),
        Value::Array(a) if a.len() == 1 => a[0].as_f64().map(|d| (d as f32, d as f32)),
        // A kit cut may carry depth as a third component. Length and width are the first two.
        Value::Array(a) if a.len() == 2 || a.len() == 3 => {
            Some((a[0].as_f64()? as f32, a[1].as_f64()? as f32))
        }
        _ => None,
    };
    pair.filter(|(a, b)| a.is_finite() && b.is_finite())
}

/// The part's first material slot: a string, or a `{slot_id, role}` object. Missing means `body`.
fn first_material_slot(part: &Value) -> String {
    let raw = part["material_slots"].as_array().and_then(|a| a.first());
    let name = raw.and_then(|s| {
        s.as_str()
            .map(str::to_string)
            .or_else(|| s.get("slot_id").and_then(Value::as_str).map(str::to_string))
    });
    match name {
        Some(s) if !s.is_empty() => s,
        _ => "body".into(),
    }
}

/// Quarter turn about +z: length moves from +x to +y, matching `regia_looks::GemCut`.
fn gem_local_facets(facets: Vec<Facet>) -> Vec<Facet> {
    facets
        .into_iter()
        .map(|mut f| {
            let n = f.normal;
            f.normal = [-n[1], n[0], n[2]];
            f
        })
        .collect()
}

/// `<house>/<part_id>@<version>` → (house, part_id).
fn parse_ref(r: &str) -> Option<(&str, &str)> {
    let (house, rest) = r.split_once('/')?;
    let part = rest.split_once('@').map_or(rest, |(p, _)| p);
    (!house.is_empty() && !part.is_empty()).then_some((house, part))
}

fn size_label(l: f32, w: f32) -> String {
    format!("{l}x{w}")
}

/// Every value a recipe variable can take: its base value plus every value drive's map.
fn variable_values(recipe: &Value, var: &str) -> Vec<f32> {
    let mut out: Vec<f32> = recipe["values"]
        .get(var)
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .into_iter()
        .collect();
    for d in recipe["drives"].as_array().into_iter().flatten() {
        if d["to"]["value"] == var {
            for v in d["to"]["map"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.values())
                .filter_map(Value::as_f64)
            {
                out.push(v as f32);
            }
        }
    }
    out.sort_by(f32::total_cmp);
    out.dedup();
    out
}

/// Resolve one `size_mm` entry: a number, or `$var` with every value the variable can take.
fn entry_values(recipe: &Value, e: &Value) -> Result<(Option<String>, Vec<f32>), PackError> {
    match e {
        Value::Number(n) => Ok((None, n.as_f64().map(|v| vec![v as f32]).unwrap_or_default())),
        Value::String(s) if s.starts_with('$') => {
            let var = s.strip_prefix('$').unwrap_or(s);
            let vals = variable_values(recipe, var);
            if vals.is_empty() {
                return Err(PackError::new(
                    "pack_bad_recipe",
                    format!("size variable {s} has no value"),
                ));
            }
            Ok((Some(var.to_string()), vals))
        }
        other => Err(PackError::new(
            "pack_bad_recipe",
            format!("size_mm entry {other} is not a number or $name"),
        )),
    }
}

/// The (length, width) sizes a gem slot can show, with the variable values that give each.
/// Variables move together when one drive group sets them all (as `Gem Size` sets `mq_l` and
/// `mq_w`); otherwise every combination is cooked.
/// One size a gem slot can show: (length mm, width mm, the variable values that give it).
type SlotSize = (f32, f32, BTreeMap<String, f32>);

fn slot_sizes(recipe: &Value, slot: &Value) -> Result<Vec<SlotSize>, PackError> {
    let entries: Vec<Value> = match &slot["size_mm"] {
        Value::Array(a) if a.len() == 2 => a.clone(),
        Value::Array(a) if a.len() == 1 => vec![a[0].clone(), a[0].clone()],
        v @ (Value::Number(_) | Value::String(_)) => vec![v.clone(), v.clone()],
        other => {
            return Err(PackError::new(
                "pack_bad_recipe",
                format!("gem slot size_mm {other}"),
            ));
        }
    };
    let (vl, ll) = entry_values(recipe, &entries[0])?;
    let (vw, lw) = entry_values(recipe, &entries[1])?;
    let mut out = Vec::new();
    // Labels of one group that set both variables: cook those pairs only.
    let joint: Option<Vec<(f32, f32)>> = match (&vl, &vw) {
        (Some(a), Some(b)) if a != b => {
            let drives = recipe["drives"].as_array().cloned().unwrap_or_default();
            let map_of = |var: &str| {
                drives
                    .iter()
                    .find(|d| d["to"]["value"] == var)
                    .map(|d| (d["group"].clone(), d["to"]["map"].clone()))
            };
            match (map_of(a), map_of(b)) {
                (Some((ga, ma)), Some((gb, mb))) if ga == gb => {
                    let mut pairs: Vec<(f32, f32)> = ma
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter_map(|(label, x)| Some((x.as_f64()? as f32, mb.get(label)?.as_f64()? as f32)))
                        .collect();
                    let base = (recipe["values"][a].as_f64(), recipe["values"][b].as_f64());
                    if let (Some(x), Some(y)) = base {
                        pairs.push((x as f32, y as f32));
                    }
                    Some(pairs)
                }
                _ => None,
            }
        }
        _ => None,
    };
    let pairs: Vec<(f32, f32)> = match joint {
        Some(p) => p,
        None if vl.is_some() && vl == vw => ll.iter().map(|&x| (x, x)).collect(),
        None => ll.iter().flat_map(|&a| lw.iter().map(move |&b| (a, b))).collect(),
    };
    let mut seen = BTreeSet::new();
    for (l, w) in pairs {
        if !l.is_finite() || !w.is_finite() || l <= 0.0 || w <= 0.0 {
            return Err(PackError::new(
                "pack_bad_recipe",
                format!("gem size {l}x{w} mm must be finite and positive"),
            ));
        }
        if !seen.insert((l.to_bits(), w.to_bits())) {
            continue;
        }
        let mut vars = BTreeMap::new();
        if let Some(v) = &vl {
            vars.insert(v.clone(), l);
        }
        if let Some(v) = &vw {
            vars.insert(v.clone(), w);
        }
        out.push((l, w, vars));
    }
    if out.len() > MAX_SIZES_PER_SLOT {
        return Err(PackError::new(
            "pack_bad_recipe",
            format!("gem slot has {} sizes (cap {MAX_SIZES_PER_SLOT})", out.len()),
        ));
    }
    Ok(out)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PackError> {
    let io = |e: std::io::Error| PackError::new("pack_io", format!("{}: {e}", path.display()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// gzip level 9 size of some bytes.
pub fn gzip_size(bytes: &[u8]) -> u64 {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    if e.write_all(bytes).is_err() {
        return bytes.len() as u64;
    }
    e.finish().map(|v| v.len() as u64).unwrap_or(bytes.len() as u64)
}

/// Re-encode a texture for the web: scaled to at most `max_px` on its longer side; color maps
/// as JPEG (quality 85), everything else (normals, masks) as PNG. KTX2 passes through as it is.
/// Returns (bytes, extension).
pub fn cook_texture(bytes: &[u8], role: &str, max_px: u32) -> Result<(Vec<u8>, &'static str), PackError> {
    if bytes.starts_with(&[0xAB, b'K', b'T', b'X', b' ', b'2', b'0', 0xBB]) {
        return Ok((bytes.to_vec(), "ktx2"));
    }
    let img = image::load_from_memory(bytes)
        .map_err(|e| PackError::new("pack_bad_kit", format!("texture: {e}")))?;
    let max_px = max_px.max(1);
    let img = if img.width().max(img.height()) > max_px {
        img.resize(max_px, max_px, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let mut out = std::io::Cursor::new(Vec::new());
    let color = matches!(role, "base_color" | "color" | "albedo");
    if color {
        let rgb = img.to_rgb8();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        rgb.write_with_encoder(enc)
            .map_err(|e| PackError::new("pack_io", format!("jpeg: {e}")))?;
        Ok((out.into_inner(), "jpg"))
    } else {
        img.write_to(&mut out, image::ImageFormat::Png)
            .map_err(|e| PackError::new("pack_io", format!("png: {e}")))?;
        Ok((out.into_inner(), "png"))
    }
}

/// The labels an option record offers per group: its `axes` and `addons` (`jewelry.option_graph/1`).
fn option_values(og: &Value) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let lists = [&og["axes"], &og["addons"], &og["groups"]];
    for g in lists.iter().flat_map(|l| l.as_array().into_iter().flatten()) {
        let Some(name) = g.get("name").or_else(|| g.get("group")).and_then(Value::as_str) else {
            continue;
        };
        let labels = g["values"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                v.as_str()
                    .or_else(|| v.get("label").and_then(Value::as_str))
                    .map(str::to_string)
            })
            .collect();
        out.insert(name.to_string(), labels);
    }
    out
}

/// Cook one package. `out` is created; files already in it with the package's names are replaced.
pub fn pack(
    kit_dir: &Path,
    recipe_path: &Path,
    out: &Path,
    opts: &CookOptions,
) -> Result<CookResult, PackError> {
    let _span = tracing::info_span!("viewer_pack", recipe = %recipe_path.display()).entered();
    let mut warnings = Vec::new();
    let mut kits = vec![Kit::open(kit_dir)?];
    for k in &opts.extra_kits {
        kits.push(Kit::open(k)?);
    }
    let recipe = read_json(recipe_path, "pack_bad_recipe")?;
    let bad = |m: String| PackError::new("pack_bad_recipe", m);
    if recipe.get("schema").and_then(Value::as_str) != Some("jewelry.piece_recipe/1") {
        return Err(bad("recipe schema is not jewelry.piece_recipe/1".into()));
    }
    let house = recipe["house"]
        .as_str()
        .ok_or_else(|| bad("recipe has no house".into()))?
        .to_string();
    let design_key = recipe["design_key"]
        .as_str()
        .ok_or_else(|| bad("recipe has no design_key".into()))?
        .to_string();
    // Tenant rule (interfaces.md § 0.3): a design uses its own house's parts and shared ones.
    for k in &kits {
        if k.house != "shared" && k.house != house {
            return Err(bad(format!(
                "kit {} belongs to {}, the recipe to {house}",
                k.id, k.house
            )));
        }
    }
    // The view must be one the drag view accepts.
    regia_viewer_view::range::from_recipe(&recipe).map_err(|e| bad(format!("view_range: {e}")))?;

    // Every part the piece can show: its parts plus every part-swap alternative.
    let mut part_refs: BTreeSet<String> = BTreeSet::new();
    for p in recipe["parts"].as_array().into_iter().flatten() {
        part_refs.insert(
            p["part_ref"]
                .as_str()
                .ok_or_else(|| bad("a part has no part_ref".into()))?
                .to_string(),
        );
    }
    let drives = recipe["drives"].as_array().cloned().unwrap_or_default();
    for d in &drives {
        if d["to"].get("part_swap").is_some() {
            for v in d["to"]["map"].as_object().into_iter().flat_map(|m| m.values()) {
                part_refs.insert(
                    v.as_str()
                        .ok_or_else(|| bad("part_swap map value is not a part_ref".into()))?
                        .to_string(),
                );
            }
        }
    }
    let mut meshes: Vec<MeshData> = Vec::new();
    let mut mesh_of_ref: BTreeMap<String, String> = BTreeMap::new();
    let mut glb_cache: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut textures: Vec<(String, Vec<u8>)> = Vec::new();
    for r in &part_refs {
        let (h, part_id) =
            parse_ref(r).ok_or_else(|| bad(format!("part_ref {r:?} is not <house>/<part>@<version>")))?;
        if h != "shared" && h != house {
            return Err(bad(format!("part {r} belongs to another house")));
        }
        let (kit, part) = kits
            .iter()
            .filter(|k| k.house == h)
            .find_map(|k| k.part(part_id).map(|p| (k, p)))
            .ok_or_else(|| bad(format!("no kit has part {r}")))?;
        let glb_name = part["glb"]
            .as_str()
            .ok_or_else(|| PackError::new("pack_bad_kit", format!("part {part_id} has no glb")))?;
        let glb_path = kit_file(&kit.dir, glb_name)?;
        if !glb_cache.contains_key(&glb_path) {
            let bytes = read_capped(&glb_path, crate::glb::MAX_GLB_BYTES as u64)?;
            glb_cache.insert(glb_path.clone(), bytes);
        }
        let glb = &glb_cache[&glb_path];
        let kind = part["kind"].as_str().unwrap_or("");
        let mut node = part["node"].as_str().unwrap_or(part_id).to_string();
        if HIDDEN_KINDS.contains(&kind) {
            let lods: Vec<&str> = part["lods"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l["node"].as_str())
                .collect();
            if let Some(last) = lods.last() {
                node = last.to_string();
            }
        }
        let slot = first_material_slot(part);
        let name = format!("part:{r}");
        let mesh = crate::glb::read_node(glb, &node, &name, &slot)?;
        if part["material_slots"].as_array().is_some_and(|a| a.len() > 1) {
            warnings.push(format!(
                "part {r}: {} material slots; all of it is drawn as {slot}",
                part["material_slots"].as_array().map_or(0, Vec::len)
            ));
        }
        for t in part["textures"].as_array().into_iter().flatten() {
            let (Some(path), role) = (t["path"].as_str(), t["role"].as_str().unwrap_or("detail")) else {
                continue;
            };
            let src = read_capped(&kit_file(&kit.dir, path)?, MAX_TEXTURE_BYTES)?;
            let (bytes, ext) = cook_texture(&src, role, opts.max_texture_px)?;
            let clean: String = format!("{part_id}-{role}")
                .to_ascii_lowercase()
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "._-".contains(c) {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            textures.push((format!("textures/{clean}.{ext}"), bytes));
        }
        mesh_of_ref.insert(r.clone(), name);
        meshes.push(mesh);
    }

    // Gem cuts at every size the slot can show.
    let mut slots_out = Vec::new();
    let mut slot_ids = std::collections::BTreeSet::new();
    for slot in recipe["gem_slots"].as_array().into_iter().flatten() {
        let slot_id = slot["slot_id"]
            .as_str()
            .ok_or_else(|| bad("a gem slot has no slot_id".into()))?;
        // Mesh names are `gem:<slot_id>@<size>` and the player takes the first of a name, so a
        // repeated id would draw the first slot's cut in the second slot.
        if !slot_ids.insert(slot_id) {
            return Err(bad(format!("gem slot {slot_id} is listed twice")));
        }
        let cut_type = slot["cut"]
            .as_str()
            .ok_or_else(|| bad(format!("gem slot {slot_id} has no cut")))?;
        let material_slot = slot["material_slot"]
            .as_str()
            .map_or_else(|| format!("gem:{slot_id}"), str::to_string);
        let mut sizes_out = Vec::new();
        for (l, w, vars) in slot_sizes(&recipe, slot)? {
            let name = format!("gem:{slot_id}@{}", size_label(l, w));
            let facets: Vec<Facet> = match kits.iter().find_map(|k| k.cut(cut_type, l, w)) {
                Some(c) => serde_json::from_value(c["facets"].clone()).map_err(|e| {
                    PackError::new("pack_bad_kit", format!("gem cut {}: facets: {e}", c["cut_id"]))
                })?,
                None => {
                    let f = geom::cut_facets(cut_type, l, w).ok_or_else(|| {
                        bad(format!(
                            "no kit has a {cut_type} cut of {l}x{w} mm and there is no stand-in for it"
                        ))
                    })?;
                    warnings.push(format!("gem slot {slot_id}: no {cut_type} {l}x{w} mm cut in the kits; generated from published proportions"));
                    // Outline helpers put length on +x. Gem-local puts length on +y (ADR-2615).
                    gem_local_facets(f)
                }
            };
            let mesh =
                geom::convex_mesh(&facets, l.max(w) * 2.0, &name, &material_slot).ok_or_else(|| {
                    PackError::new(
                        "pack_bad_kit",
                        format!("gem cut {cut_type} {l}x{w}: the facets do not bound a solid"),
                    )
                })?;
            sizes_out.push(json!({"size_mm": [l, w], "mesh": name, "values": vars}));
            meshes.push(mesh);
        }
        slots_out.push(json!({
            "slot_id": slot_id, "cut": cut_type, "material_slot": material_slot,
            "count": slot["count"], "pattern": slot["pattern"], "at": slot["at"], "sizes": sizes_out,
        }));
    }

    // Option data: the drives grouped by option group, with the labels the option record offers.
    let (og, og_hash, cost_removed) = match &opts.option_graph {
        Some(p) => {
            let bytes = read_capped(p, MAX_JSON_BYTES)?;
            let mut v: Value = serde_json::from_slice(&bytes)
                .map_err(|e| PackError::new("pack_bad_json", format!("{}: {e}", p.display())))?;
            let n = strip_cost(&mut v);
            (v, Value::String(blake3_hex(&bytes)), n)
        }
        None => (Value::Null, Value::Null, 0),
    };
    let offered = option_values(&og);
    let library = match &opts.materials {
        Some(p) => Some(read_json(p, "pack_bad_json")?),
        None => None,
    };
    let entries: Vec<Value> = library
        .as_ref()
        .and_then(|l| {
            l.get("materials")
                .or_else(|| l.get("entries"))
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default();
    let mut groups: BTreeMap<String, Value> = BTreeMap::new();
    let mut used_materials: BTreeSet<String> = BTreeSet::new();
    for d in &drives {
        let g = d["group"]
            .as_str()
            .ok_or_else(|| bad("a drive has no group".into()))?
            .to_string();
        let row = groups
            .entry(g.clone())
            .or_insert_with(|| json!({"group": g, "visual": true, "drives": [], "values": []}));
        let mut to = d["to"].clone();
        let swap = to.get("part_swap").is_some();
        if swap && let Some(map) = to.get_mut("map").and_then(Value::as_object_mut) {
            for v in map.values_mut() {
                let mesh = v.as_str().and_then(|r| mesh_of_ref.get(r)).cloned();
                if let Some(m) = mesh {
                    *v = json!({"part_ref": v.clone(), "mesh": m});
                }
            }
        }
        if let Some(a) = row["drives"].as_array_mut() {
            a.push(to);
        }
    }
    for (g, row) in groups.iter_mut() {
        let mut labels: Vec<String> = offered.get(g).cloned().unwrap_or_default();
        if labels.is_empty() {
            let mut seen = BTreeSet::new();
            for d in row["drives"].as_array().into_iter().flatten() {
                for k in d["map"].as_object().into_iter().flat_map(|m| m.keys()) {
                    if seen.insert(k.clone()) {
                        labels.push(k.clone());
                    }
                }
            }
        }
        let zone_kind = row["drives"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|d| d.get("material_zone").is_some());
        let values: Vec<Value> = labels
            .iter()
            .map(|label| {
                let facet = format!("{g}={label}");
                let mat = entries.iter().find(|e| {
                    e["catalogue_facets"]
                        .as_array()
                        .is_some_and(|f| f.iter().any(|x| x == &Value::String(facet.clone())))
                });
                match mat.and_then(|m| m["material_id"].as_str()) {
                    Some(id) => {
                        used_materials.insert(id.to_string());
                        json!({"label": label, "material_id": id})
                    }
                    None => {
                        if zone_kind && library.is_some() {
                            warnings.push(format!("option {facet}: no material entry"));
                        }
                        json!({"label": label})
                    }
                }
            })
            .collect();
        row["values"] = Value::Array(values);
    }
    for g in recipe["no_visual"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let labels = offered.get(g).cloned().unwrap_or_default();
        groups.insert(
            g.to_string(),
            json!({"group": g, "visual": false, "drives": [],
            "values": labels.iter().map(|l| json!({"label": l})).collect::<Vec<_>>()}),
        );
    }
    let layout: Vec<Value> = recipe["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let r = p["part_ref"].as_str().unwrap_or_default();
            json!({"instance_id": p["instance_id"], "part_ref": r, "mesh": mesh_of_ref.get(r), "at": p["at"]})
        })
        .collect();
    let options = json!({
        "schema": "regia.viewer_options/1",
        "groups": groups.into_values().collect::<Vec<_>>(),
        "parts": layout,
        "gem_slots": slots_out,
        "material_zones": recipe["material_zones"],
        "option_graph": og,
        "option_graph_hash": og_hash,
        "cost_fields_removed": cost_removed,
    });
    let materials = json!({
        "schema": "regia.viewer_materials/1",
        "library_version": library.as_ref().map(|l| l["library_version"].clone()).unwrap_or(Value::Null),
        "entries": entries.iter().filter(|e| e["material_id"].as_str().is_some_and(|id| used_materials.contains(id))).cloned().collect::<Vec<_>>(),
    });

    // Write the files, the manifest last.
    let recipe_text = canonical_json(&recipe);
    let mut files: Vec<(String, &'static str, Vec<u8>)> = vec![
        (MESH_FILE.into(), "mesh", mesh::encode(&meshes)?),
        (RECIPE_FILE.into(), "recipe", recipe_text.clone().into_bytes()),
        (
            OPTIONS_FILE.into(),
            "options",
            canonical_json(&options).into_bytes(),
        ),
        (
            MATERIALS_FILE.into(),
            "materials",
            canonical_json(&materials).into_bytes(),
        ),
    ];
    for (p, b) in textures {
        if files.iter().any(|f| f.0 == p) {
            warnings.push(format!("texture {p} written twice; kept the first"));
            continue;
        }
        files.push((p, "texture", b));
    }
    let mut entries_out = Vec::new();
    for (p, kind, bytes) in &files {
        if !safe_path(p) {
            return Err(PackError::new("pack_unsafe_path", format!("{p:?}")));
        }
        write_atomic(&out.join(p), bytes)?;
        entries_out.push(FileEntry {
            path: p.clone(),
            kind: kind.to_string(),
            bytes: bytes.len() as u64,
            blake3: blake3_hex(bytes),
        });
    }
    let slug = design_key
        .split_once('/')
        .map_or(design_key.as_str(), |(_, s)| s)
        .to_ascii_lowercase();
    let manifest = json!({
        "schema": SCHEMA,
        "format_version": crate::FORMAT_VERSION,
        "package_id": format!("{house}/{slug}"),
        "house": house,
        "design_key": design_key,
        "product_keys": recipe["product_keys"],
        "recipe_version": recipe["recipe_version"],
        "recipe_hash": blake3_hex(recipe_text.as_bytes()),
        "kits": kits.iter().map(|k| json!({"kit_id": k.id, "kit_version": k.version, "house": k.house})).collect::<Vec<_>>(),
        "engine_version": env!("CARGO_PKG_VERSION"),
        "meshes": meshes.iter().map(|m| json!({"name": m.name, "material_slot": m.material_slot,
            "vertices": m.positions.len(), "triangles": m.triangles()})).collect::<Vec<_>>(),
        "files": entries_out,
        "package_hash": package_hash(&entries_out),
    });
    let manifest_text = canonical_json(&manifest);
    write_atomic(&out.join(MANIFEST_FILE), manifest_text.as_bytes())?;
    let gz = |b: &[u8]| opts.gzip.then(|| gzip_size(b));
    let mut rows = vec![SizeRow {
        path: MANIFEST_FILE.into(),
        kind: "manifest".into(),
        bytes: manifest_text.len() as u64,
        gzip_bytes: gz(manifest_text.as_bytes()),
    }];
    rows.extend(files.iter().map(|(p, k, b)| SizeRow {
        path: p.clone(),
        kind: k.to_string(),
        bytes: b.len() as u64,
        gzip_bytes: gz(b),
    }));
    let tris = meshes.iter().map(|m| m.triangles() as u64).sum();
    let report = SizeReport::new(rows, meshes.len() as u64, tris);
    if !report.within_budget {
        warnings.push(format!(
            "package is {} bytes, over the {} byte budget",
            report.total_bytes, report.budget_bytes
        ));
    }
    Ok(CookResult {
        manifest,
        report,
        warnings,
    })
}
