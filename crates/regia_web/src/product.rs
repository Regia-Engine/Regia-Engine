// SPDX-License-Identifier: MPL-2.0
//! An opened product: a verified package and the configurator over its option data. It answers what the
//! page asks (`options`, `setPick`, `build`) and what the renderer asks (the draws of the current picks).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use glam::{Affine3A, Mat3A, Vec3, Vec3A};
use regia_configurator::content::{MaterialLibrary, Recipe};
use regia_configurator::graph::ProductRecord;
use regia_configurator::resolve::Pick;
use regia_configurator::runtime::{
    BuildOptions, Camera, ConfigError, Configurator, ReopenReport, SetMode, Tenant,
};
use regia_configurator::source::ProductContent;
use regia_looks::{FacetFamily, Library, StoneSize};
use regia_viewer_pack::{MeshData, Package};
use serde_json::{Value, json};

use crate::WebError;
use crate::seams::{PickApplied, PlannedDraw, PlannedMaterial, ProductModel, ScenePlan, ViewValues};

/// The view fits the piece with this much room around it.
const FIT_MARGIN: f32 = 1.1;

static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

/// The next product serial. A renderer treats two products with one serial as the same meshes.
fn next_serial() -> u64 {
    NEXT_SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// Groups a pick changed besides `group` itself. Each group once, in first-seen order (dropped, then filled).
fn adjusted_groups<'a>(
    dropped: impl IntoIterator<Item = &'a str>,
    filled: impl IntoIterator<Item = &'a str>,
    group: &str,
) -> Vec<String> {
    let mut adjusted = Vec::new();
    for name in dropped.into_iter().chain(filled) {
        if name == group || adjusted.iter().any(|seen| seen == name) {
            continue;
        }
        adjusted.push(name.to_string());
    }
    adjusted
}

fn package_invalid(what: impl std::fmt::Display) -> WebError {
    WebError::new("package_invalid", what.to_string())
}

fn open_error(e: ConfigError) -> WebError {
    let code = match e.reason {
        "configurator_not_orderable" | "configurator_not_modelled" | "configurator_duplicate" => {
            "product_not_orderable"
        }
        _ => "package_invalid",
    };
    WebError::new(code, e.message)
}

fn pick_error(e: ConfigError) -> WebError {
    let code = match e.reason {
        "configurator_unknown_group" => "pick_unknown_group",
        "configurator_unknown_value" | "configurator_value_unavailable" => "pick_unknown_value",
        "configurator_not_sold" => "pick_not_sold",
        _ => "bad_argument",
    };
    WebError::new(code, e.message)
}

