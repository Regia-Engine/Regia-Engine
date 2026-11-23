// SPDX-License-Identifier: MPL-2.0
//! `regia bake-lightmaps --scene <level>`: bakes a level directly, with no `.bake.ron` on disk
//! (ADR-2024, `book/agents/lightmaps.md`).
//!
//! The level is spawned headless and exported by the code `regia bake-gi --from-scene` and the
//! agent's `level.export_bake` run (`regia_agent::bake_export`), so the bake sees the same static
//! meshes, sun and sky. The export's text is parsed and only the traced scene is built (no probe
//! layout: a map too large for one volume still lightmaps). Each placed mesh part becomes one entry
//! named and keyed after its entity ([`regia_asset::lightmap::LightmapEntryKey`]), and because the
//! export sorts by guid and the bake's random streams are keyed by the entry's name, the file does
//! not depend on the order of the level's entities or on `--threads`.

use std::path::{Path, PathBuf};

use regia_agent::bake_export::{self, ExportError, ExportOptions, NoMeshInfo, slash};
use regia_asset::lightmap::LightmapEntryKey;
use regia_core::id::Guid;
use regia_pt::bake_desc::{BakeDescError, GiBakeDescription};
use regia_render::{DirectionalLight, MeshSource};
use regia_scene::bake_query::gi_entities;
use regia_scene::prelude::LightmapSurface;
use serde_json::{Value, json};

use super::{BakeLightmapsArgs, InstancePart, LightmapCommandError};
use crate::commands::bake_gi::GltfBakeLoader;
use crate::commands::bake_gi::from_scene::{self, FromSceneError};

impl From<ExportError> for LightmapCommandError {
    fn from(e: ExportError) -> Self {
        Self::new(e.code, e.entity, e.message)
    }
}

impl From<FromSceneError> for LightmapCommandError {
    fn from(e: FromSceneError) -> Self {
        Self::new(e.code, e.entity, e.message)
    }
}

impl From<BakeDescError> for LightmapCommandError {
    fn from(e: BakeDescError) -> Self {
        Self::new(e.code, e.path, e.message)
    }
}

/// A level ready to lay out and bake.
#[derive(Clone, Debug)]
pub struct LevelBake {
    /// The traced scene.
    pub scene: regia_pt::PtScene,
    /// One per placed mesh part, in instance order.
    pub parts: Vec<InstancePart>,
    /// `<project>/assets/maps/<level stem>.flmap`.
    pub default_out: PathBuf,
    /// The project root.
    pub project: PathBuf,
    /// The bake input's hash (the static meshes' files and transforms, the sun and the sky).
    pub input_hash: String,
    /// Spawn warnings (a prefab whose template is missing, an unregistered component).
    pub warnings: Vec<String>,
    /// What the export counted and skipped (`statics`, `skipped_by`, `skipped_entities`, ...).
    pub export: Value,
}

impl LevelBake {
    /// Adds what the level contributes to the command's JSON.
    pub fn describe(&self, out: &mut Value) {
        out["level"] = json!({
            "project": slash(&self.project),
            "input_hash": self.input_hash,
            "statics": self.export["statics"],
            "skipped": self.export["skipped"],
            "skipped_by": self.export["skipped_by"],
            "skipped_entities": self.export["skipped_entities"],
            "sun": self.export["sun"],
            "sky": self.export["sky"],
            "warnings": self.warnings,
        });
    }
}

/// Reads the level at `path` and builds what the bake needs.
///
/// # Errors
///
/// The export's codes (`export_read_failed`, `export_level_parse`, `export_mesh_missing`,
/// `export_terrain_ambiguous`, `export_no_input`, ...), the bake description's (`bake_mesh_load`,
/// ...) and `lightmap_scene` for an entity's bad `LightmapSurface` (`path` is its label).
pub fn load(path: &Path, args: &BakeLightmapsArgs) -> Result<LevelBake, LightmapCommandError> {
    let _span = tracing::info_span!("lightmap_level_load").entered();
    let level = from_scene::read_level(path)?;
    let project = args.project.as_deref().map_or_else(
        || from_scene::find_project(path),
        |p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()),
    );
    let label = slash(path);
    let mut headless = regia_scene::bake_query::spawn_headless(&level.level.scene, |r| {
        r.register::<MeshSource>();
        r.register::<DirectionalLight>();
    })
    .map_err(|e| LightmapCommandError::new("export_level_parse", Some(label.clone()), e.to_string()))?;
    let statics = bake_export::collect_statics(&mut headless.world)?;
    let mut surfaces = std::collections::HashMap::new();
    for e in gi_entities(&mut headless.world) {
        let surface = headless
            .world
            .get::<LightmapSurface>(e.entity)
            .copied()
            .unwrap_or_default();
        if !(surface.density_scale.is_finite() && surface.density_scale > 0.0) {
            return Err(LightmapCommandError::new(
                "lightmap_scene",
                Some(e.label()),
                format!(
                    "LightmapSurface.density_scale {} must be a number above 0",
                    surface.density_scale
                ),
            ));
        }
        surfaces.insert(e.guid, (surface, e.label()));
    }
    // The description is never written: it sits in the project root only so its mesh paths need
    // no `..` through a directory that may not exist. The probe layout it names is not built.
    let desc_path = project.join("lightmap.bake.ron");
    let options = ExportOptions {
        terrain_spacing: 1.0,
        bounds: None,
        bundle: None,
        terrain_holes: None,
        sun: args.sun,
    };
    let export = bake_export::export(
        &statics,
        &level.level.settings,
        &project,
        &desc_path,
        &options,
        &mut NoMeshInfo,
    )?;
    let (desc, _) = GiBakeDescription::parse_versioned(&export.text)?;
    let (scene, placed) = desc.build_scene_with(&project, &mut GltfBakeLoader)?;
    let mut parts = Vec::new();
    for placement in &placed {
        let guid: Guid = placement.id.parse().map_err(|_| {
            LightmapCommandError::new(
                "lightmap_scene",
                Some(placement.id.clone()),
                "the placement is not named by an entity guid",
            )
        })?;
        let (surface, entity_label) = surfaces
            .get(&guid)
            .cloned()
            .unwrap_or_else(|| (LightmapSurface::default(), guid.to_string()));
        for part in 0..placement.parts {
            parts.push(InstancePart {
                instance: placement.first_instance + part,
                key: LightmapEntryKey {
                    entity: guid,
                    part: part as u32,
                },
                label: entity_label.clone(),
                density_scale: surface.density_scale,
                receive: surface.receive,
            });
        }
    }
    let stem = bake_export::file_stem_to_first_dot(&label);
    let default_out = regia_core::project_path::resolve_write(&project, &format!("assets/maps/{stem}.flmap"))
        .map_err(|e| {
            LightmapCommandError::new(
                "export_path_invalid",
                Some(label.clone()),
                format!("assets/maps/{stem}.flmap leaves the project ({}): {e}", e.code()),
            )
        })?;
    Ok(LevelBake {
        scene,
        parts,
        default_out,
        project,
        input_hash: export.input_hash,
        warnings: headless.warnings,
        export: export.summary,
    })
}

