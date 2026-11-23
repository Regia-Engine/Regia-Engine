// SPDX-License-Identifier: MPL-2.0
//! The project map: a generated overview of kinds, prefabs, components, systems and levels
//! (spec 101 § 6).
//!
//! [`build_map`] is pure: files, a catalog and (optionally) a system access table in, a
//! [`ProjectMap`] out. Every list is sorted and nothing depends on time or absolute paths, so the
//! same project gives the same bytes ([`ProjectMap::to_json`], [`ProjectMap::to_markdown`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::roles::{ComponentProbe, classify, ron_says_sensor};
use super::scan::{KindCatalog, ProjectFiles, prefab_root, record_kind};
use super::systems::SystemAccessTable;
use super::{KindInfo, KindSource};

/// The map's JSON layout version.
pub const MAP_VERSION: u32 = 1;

/// File name of the markdown map.
pub const MAP_MARKDOWN: &str = "project-map.md";

/// File name of the JSON map.
pub const MAP_JSON: &str = "project-map.json";

/// A level's use of a kind or prefab.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LevelUse {
    /// The level (project-relative).
    pub level: String,
    /// How many entities.
    pub count: usize,
}

/// One kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindEntry {
    /// Name.
    pub name: String,
    /// Icon name.
    pub icon: String,
    /// `#rrggbb`.
    pub color: String,
    /// Category (`Uncategorized` when none).
    pub category: String,
    /// One-line description.
    pub description: String,
    /// Where it is declared.
    pub source: KindSource,
    /// Required component type paths.
    pub required: Vec<String>,
    /// Prefabs whose root names this kind.
    pub prefabs: Vec<String>,
    /// Levels using it.
    pub levels: Vec<LevelUse>,
}

impl KindEntry {
    /// The kind as a registry entry (`None` when the color does not parse).
    #[must_use]
    pub fn to_info(&self) -> Option<KindInfo> {
        Some(KindInfo {
            name: self.name.clone(),
            icon: self.icon.clone(),
            color: super::file::parse_color(&self.color).ok()?,
            description: self.description.clone(),
            category: (self.category != super::UNCATEGORIZED).then(|| self.category.clone()),
            required: self.required.clone(),
            source: self.source.clone(),
        })
    }
}

/// One prefab.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefabEntry {
    /// The file (project-relative).
    pub path: String,
    /// Its asset id, if it has a `.meta`.
    pub guid: Option<String>,
    /// The kind its root names.
    pub kind: Option<String>,
    /// The root's component type paths.
    pub root_components: Vec<String>,
    /// Entities in the prefab.
    pub entities: usize,
    /// Levels instancing it.
    pub levels: Vec<LevelUse>,
}

/// A system reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SystemRef {
    /// Schedule label.
    pub schedule: String,
    /// System name.
    pub system: String,
}

/// One component: the systems that read and write it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentEntry {
    /// Type path.
    pub type_path: String,
    /// Kinds that require it.
    pub required_by: Vec<String>,
    /// Prefabs whose root carries it.
    pub on_prefabs: Vec<String>,
    /// Systems that read it (and do not write it).
    pub readers: Vec<SystemRef>,
    /// Systems that write it.
    pub writers: Vec<SystemRef>,
}

/// A kind and how many entities of a level have it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindCount {
    /// Kind name.
    pub kind: String,
    /// Entities.
    pub count: usize,
}

/// One level.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelEntry {
    /// The file (project-relative).
    pub path: String,
    /// Entity records in the file.
    pub entities: usize,
    /// Kinds used, by name.
    pub kinds: Vec<KindCount>,
    /// Prefabs instanced (paths, or `asset:<guid>` when the prefab was not found).
    pub prefabs: Vec<String>,
    /// Entities with no kind that span two role families.
    pub unclassified: usize,
}

/// A "where to look" row.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct IndexEntry {
    /// The name looked up (kind name, prefab or level file name).
    pub name: String,
    /// `kind`, `prefab` or `level`.
    pub what: String,
    /// Where it is defined (a file, or a marker type path for a code kind).
    pub file: String,
}