/// The material documents of a package under the configurator's and the look library's name for them.
fn normalized_library(materials: &Value) -> Value {
    let entries = materials
        .get("materials")
        .or_else(|| materials.get("entries"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    json!({
        "schema": "jewelry.material_library/1",
        "library_version": materials.get("library_version").cloned().unwrap_or(Value::Null),
        "materials": entries,
    })
}

/// A part's or stone's place from a recipe's `at`: `origin_mm`, `z_axis`, `up`. What is absent or not finite is identity.
fn transform_of(at: &Value) -> Affine3A {
    let vec3 = |key: &str| -> Option<Vec3> {
        let a = at.get(key)?.as_array()?;
        if a.len() != 3 {
            return None;
        }
        let v = Vec3::new(
            a[0].as_f64()? as f32,
            a[1].as_f64()? as f32,
            a[2].as_f64()? as f32,
        );
        v.is_finite().then_some(v)
    };
    let origin = vec3("origin_mm").unwrap_or(Vec3::ZERO);
    let z = vec3("z_axis").and_then(Vec3::try_normalize).unwrap_or(Vec3::Z);
    let up = vec3("up").unwrap_or(Vec3::Y);
    // An `up` along `z_axis` names no turn about it: fall back to Y, then X, so the frame stays
    // orthonormal (a fixed X fallback sheared and scaled any part whose axis is not along Z).
    let x = [up, Vec3::Y, Vec3::X]
        .into_iter()
        .find_map(|u| u.cross(z).try_normalize())
        .unwrap_or(Vec3::X);
    let y = z.cross(x);
    Affine3A {
        matrix3: Mat3A::from_cols(x.into(), y.into(), z.into()),
        translation: Vec3A::from(origin),
    }
}

/// The size a stone's shader needs, from the stone's own mesh.
fn stone_of_mesh(mesh: &MeshData, cut: &str) -> StoneSize {
    let (lo, hi) = mesh.bounds();
    StoneSize::from_bounds(lo.to_array(), hi.to_array(), FacetFamily::of_cut(cut))
}

/// Whether a package's `size_mm` row is the stone size asked for. One number means a round stone: length and
/// width are the same.
fn same_size(row: &Value, size: &[f64]) -> bool {
    let (Some(row), Some(&l)) = (row.as_array(), size.first()) else {
        return false;
    };
    let w = size.get(1).copied().unwrap_or(l);
    let close = |v: Option<&Value>, want: f64| {
        v.and_then(Value::as_f64)
            .is_some_and(|x| (x - want).abs() < 0.005)
    };
    close(row.first(), l) && close(row.get(1).or_else(|| row.first()), w)
}

/// The entries of a JSON array by the string field `key`; the first entry of a repeated id wins.
fn first_by<'a>(list: &'a Value, key: &str) -> BTreeMap<&'a str, &'a Value> {
    let mut out = BTreeMap::new();
    for entry in list.as_array().into_iter().flatten() {
        if let Some(id) = entry[key].as_str() {
            out.entry(id).or_insert(entry);
        }
    }
    out
}

/// An open product.
pub struct PackageProduct {
    serial: u64,
    config: Configurator,
    package: Package,
    library: Library,
    /// `part_ref` to the mesh of that part.
    mesh_of_part: BTreeMap<String, String>,
    /// Mesh name to its index in the package (the first mesh of a repeated name, as a search finds it).
    mesh_index: BTreeMap<String, usize>,
    base_camera: Camera,
    no_visual: BTreeSet<String>,
    content_build: String,
    studio_id: String,
}

impl PackageProduct {
    /// Opens a verified package. A product the snapshot does not sell does not open.
    pub fn open(package: Package, studio_id: &str) -> Result<Self, WebError> {
        let graph = package
            .options
            .get("option_graph")
            .filter(|g| !g.is_null())
            .or_else(|| package.options.get("product_key").map(|_| &package.options))
            .cloned()
            .ok_or_else(|| package_invalid("options.json has no option record: nothing could be refused"))?;
        let product =
            ProductRecord::from_value(graph).map_err(|e| package_invalid(format!("options.json: {e}")))?;
        let recipe = Recipe::from_value(package.recipe.clone())
            .map_err(|e| package_invalid(format!("recipe.json: {e}")))?;
        let library_doc = normalized_library(&package.materials);
        let library_bytes = serde_json::to_vec(&library_doc).map_err(package_invalid)?;
        let library = MaterialLibrary::from_json(&library_bytes)
            .map_err(|e| package_invalid(format!("materials.json: {e}")))?;
        let looks = Library::parse(&String::from_utf8_lossy(&library_bytes))
            .map_err(|e| package_invalid(format!("materials.json: {e}")))?;

        let no_visual = recipe.no_visual.iter().cloned().collect();
        let base_camera = Camera::default_for(Some(&recipe));
        let manifest = &package.manifest.doc;
        // The package names the product it was cooked for: an option record for another product is a
        // damaged or mixed-up package, not something to show.
        if let Some(keys) = manifest.get("product_keys").and_then(Value::as_array)
            && !keys
                .iter()
                .any(|k| k.as_str() == Some(product.product_key.as_str()))
        {
            return Err(package_invalid(format!(
                "options.json is for `{}`, which the manifest does not list",
                product.product_key
            )));
        }
        // A package holds one house plus shared parts. The tenant may show the manifest's house (the
        // product's own house only when the manifest names none), so Configurator::open refuses an
        // option record of another house instead of trusting the record's own claim.
        let house = manifest
            .get("house")
            .and_then(Value::as_str)
            .map_or_else(|| product.house().to_string(), str::to_string);
        let tenant_id = manifest
            .get("tenant_id")
            .and_then(Value::as_str)
            .map_or_else(|| format!("house:{house}"), str::to_string);
        let tenant = Tenant::new(tenant_id, vec![house]);
        let config = Configurator::open(
            ProductContent {
                product,
                recipe: Some(recipe),
                kits: Vec::new(),
                library: Some(library),
                read: Vec::new(),
            },
            tenant,
        )
        .map_err(open_error)?;

        let mut mesh_of_part = BTreeMap::new();
        for part in package.options["parts"].as_array().into_iter().flatten() {
            if let (Some(r), Some(m)) = (part["part_ref"].as_str(), part["mesh"].as_str()) {
                mesh_of_part.insert(r.to_string(), m.to_string());
            }
        }
        for group in package.options["groups"].as_array().into_iter().flatten() {
            for drive in group["drives"].as_array().into_iter().flatten() {
                if drive.get("part_swap").is_none() {
                    continue;
                }
                for target in drive["map"].as_object().into_iter().flat_map(|m| m.values()) {
                    if let (Some(r), Some(m)) = (target["part_ref"].as_str(), target["mesh"].as_str()) {
                        mesh_of_part.insert(r.to_string(), m.to_string());
                    }
                }
            }
        }
        let content_build = package.manifest.package_hash.clone();
        let serial = next_serial();
        let mut mesh_index = BTreeMap::new();
        for (i, mesh) in package.meshes.iter().enumerate() {
            mesh_index.entry(mesh.name.clone()).or_insert(i);
        }
        Ok(PackageProduct {
            serial,
            config,
            package,
            library: looks,
            mesh_of_part,
            mesh_index,
            base_camera,
            no_visual,
            content_build,
            studio_id: studio_id.to_string(),
        })
    }

    /// A package mesh by name, through the index made on open.
    fn mesh(&self, name: &str) -> Option<&MeshData> {
        self.mesh_index
            .get(name)
            .and_then(|&i| self.package.meshes.get(i))
    }

    /// The recipe document (the view limits come from it).
    pub fn recipe_doc(&self) -> &Value {
        &self.package.recipe
    }

    /// The middle of what the current picks draw: where the view turns about.
    pub fn pivot(&self) -> Result<[f32; 3], WebError> {
        let plan = self.plan()?;
        if plan.draws.is_empty() {
            return Ok([0.0; 3]);
        }
        let (min, max) = self.bounds_of(&plan);
        Ok(((min + max) * 0.5).to_array())
    }

    /// The camera distance at which the current piece, seen anywhere inside `tilt_max_deg`, fits a view of
    /// `fov_short_deg` across its shorter side with a 10 % margin (`docs/specs/42-product-viewer.md` § 6.2).
    pub fn fit_distance(&self, fov_short_deg: f32, tilt_max_deg: f32) -> Result<Option<f32>, WebError> {
        let plan = self.plan()?;
        let (min, max) = self.bounds_of(&plan);
        if min.x > max.x {
            return Ok(None);
        }
        let half = (max - min) * 0.5;
        let reach = half.x.max(half.y) + half.z * tilt_max_deg.clamp(0.0, 90.0).to_radians().sin();
        if !(reach.is_finite() && reach > 0.0) {
            return Ok(None);
        }
        let half_angle = (fov_short_deg.clamp(1.0, 170.0) * 0.5).to_radians();
        Ok(Some(FIT_MARGIN * reach / half_angle.tan()))
    }

    fn bounds_of(&self, plan: &ScenePlan) -> (Vec3, Vec3) {
        let mut min = Vec3::splat(f32::MAX);
        let mut max = Vec3::splat(f32::MIN);
        for draw in &plan.draws {
            let Some(mesh) = self.mesh(&draw.mesh) else {
                continue;
            };
            let (lo, hi) = mesh.bounds();
            for t in &draw.transforms {
                for corner in 0..8u32 {
                    let p = Vec3::new(
                        if corner & 1 == 0 { lo.x } else { hi.x },
                        if corner & 2 == 0 { lo.y } else { hi.y },
                        if corner & 4 == 0 { lo.z } else { hi.z },
                    );
                    let q = t.transform_point3(p);
                    min = min.min(q);
                    max = max.max(q);
                }
            }
        }
        (min, max)
    }

    /// The camera distance the builds report; the view frames the piece at this distance.
    pub fn set_camera_distance(&mut self, distance_mm: f32) {
        self.base_camera.focus_distance_mm = f64::from(distance_mm);
    }

    /// Reopens a saved `jewelry.build/1` of this product (`Configurator::load_build`): its picks, wear and
    /// camera. Returns what reopening found and the view the build was saved at.
    pub fn load_build(&mut self, build: &Value) -> Result<(ReopenReport, ViewValues), WebError> {
        let report = self
            .config
            .load_build(build)
            .map_err(|e| WebError::new("package_invalid", format!("{}: {}", e.reason, e.message)))?;
        let camera = self.config.camera();
        let azimuth = camera.tilt_azimuth_deg.to_radians();
        let view = ViewValues {
            tilt: [
                (camera.tilt_deg * azimuth.sin()) as f32,
                (camera.tilt_deg * azimuth.cos()) as f32,
            ],
            spin: camera.spin_deg as f32,
            pan: [camera.pan_mm[0] as f32, camera.pan_mm[1] as f32],
            zoom: camera.zoom as f32,
        };
        Ok((report, view))
    }

    /// The package's meshes by name.
    pub fn mesh_names(&self) -> Vec<String> {
        self.package.meshes.iter().map(|m| m.name.clone()).collect()
    }

    /// The package's manifest hash.
    pub fn package_hash(&self) -> &str {
        &self.content_build
    }

    /// The labels of `group=value` picks nothing maps; a viewer shows its fallback for them.
    pub fn unmapped(&self) -> Vec<String> {
        self.config.look().unmapped
    }

    fn camera(&self, view: &ViewValues) -> Camera {
        let mut c = self.base_camera.clone();
        let [x, y] = view.tilt;
        c.rig_preset = if view.spin != 0.0 {
            "spin"
        } else if x != 0.0 || y != 0.0 {
            "tilt"
        } else if view.pan != [0.0, 0.0] {
            "pan"
        } else {
            "front"
        }
        .to_string();
        c.pan_mm = [f64::from(view.pan[0]), f64::from(view.pan[1])];
        c.tilt_deg = f64::from((x * x + y * y).sqrt());
        c.tilt_azimuth_deg = f64::from(x.atan2(y).to_degrees());
        c.spin_deg = f64::from(view.spin);
        c.zoom = f64::from(view.zoom);
        c
    }
}

fn pick_of(value: &Value) -> Result<Pick, WebError> {
    match value {
        Value::String(s) => Ok(Pick::One(s.clone())),
        Value::Array(a) => a
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(Pick::Many)
            .ok_or_else(|| WebError::new("pick_unknown_value", "a multi group takes a list of labels")),
        other => Err(WebError::new(
            "pick_unknown_value",
            format!("a pick is a label or a list of labels, not {other}"),
        )),
    }
}

impl ProductModel for PackageProduct {
    fn serial(&self) -> u64 {
        self.serial
    }

    fn meshes(&self) -> &[MeshData] {
        &self.package.meshes
    }

    fn plan(&self) -> Result<ScenePlan, WebError> {
        let look = self.config.look();
        let mut plan = ScenePlan::default();
        // A zone's stone: its size as its mesh measures, and whether the cut is faceted.
        let mut stone_of_zone: BTreeMap<String, (StoneSize, bool)> = BTreeMap::new();

        // Indexed once per plan, by reference (the first entry of an id wins, as a search would find
        // it): a clone of the arrays and a search per part made every pick O(parts²).
        let layout = first_by(&self.package.options["parts"], "instance_id");
        for part in &look.parts {
            let entry = layout.get(part.instance_id.as_str()).copied();
            let mesh_name = self
                .mesh_of_part
                .get(&part.part_ref)
                .ok_or_else(|| package_invalid(format!("no mesh for part {}", part.part_ref)))?;
            let mesh = self
                .mesh(mesh_name)
                .ok_or_else(|| package_invalid(format!("mesh.rvm has no mesh {mesh_name}")))?;
            let at = entry.map_or(Value::Null, |e| e["at"].clone());
            if at.get("origin_mm").is_none() && at.get("socket").is_some_and(|s| !s.is_null()) {
                plan.problems.push(format!(
                    "{}: placed by socket {}, which the package does not resolve",
                    part.instance_id, at["socket"]
                ));
            }
            plan.draws.push(PlannedDraw {
                mesh: mesh_name.clone(),
                zone: mesh.material_slot.clone(),
                transforms: vec![transform_of(&at)],
            });
        }

        let slots = first_by(&self.package.options["gem_slots"], "slot_id");
        for gem in &look.gems {
            let Some(slot) = slots.get(gem.slot_id.as_str()).copied() else {
                return Err(package_invalid(format!(
                    "options.json has no gem slot {}",
                    gem.slot_id
                )));
            };
            let size: Vec<f64> = gem.size_mm.clone().unwrap_or_default();
            let row = slot["sizes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|row| same_size(&row["size_mm"], &size))
                .ok_or_else(|| {
                    package_invalid(format!("gem slot {} has no mesh for size {size:?}", gem.slot_id))
                })?;
            let mesh_name = row["mesh"]
                .as_str()
                .ok_or_else(|| package_invalid(format!("gem slot {} names no mesh", gem.slot_id)))?;
            let zone = slot["material_slot"]
                .as_str()
                .map_or_else(|| format!("gem:{}", gem.slot_id), str::to_string);
            let mesh = self
                .mesh(mesh_name)
                .ok_or_else(|| package_invalid(format!("mesh.rvm has no mesh {mesh_name}")))?;
            stone_of_zone.insert(
                zone.clone(),
                (stone_of_mesh(mesh, &gem.cut), !gem.cut.contains("cabochon")),
            );
            plan.draws.push(PlannedDraw {
                mesh: mesh_name.to_string(),
                zone,
                transforms: vec![transform_of(&slot["at"])],
            });
        }

        let zones: BTreeSet<String> = plan.draws.iter().map(|d| d.zone.clone()).collect();
        for zone in zones {
            let entry = look.zones.get(&zone);
            let id = entry.and_then(|z| z.material_id.clone());
            let finish = entry.and_then(|z| z.finishes.first().cloned());
            let stone = stone_of_zone.get(&zone).copied();
            // A size pick changes what the stone shader reads, so the size is part of the key.
            let key = format!(
                "{}|{}|{}",
                id.as_deref().unwrap_or("-"),
                finish.as_deref().unwrap_or("-"),
                stone.map_or_else(String::new, |(s, _)| format!(
                    "{}x{}x{}",
                    s.half_length_mm, s.half_width_mm, s.depth_mm
                )),
            );
            let drawn = match id.as_deref() {
                Some(id) => match self.library.look(id, finish.as_deref()) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        plan.problems.push(format!("{zone}: {e}"));
                        None
                    }
                },
                None => {
                    plan.problems
                        .push(format!("{zone}: no material entry for the current pick"));
                    None
                }
            };
            let is_gem = zone.starts_with("gem:") || entry.is_some_and(|z| z.kind == "gem");
            let material = match drawn {
                Some(m) => m,
                None if is_gem => regia_looks::LookMaterial::Gem(regia_looks::GemLook {
                    n_d: 1.54,
                    abbe: 60.0,
                    attenuation_per_mm: [0.0; 3],
                    roughness: 0.0,
                    fire: regia_looks::material::DEFAULT_FIRE,
                }),
                None => regia_looks::LookMaterial::Metal(regia_looks::MetalLook {
                    f0: [0.56, 0.57, 0.58],
                    f82_tint: [1.0; 3],
                    roughness: 0.2,
                    anodize_lut: None,
                    grain_mm: 0.0,
                }),
            };
            // The cut, not the library entry, decides a dome or facets.
            let material = match stone {
                Some((_, faceted)) => material.for_cut(faceted),
                None => material,
            };
            plan.materials.insert(
                zone.clone(),
                PlannedMaterial {
                    key,
                    look: material,
                    stone: stone.map_or_else(StoneSize::default, |(s, _)| s),
                },
            );
        }
        Ok(plan)
    }

    fn options(&self) -> Value {
        let view = self.config.options();
        Value::Array(
            view.groups
                .iter()
                .map(|g| {
                    let values: Vec<Value> = g
                        .values
                        .iter()
                        .map(|v| {
                            let mut row =
                                json!({"value": v.label, "label": v.label, "available": v.selectable});
                            if let Some(id) = &v.material_id {
                                row["material_id"] = json!(id);
                            }
                            if let Some(reason) = v.reason {
                                row["reason"] = json!(reason);
                                if !v.conflicts.is_empty() {
                                    row["conflicts"] = json!(v.conflicts);
                                }
                            }
                            row
                        })
                        .collect();
                    json!({
                        "group": g.group,
                        "kind": if g.multi { "multi" } else { "single" },
                        "current": g.picked,
                        "visual": !g.no_visual && !self.no_visual.contains(&g.group),
                        "required": g.required,
                        "applies": g.applies,
                        "values": values,
                    })
                })
                .collect(),
        )
    }

    fn set_pick(&mut self, group: &str, value: &Value, adjust: bool) -> Result<PickApplied, WebError> {
        let pick = pick_of(value)?;
        let mode = if adjust { SetMode::Adjust } else { SetMode::Strict };
        let before = self.config.geometry_key();
        // A pick the package cannot draw (an offered size it has no mesh for) is refused and changes nothing:
        // the configurator is put back, so the player's build, options and picture stay as they were.
        let saved = self.config.clone();
        let outcome = self
            .config
            .set_pick(group, Some(pick), mode)
            .map_err(pick_error)?;
        if let Err(e) = self.plan() {
            self.config = saved;
            return Err(e);
        }
        let adjusted = adjusted_groups(
            outcome.dropped.keys().map(String::as_str),
            outcome.filled.keys().map(String::as_str),
            group,
        );
        Ok(PickApplied {
            adjusted,
            geometry_changed: before != self.config.geometry_key(),
        })
    }

    fn build(&self, view: &ViewValues) -> Result<Value, WebError> {
        let options = BuildOptions {
            camera: Some(self.camera(view)),
            studio_id: self.studio_id.clone(),
            output_gamut: "srgb".to_string(),
            content_build_id: self.content_build.clone(),
            ..BuildOptions::default()
        };
        self.config
            .build(&options)
            .map_err(|e| WebError::new("package_invalid", format!("build: {e}")))
    }

    fn incomplete(&self) -> bool {
        self.config.options().listing == "incomplete"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stone_size_matches_by_length_and_width() {
        assert!(same_size(&json!([3.0, 1.5]), &[3.0, 1.5]));
        assert!(same_size(&json!([3.0, 1.5]), &[3.001, 1.499]));
        assert!(!same_size(&json!([3.0, 1.5]), &[3.0, 2.0]));
        assert!(!same_size(&json!([4.0, 2.0]), &[3.0, 1.5]));
    }

    #[test]
    fn one_number_is_a_round_stone() {
        assert!(same_size(&json!([2.25, 2.25]), &[2.25]));
        assert!(same_size(&json!([2.25]), &[2.25]));
        assert!(same_size(&json!([2.25]), &[2.25, 2.25]));
        assert!(!same_size(&json!([3.0, 3.0]), &[2.25]));
    }

    #[test]
    fn no_size_matches_nothing() {
        assert!(!same_size(&json!([2.25, 2.25]), &[]));
        assert!(!same_size(&Value::Null, &[2.25]));
    }

    #[test]
    fn a_place_comes_from_origin_and_axes() {
        let t = transform_of(&json!({"origin_mm": [1, 2, 3], "z_axis": [0, 0, 1], "up": [0, 1, 0]}));
        assert_eq!(t.transform_point3(Vec3::ZERO), Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(t.transform_vector3(Vec3::Z), Vec3::Z);
        assert_eq!(t.transform_vector3(Vec3::Y), Vec3::Y);
    }

    #[test]
    fn a_turned_axis_turns_the_part_and_nonsense_is_identity() {
        let t = transform_of(&json!({"z_axis": [1, 0, 0], "up": [0, 1, 0]}));
        assert!(t.transform_vector3(Vec3::Z).distance(Vec3::X) < 1e-6);
        assert_eq!(transform_of(&Value::Null), Affine3A::IDENTITY);
        assert_eq!(
            transform_of(&json!({"origin_mm": "x", "z_axis": [0, 0, 0]})),
            Affine3A::IDENTITY
        );
    }

    /// Correctness audit (corr-web-viewer): an `up` along a tilted `z_axis` used to fall back to a fixed X
    /// axis, which is not perpendicular to that `z_axis`, so the part was sheared and scaled.
    #[test]
    fn corr_up_along_the_axis_keeps_the_frame_orthonormal() {
        for (z, up) in [
            ([0.6, 0.0, 0.8], [0.6, 0.0, 0.8]),
            ([1.0, 0.0, 0.0], [-1.0, 0.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 2.0, 0.0]),
        ] {
            let t = transform_of(&json!({"z_axis": z, "up": up}));
            let m = t.matrix3;
            for c in [m.x_axis, m.y_axis, m.z_axis] {
                assert!((c.length() - 1.0).abs() < 1e-5, "{z:?} {up:?}: column {c}");
            }
            assert!(m.x_axis.dot(m.y_axis).abs() < 1e-5, "{z:?} {up:?}");
            assert!(m.x_axis.dot(m.z_axis).abs() < 1e-5, "{z:?} {up:?}");
            assert!(m.y_axis.dot(m.z_axis).abs() < 1e-5, "{z:?} {up:?}");
            assert!((m.determinant() - 1.0).abs() < 1e-5, "{z:?} {up:?}");
            assert!(t.transform_vector3(Vec3::Z).distance(Vec3::from(z).normalize()) < 1e-5);
        }
    }

    #[test]
    fn audit_empty_stone_mesh_is_finite() {
        let mesh = MeshData {
            name: "cut:empty".into(),
            material_slot: "gem:s".into(),
            positions: Vec::new(),
            normals: Vec::new(),
            uvs: None,
            indices: Vec::new(),
        };
        let size = stone_of_mesh(&mesh, "round");
        assert!(
            size.depth_mm.is_finite() && size.half_length_mm.is_finite() && size.half_width_mm.is_finite(),
            "empty mesh size {size:?}"
        );
        assert_eq!(size.depth_mm, 0.0);
        assert_eq!(size.half_length_mm, 0.0);
        assert_eq!(size.half_width_mm, 0.0);
    }

    #[test]
    fn audit_non_finite_place_is_identity() {
        // JSON rejects non-finite numbers. A magnitude past f32::MAX is a finite f64 that becomes
        // infinity when the place is stored as f32, and used to translate the part there.
        assert_eq!(
            transform_of(&json!({"origin_mm": [1e39, 0.0, 0.0]})),
            Affine3A::IDENTITY
        );
    }

    #[test]
    fn audit_adjusted_group_is_listed_once() {
        // Adjust drops a group and the sold variant fills it again, plus a group that sorts between
        // the two copies. `dedup` only collapses neighbours, so the group was reported twice.
        assert_eq!(
            adjusted_groups(["Metal"], ["Gem", "Metal"], "Finish"),
            vec!["Metal".to_string(), "Gem".to_string()]
        );
    }

    #[test]
    fn audit_serials_differ_across_threads() {
        let a = std::thread::spawn(next_serial).join().expect("thread");
        let b = std::thread::spawn(next_serial).join().expect("thread");
        assert_ne!(
            a, b,
            "two opens must not share a serial, including across threads"
        );
    }

    #[test]
    fn both_library_spellings_read_the_same() {
        let a = normalized_library(&json!({"materials": [{"material_id": "m"}]}));
        let b = normalized_library(
            &json!({"entries": [{"material_id": "m"}], "schema": "regia.viewer_materials/1"}),
        );
        assert_eq!(a, b);
        assert_eq!(a["schema"], "jewelry.material_library/1");
    }
}