/// Names the lightmap written at `out` in the level at `path`'s `look.lightmap.file` (keeping its
/// overrides) and rewrites the level through the scene writer, atomically (ADR-2313); nothing is
/// written when it already names it. The file is rewritten in the current scene format (canonical
/// text; comments are not kept). Returns whether the file changed.
///
/// # Errors
///
/// `lightmap_write_level`: the level is a `.level.d` directory, or `out` is not under the
/// project's `assets/` (a level names its lightmap by asset path); `export_level_parse` when the
/// level does not read or the rewritten text does not load back; `export_write_failed`.
pub fn write_level(path: &Path, project: &Path, out: &Path) -> Result<bool, LightmapCommandError> {
    let label = slash(path);
    let refuse =
        |message: String| LightmapCommandError::new("lightmap_write_level", Some(label.clone()), message);
    if path.is_dir() {
        return Err(refuse(
            "--write-level rewrites a .level.ron file, not a .level.d directory".into(),
        ));
    }
    let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let assets = absolute(project).join("assets");
    let file = absolute(out);
    let Ok(rel) = file.strip_prefix(&assets) else {
        return Err(refuse(format!(
            "{} is not under {}: a level names its lightmap by asset path",
            file.display(),
            assets.display()
        )));
    };
    let rel = slash(rel);
    let read = from_scene::read_level(path)?;
    let mut updated = read.level.clone();
    let current = updated.settings.look.lightmap.clone().unwrap_or_default();
    if current.file == rel {
        return Ok(false);
    }
    updated.settings.look.lightmap = Some(regia_scene::level::LevelLightmap {
        file: rel.clone(),
        ..current
    });
    let parse = |e: String| LightmapCommandError::new("export_level_parse", Some(label.clone()), e);
    let text = updated
        .to_ron_string()
        .map_err(|e| LightmapCommandError::new("export_write_failed", Some(label.clone()), e.to_string()))?;
    let back =
        regia_scene::level::Level::from_ron_str(updated.id, &text).map_err(|e| parse(e.to_string()))?;
    if back.settings.look.lightmap.as_ref().map(|l| l.file.as_str()) != Some(rel.as_str()) {
        return Err(parse("the rewritten level does not name the lightmap".into()));
    }
    bake_export::write_atomic(path, text.as_bytes())
        .map_err(|e| LightmapCommandError::new("export_write_failed", Some(label.clone()), e.to_string()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use regia_asset::lightmap::Lightmap;

    use super::super::{UnwrapMode, execute};
    use super::*;

    const GUID_A: &str = "00000000-0000-0000-0000-00000000000a";
    const GUID_B: &str = "00000000-0000-0000-0000-00000000000b";
    const GUID_DYNAMIC: &str = "00000000-0000-0000-0000-00000000000c";
    /// The GI bake end-to-end test's mesh: two rooms of a few boxes.
    const ROOM_GLTF: &str = include_str!("../../../tests/fixtures/bake_e2e/assets/meshes/room.gltf");

    fn entity(guid: &str, name: &str, x: f32, statics: bool, extra: &str) -> String {
        let mobility = if statics {
            "\"regia_scene::components::GiMobility\": Static,"
        } else {
            ""
        };
        format!(
            r#"(
            guid: "{guid}",
            name: "{name}",
            components: {{
                "regia_scene::transform::Transform": (translation: ({x:?}, 0.0, 0.0)),
                "regia_render::runtime::MeshSource": (mesh: "assets/meshes/room.gltf", material: ""),
                {mobility}
                {extra}
            }},
        )"#
        )
    }

    fn level_text(entities: &[String]) -> String {
        format!(
            "SceneFile(format_version: 10, entities: [{}], resources: {{ \"regia_scene::level::LevelSettings\": (atmosphere: (preset: Earth, time_of_day: Some((hour: 12.0, day_of_year: 172, latitude_deg: 40.0)))), }})",
            entities.join(",")
        )
    }

    fn project(tag: &str, text: &str) -> (PathBuf, PathBuf) {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let meshes = dir.join("assets/meshes");
        std::fs::create_dir_all(&meshes).expect("dirs");
        regia_test_utils::write_atomic(&meshes.join("room.gltf"), ROOM_GLTF.as_bytes()).expect("mesh");
        let levels = dir.join("assets/levels");
        std::fs::create_dir_all(&levels).expect("dirs");
        let level = levels.join("yard.level.ron");
        regia_test_utils::write_atomic(&level, text.as_bytes()).expect("level");
        regia_test_utils::write_atomic(
            &dir.join(regia_core::brand::MANIFEST_FILE),
            b"[project]\nname = \"t\"\nid = \"com.example.t\"\n",
        )
        .expect("manifest");
        (dir, level)
    }

    fn args_for(level: &Path) -> BakeLightmapsArgs {
        BakeLightmapsArgs {
            scene: None,
            level: Some(level.to_path_buf()),
            project: None,
            sun: Some(bake_export::SunOverride {
                elevation_deg: 40.0,
                azimuth_deg: 30.0,
                lux: 1000.0,
            }),
            unwrap: UnwrapMode::Atlas,
            seam_budget: None,
            out: None,
            density: 2.0,
            padding: 2,
            max_atlas: 1024,
            max_pages: super::super::DEFAULT_MAX_PAGES,
            chart_angle: 60.0,
            samples: 4,
            bounces: 2,
            seed: 5,
            direct_sun: false,
            gpu: false,
            gpu_opts: crate::commands::bake_lightmaps::GpuBakeArgs::default(),
            compare: None,
            no_filter: false,
            denoise: crate::commands::bake_lightmaps::DenoiseMode::On,
            denoise_iterations: None,
            encoding: crate::commands::bake_lightmaps::TexelEncodingArg::Rgb9e5,
            crease: crate::commands::bake_lightmaps::Switch::On,
            stitch: crate::commands::bake_lightmaps::Switch::On,
            auto_density: crate::commands::bake_lightmaps::Switch::On,
            no_directional: false,
            max_samples: 0,
            noise_target: 0.0,
            threads: 1,
            min_area: 0.0,
            instances: Vec::new(),
            dry_run: false,
            inspect: false,
            meshlets: false,
            write_level: false,
            checkpoint: crate::commands::bake_checkpoint::CheckpointArgs::default(),
            cells: crate::commands::bake_lightmaps::cells::CellArgs::default(),
        }
    }

    /// A level bakes with no `.bake.ron` on disk: its static entities become keyed entries, the
    /// dynamic one stays out, the default output is under `assets/maps` and nothing else is written.
    #[test]
    fn a_level_bakes_directly_with_entries_keyed_by_entity() {
        let text = level_text(&[
            entity(GUID_A, "wall", 0.0, true, ""),
            entity(GUID_B, "crate", 40.0, true, ""),
            entity(GUID_DYNAMIC, "cart", 80.0, false, ""),
        ]);
        let (dir, level) = project("lightmap_level_bake", &text);
        let v = execute(&args_for(&level)).expect("bake");
        assert_eq!(v["ok"], true, "{v}");
        let out = dir.join("assets/maps/yard.flmap");
        // Compared as paths: the reply is a native host path (`\` on Windows, ADR-4924).
        assert_eq!(
            v["out"].as_str().map(std::path::Path::new),
            Some(out.as_path()),
            "{v}"
        );
        assert_eq!(v["atlas"]["generator"], "atlas");
        let map = Lightmap::read(&out).expect("the file reads back");
        assert_eq!(map.entries.len(), 2);
        let guid = |s: &str| s.parse::<Guid>().expect("guid");
        for (entry, g) in map.entries.iter().zip([GUID_A, GUID_B]) {
            assert_eq!(
                entry.key,
                Some(LightmapEntryKey {
                    entity: guid(g),
                    part: 0
                })
            );
            assert_eq!(entry.name, format!("entity_{g}_0"));
        }
        assert_eq!(
            map.entry_for_key(LightmapEntryKey {
                entity: guid(GUID_B),
                part: 0
            }),
            Some(1)
        );
        assert_eq!(v["entries"][0]["entity"], GUID_A);
        assert_eq!(v["entries"][1]["label"], format!("crate ({GUID_B})"));
        assert_eq!(v["level"]["statics"], 2);
        assert_eq!(v["level"]["skipped_by"]["dynamic"], 1);
        assert_eq!(v["level"]["input_hash"].as_str().map(str::len), Some(64));
        assert!(
            !dir.join("lightmap.bake.ron").exists() && !dir.join("bake").exists(),
            "no description is left behind"
        );
        // The bake lights the cubes: the sky is the level's atmosphere, the sun the override.
        assert!(
            (0..map.height).any(|y| (0..map.width).any(|x| map.irradiance_at(x, y)[1] > 0.0)),
            "{v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file's bytes depend only on the level's content: the same level baked twice, baked on
    /// another thread count, and with its entities written in another order are identical.
    #[test]
    fn the_bake_is_deterministic_and_does_not_depend_on_the_level_s_order() {
        let a = entity(GUID_A, "wall", 0.0, true, "");
        let b = entity(GUID_B, "crate", 40.0, true, "");
        let (dir, level) = project("lightmap_level_determinism", &level_text(&[a.clone(), b.clone()]));
        let bake = |threads: usize, name: &str| {
            let mut args = args_for(&level);
            args.threads = threads;
            let out = dir.join(name);
            args.out = Some(out.clone());
            execute(&args).expect("bake");
            std::fs::read(&out).expect("read")
        };
        let first = bake(1, "first.flmap");
        assert_eq!(first, bake(1, "second.flmap"), "a re-bake is byte-identical");
        assert_eq!(
            first,
            bake(3, "threads.flmap"),
            "the thread count does not matter"
        );
        regia_test_utils::write_atomic(&level, level_text(&[b, a]).as_bytes()).expect("reorder");
        assert_eq!(
            first,
            bake(1, "reordered.flmap"),
            "the level's order does not matter"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A level of many entities, which the layout charts and packs one mesh per worker (ADR-2955,
    /// ADR-3121), bakes the same atlas, the same UV2 and the same file bytes on 1, 2 and 8 threads.
    #[test]
    fn many_entities_bake_the_same_atlas_uv2_and_bytes_on_1_2_and_8_threads() {
        let guids: Vec<String> = (0..12u32)
            .map(|k| format!("00000000-0000-0000-0000-0000000001{k:02x}"))
            .collect();
        let entities: Vec<String> = guids
            .iter()
            .enumerate()
            .map(|(k, g)| entity(g, &format!("prop{k}"), 40.0 * k as f32, true, ""))
            .collect();
        let (dir, level) = project("lightmap_level_threads", &level_text(&entities));
        let bake = |threads: usize| {
            let mut args = args_for(&level);
            args.threads = threads;
            args.samples = 1;
            args.bounces = 1;
            let out = dir.join(format!("threads_{threads}.flmap"));
            args.out = Some(out.clone());
            let v = execute(&args).expect("bake");
            assert_eq!(v["entries"].as_array().map(Vec::len), Some(12), "{v}");
            let bytes = std::fs::read(&out).expect("read");
            let map = Lightmap::read(&out).expect("reads back");
            (bytes, map)
        };
        let (one, map_one) = bake(1);
        for threads in [2, 8] {
            let (bytes, map) = bake(threads);
            let rects = |m: &Lightmap| m.entries.iter().map(|e| e.rect).collect::<Vec<_>>();
            assert_eq!(
                (map.width, map.height),
                (map_one.width, map_one.height),
                "threads {threads}"
            );
            assert_eq!(rects(&map), rects(&map_one), "the atlas, threads {threads}");
            assert_eq!(map.unwraps, map_one.unwraps, "UV2, threads {threads}");
            assert_eq!(bytes, one, "the file, threads {threads}");
        }
        // The same on pages (ADR-3300): a page side the twelve entities need several pages of.
        let paged = |threads: usize, side: u32, dry: bool| {
            let mut args = args_for(&level);
            args.threads = threads;
            args.samples = 1;
            args.bounces = 1;
            args.max_atlas = side;
            args.max_pages = 8;
            args.dry_run = dry;
            let out = dir.join(format!("paged_{threads}.flmap"));
            args.out = Some(out.clone());
            execute(&args).ok().map(|v| (v, out))
        };
        let side = (16..=map_one.width.max(map_one.height))
            .step_by(4)
            .find(|&s| {
                paged(1, s, true).is_some_and(|(v, _)| {
                    v["pages"].as_u64().is_some_and(|p| p >= 2)
                        && v["atlas"]["fit_scale"].as_f64() == Some(1.0)
                })
            })
            .expect("a page side that needs several pages");
        let (_, out) = paged(1, side, false).expect("paged bake");
        let (one, map_one) = (
            std::fs::read(&out).expect("read"),
            Lightmap::read(&out).expect("reads back"),
        );
        assert!(map_one.pages() >= 2);
        for threads in [2, 8] {
            let (_, out) = paged(threads, side, false).expect("paged bake");
            let map = Lightmap::read(&out).expect("reads back");
            let placed = |m: &Lightmap| m.entries.iter().map(|e| (e.page, e.rect)).collect::<Vec<_>>();
            assert_eq!(placed(&map), placed(&map_one), "pages, threads {threads}");
            assert_eq!(map.unwraps, map_one.unwraps, "UV2 on pages, threads {threads}");
            assert_eq!(
                std::fs::read(&out).expect("read"),
                one,
                "the paged file, threads {threads}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `.flmap` payload hash (blake3 over the sections) of the one-page bake in
    /// [`a_level_past_one_atlas_bakes_on_pages_at_the_asked_density`], with ADR-4108's Sobol
    /// first-hemisphere sampling and density-based denoiser default (four passes at density 6).
    /// Before those intended changes, a0e8cb026's PCG bake with three denoiser passes
    /// pinned 7a4eaa4b13c6d2fba4c4a06ca86366eab9b92a66baab55719c739bf429c88aa4.
    /// The same project and page side 308 now pin the new default bake's sections exactly;
    /// the format's v6/v7 compatibility is separately checked by the lightmap migration tests.
    const ONE_PAGE_PAYLOAD: &str = "c0293f34adea64b3f9c8b3e1f2dc6011f8f3941bb34404fadaead789e126f722";

    /// ADR-3300 acceptance: a level whose charts need about two and a half pages of the atlas side
    /// bakes on three pages at the density it asked for (`fit_scale` 1, every entry at the texels
    /// per meter a one-page bake on a large atlas gives it), and the same bake capped at
    /// `--max-pages 1` is the one-page bake of the old rule (`fit_scale` below 1, one page).
    #[test]
    fn a_level_past_one_atlas_bakes_on_pages_at_the_asked_density() {
        // Ten equal props: four fill a page, so ten need two and a half pages.
        let guids: Vec<String> = (0..10u32)
            .map(|k| format!("00000000-0000-0000-0000-0000000002{k:02x}"))
            .collect();
        let entities: Vec<String> = guids
            .iter()
            .enumerate()
            .map(|(k, g)| entity(g, &format!("prop{k}"), 40.0 * k as f32, true, ""))
            .collect();
        let (dir, level) = project("lightmap_level_pages", &level_text(&entities));
        let run = |max_atlas: u32, max_pages: u32, dry: bool, name: &str| {
            let mut args = args_for(&level);
            args.density = 6.0;
            args.samples = 1;
            args.bounces = 1;
            args.max_atlas = max_atlas;
            args.max_pages = max_pages;
            args.dry_run = dry;
            args.out = Some(dir.join(name));
            execute(&args).expect("bake")
        };
        let tpm = |v: &Value| -> Vec<f64> {
            v["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .map(|e| e["texels_per_metre"].as_f64().expect("texels_per_metre"))
                .collect()
        };
        // The asked density: a one-page layout on an atlas large enough.
        let wide = run(4096, 1, true, "wide.flmap");
        assert_eq!(
            (wide["pages"].as_u64(), wide["atlas"]["fit_scale"].as_f64()),
            (Some(1), Some(1.0))
        );
        let want = tpm(&wide);
        let area: f64 = wide["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|e| {
                let r = &e["rect"];
                r[2].as_f64().unwrap_or(0.0) * r[3].as_f64().unwrap_or(0.0)
            })
            .sum();
        // The smallest page side on which the charts need three pages at that density.
        let probe = |s: u32| {
            let mut args = args_for(&level);
            args.density = 6.0;
            args.max_atlas = s;
            args.max_pages = 8;
            args.dry_run = true;
            execute(&args).ok()
        };
        let from = ((area / 3.0).sqrt() as u32 / 4 * 4).max(16);
        let side = (from..=4096u32)
            .step_by(4)
            .find(|&s| {
                probe(s).is_some_and(|v| {
                    v["pages"].as_u64() == Some(3) && v["atlas"]["fit_scale"].as_f64() == Some(1.0)
                })
            })
            .expect("a page side that needs three pages");
        // About two and a half pages: the first two pages hold the same number of props, the third
        // fewer (ten equal props, four a page: 4 + 4 + 2, so the level needs 10 / 4 = 2.5 pages of
        // room). The rectangles' own area is less (the gutters and the shelves leave room).
        let probed = probe(side).expect("probe");
        let mut per_page = [0u32; 3];
        for e in probed["entries"].as_array().expect("entries") {
            let p = e["page"].as_u64().expect("page") as usize;
            per_page[p] += 1;
        }
        let full = per_page[0];
        assert!(
            per_page[1] == full && per_page[2] > 0 && per_page[2] < full,
            "side {side}: props per page {per_page:?}"
        );
        let need = 10.0 / f64::from(full);
        assert!(
            need > 2.0 && need < 3.0,
            "side {side}: props per page {per_page:?}, {need} pages of room"
        );
        let fill = area / f64::from(side * side);
        eprintln!("side {side}: props per page {per_page:?}, {need} pages of room, rectangles {fill} pages");

        let paged = run(side, 4, false, "paged.flmap");
        assert_eq!(paged["pages"], 3, "{paged:#}");
        assert_eq!(paged["atlas"]["fit_scale"], 1.0);
        assert_eq!(paged["density_scaled"], Value::Null);
        assert_eq!(tpm(&paged), want, "every entry at the asked density");
        let map = Lightmap::read(&dir.join("paged.flmap")).expect("reads");
        assert_eq!(map.pages(), 3);
        assert_eq!((map.width, map.height <= side), (side / 4 * 4, true));
        for p in 0..3 {
            assert!(map.entries.iter().any(|e| e.page == p), "page {p} holds an entry");
        }
        let pages_json: Vec<u64> = paged["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|e| e["page"].as_u64().expect("page"))
            .collect();
        assert_eq!(
            pages_json,
            map.entries.iter().map(|e| u64::from(e.page)).collect::<Vec<_>>()
        );
        // Lit on every page.
        for p in 0..3u32 {
            let lit = (0..map.height)
                .any(|y| (0..map.width).any(|x| map.irradiance_at(x, p * map.height + y)[1] > 0.0));
            assert!(lit, "page {p} holds light");
        }

        let capped = run(side, 1, false, "capped.flmap");
        assert_eq!(capped["pages"], 1);
        let fit = capped["atlas"]["fit_scale"].as_f64().expect("fit_scale");
        assert!(fit < 1.0, "{capped:#}");
        assert_eq!(capped["density_scaled"]["max_pages"], 1);
        let bytes = std::fs::read(dir.join("capped.flmap")).expect("read");
        let one = Lightmap::from_bytes(&bytes).expect("reads");
        assert_eq!(one.pages(), 1);
        assert!(one.entries.iter().all(|e| e.page == 0));
        let hash: String = Lightmap::read_header(&bytes)
            .expect("header")
            .payload_hash
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        eprintln!(
            "one-page payload {hash} (side {side}, fit {fit}; dir {})",
            dir.display()
        );
        // Pinned where it was measured: the bake's float results go through the platform's libm,
        // which may round differently on other targets.
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(
                (side, hash.as_str()),
                (308, ONE_PAGE_PAYLOAD),
                "the one-page bake keeps the ADR-4108 default bake sections"
            );
        }
        if std::env::var_os("REGIA_KEEP_PAGES_PROJECT").is_none() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// `LightmapSurface` applies the density rule per entity and opts an entity out; a bad scale is
    /// refused naming it.
    #[test]
    fn lightmap_surface_scales_density_and_opts_out() {
        const SURFACE: &str = "regia_scene::components::LightmapSurface";
        let text = level_text(&[
            entity(
                GUID_A,
                "hero",
                0.0,
                true,
                &format!("\"{SURFACE}\": (density_scale: 2.0, receive: true),"),
            ),
            entity(GUID_B, "plain", 40.0, true, ""),
            entity(
                GUID_DYNAMIC,
                "skip",
                80.0,
                true,
                &format!("\"{SURFACE}\": (density_scale: 1.0, receive: false),"),
            ),
        ]);
        let (dir, level) = project("lightmap_level_surface", &text);
        let mut args = args_for(&level);
        args.dry_run = true;
        let v = execute(&args).expect("dry run");
        let entries = v["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 2, "{v}");
        assert_eq!(entries[0]["texels_per_metre"], 4.0);
        assert_eq!(entries[1]["texels_per_metre"], 2.0);
        assert_eq!(v["skipped_opted_out"].as_array().expect("list").len(), 1);

        let bad = level_text(&[entity(
            GUID_A,
            "hero",
            0.0,
            true,
            &format!("\"{SURFACE}\": (density_scale: -1.0, receive: true),"),
        )]);
        regia_test_utils::write_atomic(&level, bad.as_bytes()).expect("level");
        let e = execute(&args).expect_err("bad scale");
        assert_eq!(e.code, "lightmap_scene");
        assert_eq!(e.path, Some(format!("hero ({GUID_A})")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The directory form of a level bakes the same file as the single-file form: same entities,
    /// and the bake depends on neither their order nor where they are stored. The output is named
    /// after the level, not `<level>.level`.
    #[test]
    fn a_split_level_bakes_the_same_file() {
        let text = level_text(&[
            entity(GUID_A, "wall", 0.0, true, ""),
            entity(GUID_B, "crate", 40.0, true, ""),
        ]);
        let (dir, level) = project("lightmap_level_split", &text);
        execute(&args_for(&level)).expect("bakes the file form");
        let from_file = std::fs::read(dir.join("assets/maps/yard.flmap")).expect("read");
        std::fs::remove_file(dir.join("assets/maps/yard.flmap")).expect("remove");

        let split = dir.join("assets/levels/yard.level.d");
        regia_scene::level_dir::split_level_file(&level, &split).expect("splits");
        std::fs::remove_file(&level).expect("only the directory is left");
        let v = execute(&args_for(&split)).expect("bakes the directory form");
        assert_eq!(v["entries"].as_array().map(Vec::len), Some(2), "{v}");
        let from_dir = std::fs::read(dir.join("assets/maps/yard.flmap")).expect("named after the level");
        assert_eq!(from_file, from_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A mesh with two primitives, of different materials so nothing merges them, gets one entry per
    /// part, each with its own key, name and rectangle, and the density rule applies to both.
    #[test]
    fn a_multi_part_mesh_gets_one_keyed_entry_per_part() {
        const SURFACE: &str = "regia_scene::components::LightmapSurface";
        let text = level_text(&[entity(
            GUID_A,
            "kit",
            0.0,
            true,
            &format!("\"{SURFACE}\": (density_scale: 2.0, receive: true),"),
        )]);
        let (dir, level) = project("lightmap_level_parts", &text);
        let mut gltf: Value = serde_json::from_str(ROOM_GLTF).expect("the fixture is json");
        let second = {
            let mut p = gltf["meshes"][0]["primitives"][0].clone();
            p["material"] = json!(1);
            p
        };
        gltf["meshes"][0]["primitives"]
            .as_array_mut()
            .expect("primitives")
            .push(second);
        let material = gltf["materials"][0].clone();
        gltf["materials"]
            .as_array_mut()
            .expect("materials")
            .push(material);
        regia_test_utils::write_atomic(&dir.join("assets/meshes/room.gltf"), gltf.to_string().as_bytes())
            .expect("mesh");

        let v = execute(&args_for(&level)).expect("bake");
        let entries = v["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 2, "{v}");
        for (k, entry) in entries.iter().enumerate() {
            assert_eq!(entry["entity"], GUID_A);
            assert_eq!(entry["part"], k);
            assert_eq!(entry["name"], format!("entity_{GUID_A}_{k}"));
            assert_eq!(entry["label"], format!("kit ({GUID_A})"));
            assert_eq!(entry["texels_per_metre"], 4.0, "{entry}");
        }
        assert_ne!(entries[0]["rect"], entries[1]["rect"]);
        let map = Lightmap::read(&dir.join("assets/maps/yard.flmap")).expect("reads back");
        let entity = GUID_A.parse::<Guid>().expect("guid");
        for part in 0..2 {
            assert_eq!(
                map.entry_for_key(LightmapEntryKey { entity, part }),
                Some(part as usize)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn level_errors_carry_the_export_s_codes() {
        let (dir, level) = project(
            "lightmap_level_errors",
            &level_text(&[entity(GUID_A, "wall", 0.0, true, "")]),
        );
        let missing = dir.join("nope.level.ron");
        assert_eq!(
            execute(&args_for(&missing)).expect_err("missing").code,
            "export_read_failed"
        );
        std::fs::remove_file(dir.join("assets/meshes/room.gltf")).expect("remove");
        let e = execute(&args_for(&level)).expect_err("no mesh");
        assert_eq!(e.code, "export_mesh_missing");
        assert!(e.path.as_deref().is_some_and(|p| p.contains("wall")), "{e:?}");
        regia_test_utils::write_atomic(
            &level,
            level_text(&[entity(GUID_A, "cart", 0.0, false, "")]).as_bytes(),
        )
        .expect("level");
        assert_eq!(
            execute(&args_for(&level)).expect_err("nothing static").code,
            "export_no_input"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A closed box of 6 faces (4 vertices each, outward normals) centred at `c` with half size `h`.
    fn box_part(c: [f32; 3], h: f32) -> (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<u32>) {
        let axes: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let (mut pos, mut nrm, mut idx) = (Vec::new(), Vec::new(), Vec::new());
        for (n, u, v) in axes {
            let base = pos.len() as u32;
            for (s, t) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                pos.push(std::array::from_fn(|i| c[i] + h * (n[i] + s * u[i] + t * v[i])));
                nrm.push(n);
            }
            idx.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        (pos, nrm, idx)
    }

    /// A glTF with one mesh of two primitives (two materials): two boxes side by side.
    fn write_two_part_gltf(dir: &Path) {
        let parts = [box_part([0.0, 1.0, 0.0], 1.0), box_part([3.0, 0.5, 0.0], 0.5)];
        let mut bin: Vec<u8> = Vec::new();
        let mut views = Vec::new();
        let mut accessors = Vec::new();
        let mut primitives = Vec::new();
        for (k, (pos, nrm, idx)) in parts.iter().enumerate() {
            let first = accessors.len();
            let lo: [f32; 3] = std::array::from_fn(|i| pos.iter().map(|p| p[i]).fold(f32::MAX, f32::min));
            let hi: [f32; 3] = std::array::from_fn(|i| pos.iter().map(|p| p[i]).fold(f32::MIN, f32::max));
            for (data, kind) in [(pos, "VEC3"), (nrm, "VEC3")] {
                let offset = bin.len();
                for p in data {
                    for x in p {
                        bin.extend(x.to_le_bytes());
                    }
                }
                views.push(json!({ "buffer": 0, "byteOffset": offset, "byteLength": bin.len() - offset, "target": 34962 }));
                let mut a = json!({ "bufferView": views.len() - 1, "componentType": 5126, "count": data.len(), "type": kind });
                if accessors.len() == first {
                    a["min"] = json!(lo);
                    a["max"] = json!(hi);
                }
                accessors.push(a);
            }
            let offset = bin.len();
            for i in idx {
                bin.extend(i.to_le_bytes());
            }
            views.push(json!({ "buffer": 0, "byteOffset": offset, "byteLength": bin.len() - offset, "target": 34963 }));
            accessors.push(json!({ "bufferView": views.len() - 1, "componentType": 5125, "count": idx.len(), "type": "SCALAR" }));
            primitives.push(json!({
                "attributes": { "POSITION": first, "NORMAL": first + 1 },
                "indices": first + 2,
                "material": k,
            }));
        }
        let gltf = json!({
            "asset": { "version": "2.0" },
            "buffers": [{ "uri": "pair.bin", "byteLength": bin.len() }],
            "bufferViews": views,
            "accessors": accessors,
            "materials": [
                { "name": "stone", "pbrMetallicRoughness": { "baseColorFactor": [0.5, 0.5, 0.5, 1.0], "metallicFactor": 0.0, "roughnessFactor": 1.0 } },
                { "name": "wood", "pbrMetallicRoughness": { "baseColorFactor": [0.6, 0.4, 0.2, 1.0], "metallicFactor": 0.0, "roughnessFactor": 1.0 } },
            ],
            "meshes": [{ "name": "pair", "primitives": primitives }],
            "nodes": [{ "name": "pair", "mesh": 0 }],
            "scenes": [{ "nodes": [0] }],
            "scene": 0,
        });
        let meshes = dir.join("assets/meshes");
        std::fs::create_dir_all(&meshes).expect("dirs");
        regia_test_utils::write_atomic(&meshes.join("pair.bin"), &bin).expect("bin");
        regia_test_utils::write_atomic(&meshes.join("pair.gltf"), gltf.to_string().as_bytes()).expect("gltf");
    }

    /// What an opened level shows: its receivers as `(owner guid, part, entry)` (sorted), how many
    /// the renderer's receiver system applied, and the installed lightmap.
    type Opened = (Vec<(Guid, u32, String)>, usize, std::sync::Arc<Lightmap>);

    /// Opens the level at `path` like a game does (the framework's lightmap plugin, a host that
    /// resolves each `MeshSource` into one renderer per part, the renderer's receiver system), and
    /// with `save` writes the level back from the world (what the editor's save does).
    fn open_level_file(project: &Path, path: &Path, save: bool) -> Opened {
        use regia_app::prelude::*;
        use regia_pt::bake_meshes::BakeMeshLoader;
        use regia_render::lightmap::{
            InstalledLightmap, LightmapReceiver, LightmapReceiverReport, MeshPartOf, apply_lightmap_receivers,
        };
        use regia_scene::level::{LEVEL_SETTINGS_PATH, Level, LevelId, LevelLibrary, LoadMode, open_level};
        use regia_scene::prelude::{EntityGuid, ScenePlugin};

        let mut app = App::new();
        app.add_plugins((ScenePlugin, regia_framework::lightmap_level::LightmapLevelPlugin));
        regia_render::register_render_types(&mut app);
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: project.to_path_buf(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("asset server");
        app.insert_resource(server);
        app.insert_resource(regia_render::RuntimeRenderer::default());
        app.init_resource::<LightmapReceiverReport>();
        let id = LevelId(Guid::from_u128(0x1e7e1));
        let text = std::fs::read_to_string(path).expect("level");
        let level = Level::from_ron_str(id, &text).expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(
            app.world()
                .resource::<regia_render::RuntimeRenderer>()
                .0
                .warm_lightmaps()
        );

        // The host's mesh resolver: part 0 on the level entity, part k on a part entity.
        let mut q = app.world_mut().query::<(Entity, &MeshSource)>();
        let sources: Vec<(Entity, String)> = q.iter(app.world()).map(|(e, s)| (e, s.mesh.clone())).collect();
        let material = std::sync::Arc::new(regia_render::LabMaterial::default());
        for (owner, mesh) in sources {
            let loaded = GltfBakeLoader.load(&project.join(&mesh)).expect("mesh loads");
            for (k, part) in loaded.parts.iter().enumerate() {
                let m = &part.mesh;
                let vertices = (0..m.positions.len())
                    .map(|i| {
                        regia_render::LabVertex::from_pos_normal_uv(
                            m.positions[i],
                            m.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]),
                            m.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                        )
                    })
                    .collect();
                let cpu = std::sync::Arc::new(regia_render::CpuMesh {
                    vertices,
                    indices: m.indices.clone(),
                });
                let renderer = regia_render::MeshRenderer::new(cpu, material.clone());
                if k == 0 {
                    app.world_mut().entity_mut(owner).insert(renderer);
                } else {
                    app.world_mut().spawn((
                        renderer,
                        MeshPartOf {
                            owner,
                            part: k as u32,
                        },
                    ));
                }
            }
        }
        app.update();
        let mut schedule = Schedule::default();
        schedule.add_systems(apply_lightmap_receivers);
        schedule.run(app.world_mut());
        let applied = app.world().resource::<LightmapReceiverReport>().applied;
        // The hash the bake stored is the hash of the mesh the host draws (ADR-2656).
        let installed = app.world().resource::<InstalledLightmap>().lightmap.clone();
        let counts = regia_render::lightmap::lightmap_instance_counts(app.world(), installed.as_ref());
        assert_eq!(counts.instances, applied, "{counts:?}");
        assert_eq!(
            (counts.stale_instances, counts.unverified_instances),
            (0, 0),
            "{counts:?}"
        );

        let mut q = app
            .world_mut()
            .query::<(Entity, &LightmapReceiver, Option<&MeshPartOf>)>();
        let found: Vec<(Entity, String, Option<MeshPartOf>)> = q
            .iter(app.world())
            .map(|(e, r, p)| (e, r.entry.clone(), p.copied()))
            .collect();
        let mut receivers: Vec<(Guid, u32, String)> = found
            .into_iter()
            .map(|(e, entry, part)| {
                let owner = part.map_or(e, |p| p.owner);
                let guid = app.world().get::<EntityGuid>(owner).expect("owner guid").0;
                (guid, part.map_or(0, |p| p.part), entry)
            })
            .collect();
        receivers.sort();
        let map = app
            .world()
            .resource::<InstalledLightmap>()
            .lightmap
            .clone()
            .expect("the level's lightmap is installed");

        if save {
            let file = regia_scene::spawn::snapshot_scene(
                app.world_mut(),
                &regia_scene::spawn::SnapshotFilter {
                    level: Some(id),
                    resources: vec![LEVEL_SETTINGS_PATH.to_string()],
                },
            )
            .expect("snapshot");
            let text = Level::from_scene_file(id, file).to_ron_string().expect("ron");
            assert!(
                text.contains("regia_render::lightmap::LightmapReceiver"),
                "{text}"
            );
            assert!(text.contains("lightmap: Some("), "{text}");
            regia_test_utils::write_atomic(path, text.as_bytes()).expect("save");
        }
        (receivers, applied, map)
    }

    /// The brief's round trip (ADR-2313): a level with two two-part meshes bakes, names its
    /// lightmap (`--write-level`), opens with a receiver on every part, saves, and reopens with the
    /// same receivers and the same lightmap texels. A bake without `--write-level` leaves the level
    /// file byte-identical.
    #[test]
    fn a_two_part_level_bakes_saves_and_reopens_with_the_same_receivers() {
        let entity = |guid: &str, name: &str, x: f32| {
            format!(
                r#"(
            guid: "{guid}",
            name: "{name}",
            components: {{
                "regia_scene::transform::Transform": (translation: ({x:?}, 0.0, 0.0)),
                "regia_render::runtime::MeshSource": (mesh: "assets/meshes/pair.gltf", material: ""),
                "regia_scene::components::GiMobility": Static,
            }},
        )"#
            )
        };
        let text = level_text(&[entity(GUID_A, "hall", 0.0), entity(GUID_B, "annex", 12.0)]);
        let (dir, level) = project("lightmap_level_two_part", &text);
        write_two_part_gltf(&dir);
        let before = std::fs::read(&level).expect("level");

        // Without --write-level the level is untouched.
        let v = execute(&args_for(&level)).expect("bake");
        assert_eq!(v["ok"], true, "{v}");
        assert!(v.get("level_written").is_none());
        assert_eq!(std::fs::read(&level).expect("level"), before, "byte-identical");

        let mut args = args_for(&level);
        args.write_level = true;
        let v = execute(&args).expect("bake");
        assert_eq!(v["level_written"], true, "{v}");
        let map = Lightmap::read(&dir.join("assets/maps/yard.flmap")).expect("written");
        assert_eq!(map.entries.len(), 4, "two entities of two parts: {v}");
        let named = from_scene::read_level(&level)
            .expect("reads")
            .level
            .settings
            .look
            .lightmap;
        assert_eq!(named.map(|l| l.file), Some("maps/yard.flmap".to_string()));
        // A second --write-level finds the level already naming it.
        let v = execute(&args).expect("bake");
        assert_eq!(v["level_written"], false);

        let (first, applied, map1) = open_level_file(&dir, &level, true);
        let want: Vec<(Guid, u32, String)> = [GUID_A, GUID_B]
            .iter()
            .flat_map(|g| (0..2u32).map(move |p| (g.parse().expect("guid"), p, format!("entity_{g}_{p}"))))
            .collect();
        assert_eq!(first, want, "every part of both meshes receives");
        assert_eq!(applied, 4, "every receiver draws its entry's unwrap");

        let (again, applied, map2) = open_level_file(&dir, &level, false);
        assert_eq!(again, first, "the reopened level has the same receivers");
        assert_eq!(applied, 4);
        assert_eq!(map2.texels, map1.texels, "the same lightmap texels");
        assert_eq!(
            blake3::hash(&map2.to_bytes().expect("encode")),
            blake3::hash(&map1.to_bytes().expect("encode"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