/// The project map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMap {
    /// [`MAP_VERSION`].
    pub map_version: u32,
    /// The project's name.
    pub project: String,
    /// `live` when the system access came from a running app, else `offline`.
    pub systems_source: String,
    /// Kinds, by name.
    pub kinds: Vec<KindEntry>,
    /// Prefabs, by path.
    pub prefabs: Vec<PrefabEntry>,
    /// Components kinds require or prefab roots carry, by type path.
    pub components: Vec<ComponentEntry>,
    /// Exclusive systems (they may touch any component).
    pub exclusive_systems: Vec<SystemRef>,
    /// Levels, by path.
    pub levels: Vec<LevelEntry>,
    /// Where each kind, prefab and level is defined, by name.
    pub where_to_look: Vec<IndexEntry>,
    /// Schedules whose systems could not be read.
    pub unread_schedules: Vec<String>,
    /// Files that failed to parse and registry notes.
    pub notes: Vec<String>,
}

/// Builds the map of `files` with the kinds of `catalog` and, when given, the systems of `systems`.
#[must_use]
pub fn build_map(
    project: &str,
    files: &ProjectFiles,
    catalog: &KindCatalog,
    systems: Option<&SystemAccessTable>,
    registry_notes: &[String],
) -> ProjectMap {
    let _span = tracing::info_span!("kind_build_map").entered();
    // Prefabs: kind and root components.
    let mut prefab_kind: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut prefabs: Vec<PrefabEntry> = files
        .prefabs
        .iter()
        .map(|p| {
            let root = prefab_root(&p.file);
            let kind = root.and_then(|r| record_kind(catalog, r)).map(|(name, _)| name);
            prefab_kind.insert(p.path.clone(), kind.clone());
            PrefabEntry {
                path: p.path.clone(),
                guid: p.guid.map(|g| g.to_string()),
                kind,
                root_components: root
                    .map(|r| r.components.keys().cloned().collect())
                    .unwrap_or_default(),
                entities: p.file.entities.len(),
                levels: Vec::new(),
            }
        })
        .collect();
    prefabs.sort_by(|a, b| a.path.cmp(&b.path));

    // Levels: kinds and prefabs used.
    let mut kind_levels: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut prefab_levels: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut levels = Vec::new();
    for level in &files.levels {
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        let mut used_prefabs: BTreeSet<String> = BTreeSet::new();
        let mut unclassified = 0;
        for record in &level.file.entities {
            let mut kind = record_kind(catalog, record).map(|(name, _)| name);
            if let Some(instance) = &record.prefab_instance {
                let guid = instance.prefab.guid();
                let prefab_path = match files.prefab(guid) {
                    Some(prefab) => {
                        if kind.is_none() {
                            kind = prefab_kind.get(&prefab.path).cloned().flatten();
                        }
                        prefab.path.clone()
                    }
                    None => format!("asset:{guid}"),
                };
                *prefab_levels
                    .entry(prefab_path.clone())
                    .or_default()
                    .entry(level.path.clone())
                    .or_default() += 1;
                used_prefabs.insert(prefab_path);
            }
            match kind {
                Some(name) => *kinds.entry(name).or_default() += 1,
                None => {
                    let c = classify(record.components.iter().map(|(path, raw)| ComponentProbe {
                        type_path: path,
                        sensor: ron_says_sensor(raw.get_ron()),
                    }));
                    if c.is_multi_family() {
                        unclassified += 1;
                    }
                }
            }
        }
        for (kind, count) in &kinds {
            *kind_levels
                .entry(kind.clone())
                .or_default()
                .entry(level.path.clone())
                .or_default() += count;
        }
        levels.push(LevelEntry {
            path: level.path.clone(),
            entities: level.file.entities.len(),
            kinds: kinds
                .into_iter()
                .map(|(kind, count)| KindCount { kind, count })
                .collect(),
            prefabs: used_prefabs.into_iter().collect(),
            unclassified,
        });
    }
    levels.sort_by(|a, b| a.path.cmp(&b.path));
    for prefab in &mut prefabs {
        if let Some(uses) = prefab_levels.get(&prefab.path) {
            prefab.levels = uses
                .iter()
                .map(|(level, count)| LevelUse {
                    level: level.clone(),
                    count: *count,
                })
                .collect();
        }
    }

    // Kinds.
    let kinds: Vec<KindEntry> = catalog
        .iter()
        .map(|info| KindEntry {
            name: info.name.clone(),
            icon: info.icon.clone(),
            color: info.color_hex(),
            category: info.category_name().to_string(),
            description: info.description.clone(),
            source: info.source.clone(),
            required: info.required.clone(),
            prefabs: prefabs
                .iter()
                .filter(|p| p.kind.as_deref() == Some(info.name.as_str()))
                .map(|p| p.path.clone())
                .collect(),
            levels: kind_levels
                .get(&info.name)
                .map(|uses| {
                    uses.iter()
                        .map(|(level, count)| LevelUse {
                            level: level.clone(),
                            count: *count,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();

    // Components: required by kinds or on prefab roots.
    let mut component_sources: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    for kind in &kinds {
        for required in &kind.required {
            component_sources
                .entry(required.clone())
                .or_default()
                .0
                .insert(kind.name.clone());
        }
    }
    for prefab in &prefabs {
        for component in &prefab.root_components {
            component_sources
                .entry(component.clone())
                .or_default()
                .1
                .insert(prefab.path.clone());
        }
    }
    let refs = |pairs: Vec<(String, String)>| -> Vec<SystemRef> {
        pairs
            .into_iter()
            .map(|(schedule, system)| SystemRef { schedule, system })
            .collect()
    };
    let components = component_sources
        .into_iter()
        .map(|(type_path, (required_by, on_prefabs))| ComponentEntry {
            readers: systems
                .map(|s| refs(s.readers_of(&type_path)))
                .unwrap_or_default(),
            writers: systems
                .map(|s| refs(s.writers_of(&type_path)))
                .unwrap_or_default(),
            type_path,
            required_by: required_by.into_iter().collect(),
            on_prefabs: on_prefabs.into_iter().collect(),
        })
        .collect();

    // Where to look.
    let mut where_to_look: Vec<IndexEntry> = Vec::new();
    for kind in &kinds {
        let file = match &kind.source {
            KindSource::Code { marker } => marker.clone(),
            KindSource::Data { path, .. } => path.clone(),
        };
        where_to_look.push(IndexEntry {
            name: kind.name.clone(),
            what: "kind".into(),
            file,
        });
    }
    let file_name = |path: &str| path.rsplit('/').next().unwrap_or(path).to_string();
    for prefab in &prefabs {
        where_to_look.push(IndexEntry {
            name: file_name(&prefab.path),
            what: "prefab".into(),
            file: prefab.path.clone(),
        });
    }
    for level in &levels {
        where_to_look.push(IndexEntry {
            name: file_name(&level.path),
            what: "level".into(),
            file: level.path.clone(),
        });
    }
    where_to_look.sort();

    let mut notes: Vec<String> = files
        .errors
        .iter()
        .map(|(path, error)| format!("{path}: {error}"))
        .collect();
    notes.extend(registry_notes.iter().cloned());
    ProjectMap {
        map_version: MAP_VERSION,
        project: project.to_string(),
        systems_source: if systems.is_some() { "live" } else { "offline" }.into(),
        kinds,
        prefabs,
        components,
        exclusive_systems: systems.map(|s| refs(s.exclusive())).unwrap_or_default(),
        levels,
        where_to_look,
        unread_schedules: systems.map(|s| s.unread.clone()).unwrap_or_default(),
        notes,
    }
}

/// Escapes a markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

fn uses(list: &[LevelUse]) -> String {
    if list.is_empty() {
        return "-".into();
    }
    list.iter()
        .map(|u| format!("`{}` ({})", u.level, u.count))
        .collect::<Vec<_>>()
        .join(", ")
}

fn code_list(list: &[String]) -> String {
    if list.is_empty() {
        return "-".into();
    }
    list.iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn system_list(list: &[SystemRef]) -> String {
    if list.is_empty() {
        return "-".into();
    }
    list.iter()
        .map(|s| format!("`{}` ({})", s.system, s.schedule))
        .collect::<Vec<_>>()
        .join(", ")
}

impl ProjectMap {
    /// The JSON text (pretty, trailing newline).
    ///
    /// # Errors
    ///
    /// Serialization failed.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        Ok(text)
    }

    /// The markdown text.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();
        let _ = writeln!(md, "# Project map: {}\n", cell(&self.project));
        let _ = writeln!(
            md,
            "Generated by `regia describe` / `project.describe` (map version {}). Do not edit; regenerate.\n",
            self.map_version
        );
        let _ = writeln!(md, "## Kinds\n");
        if self.kinds.is_empty() {
            let _ = writeln!(md, "No kinds are declared.\n");
        } else {
            let _ = writeln!(
                md,
                "| Kind | Category | Icon | Color | Description | Declared in | Required | Prefabs | Levels |"
            );
            let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|");
            for k in &self.kinds {
                let declared = match &k.source {
                    KindSource::Code { marker } => format!("code `{marker}`"),
                    KindSource::Data { path, .. } => format!("`{path}`"),
                };
                let _ = writeln!(
                    md,
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    cell(&k.name),
                    cell(&k.category),
                    cell(&k.icon),
                    k.color,
                    cell(&k.description),
                    declared,
                    code_list(&k.required),
                    code_list(&k.prefabs),
                    uses(&k.levels)
                );
            }
            md.push('\n');
        }
        let _ = writeln!(md, "## Prefabs\n");
        if self.prefabs.is_empty() {
            let _ = writeln!(md, "No prefabs.\n");
        } else {
            let _ = writeln!(md, "| Prefab | Kind | Root components | Entities | Levels |");
            let _ = writeln!(md, "|---|---|---|---|---|");
            for p in &self.prefabs {
                let _ = writeln!(
                    md,
                    "| `{}` | {} | {} | {} | {} |",
                    p.path,
                    p.kind.as_deref().map_or_else(|| "-".to_string(), cell),
                    code_list(&p.root_components),
                    p.entities,
                    uses(&p.levels)
                );
            }
            md.push('\n');
        }
        let _ = writeln!(md, "## Components and systems\n");
        if self.systems_source != "live" {
            let _ = writeln!(
                md,
                "Systems are not listed: this map was made offline. Run `regia describe --live` with the game running (or the agent method `project.describe`) to add them.\n"
            );
        }
        if self.components.is_empty() {
            let _ = writeln!(
                md,
                "No components are required by a kind or carried by a prefab root.\n"
            );
        } else {
            let _ = writeln!(
                md,
                "| Component | Required by | On prefabs | Read by | Written by |"
            );
            let _ = writeln!(md, "|---|---|---|---|---|");
            for c in &self.components {
                let _ = writeln!(
                    md,
                    "| `{}` | {} | {} | {} | {} |",
                    c.type_path,
                    code_list(&c.required_by),
                    code_list(&c.on_prefabs),
                    system_list(&c.readers),
                    system_list(&c.writers)
                );
            }
            md.push('\n');
        }
        if !self.exclusive_systems.is_empty() {
            let _ = writeln!(
                md,
                "Exclusive systems (may touch any component): {}\n",
                system_list(&self.exclusive_systems)
            );
        }
        if !self.unread_schedules.is_empty() {
            let _ = writeln!(md, "Schedules not read: {}\n", code_list(&self.unread_schedules));
        }
        let _ = writeln!(md, "## Levels\n");
        if self.levels.is_empty() {
            let _ = writeln!(md, "No levels.\n");
        } else {
            let _ = writeln!(md, "| Level | Entities | Kinds | Prefabs | Unclassified |");
            let _ = writeln!(md, "|---|---|---|---|---|");
            for l in &self.levels {
                let kinds = if l.kinds.is_empty() {
                    "-".to_string()
                } else {
                    l.kinds
                        .iter()
                        .map(|k| format!("{} ({})", cell(&k.kind), k.count))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let _ = writeln!(
                    md,
                    "| `{}` | {} | {} | {} | {} |",
                    l.path,
                    l.entities,
                    kinds,
                    code_list(&l.prefabs),
                    l.unclassified
                );
            }
            md.push('\n');
        }
        let _ = writeln!(md, "## Where to look\n");
        let _ = writeln!(md, "| Name | What | Defined in |");
        let _ = writeln!(md, "|---|---|---|");
        for e in &self.where_to_look {
            let _ = writeln!(md, "| {} | {} | `{}` |", cell(&e.name), e.what, e.file);
        }
        if !self.notes.is_empty() {
            let _ = writeln!(md, "\n## Notes\n");
            for note in &self.notes {
                let _ = writeln!(md, "- {}", cell(note));
            }
        }
        md
    }
}

/// Writes `project-map.md` and `project-map.json` into `dir` atomically (temporary file, then
/// rename). Returns the two paths.
///
/// # Errors
///
/// The directory cannot be created or a file cannot be written.
pub fn write_map(dir: &Path, map: &ProjectMap) -> std::io::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let json = map.to_json().map_err(std::io::Error::other)?;
    let md_path = dir.join(MAP_MARKDOWN);
    let json_path = dir.join(MAP_JSON);
    write_atomic(&md_path, map.to_markdown().as_bytes())?;
    write_atomic(&json_path, json.as_bytes())?;
    Ok((md_path, json_path))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}
