// SPDX-License-Identifier: MPL-2.0
//! The cook's import reports (`import_reports.json` in the cooked cache, ADR-3358): one entry
//! per imported Aseprite/LDtk source, written by `regia cook` and read at run time by the agent
//! method `import.report` (the runtime does not link this crate; a test ties the two, as for
//! `assets.font_subset` and `font_subsets.json`). The file is derived data: delete it and the
//! next cook rebuilds it.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The reports file's name in the cooked cache.
pub const IMPORT_REPORTS_FILE: &str = "import_reports.json";
/// `format_version` of [`IMPORT_REPORTS_FILE`] (1 is the first version).
pub const IMPORT_REPORTS_VERSION: u32 = 1;

/// One imported source's report: the importer name, the structured details and the warnings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImportReportEntry {
    /// The importer (`"aseprite"`, `"ldtk"`).
    pub importer: String,
    /// The structured details (`frames`, `tags`, `layers`, `levels`, ... — importer-specific).
    pub data: serde_json::Value,
    /// Every feature dropped or adjusted, one line each.
    pub warnings: Vec<String>,
}

/// The whole reports document, keyed by project-relative source path (`assets/...`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportReports {
    /// The version ([`IMPORT_REPORTS_VERSION`]).
    pub format_version: u32,
    /// Entries keyed by project-relative source path, sorted for a stable file.
    pub entries: BTreeMap<String, ImportReportEntry>,
}

impl ImportReports {
    /// An empty document at the current version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            format_version: IMPORT_REPORTS_VERSION,
            entries: BTreeMap::new(),
        }
    }

    /// Loads the reports from `cache_dir`; a missing or damaged file yields an empty document
    /// (the file is derived data, rebuilt by the next cook).
    #[must_use]
    pub fn load(cache_dir: &Path) -> Self {
        let path = cache_dir.join(IMPORT_REPORTS_FILE);
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::new();
        };
        match serde_json::from_slice::<ImportReports>(&bytes) {
            Ok(doc) if doc.format_version == IMPORT_REPORTS_VERSION => doc,
            _ => Self::new(),
        }
    }

    /// Inserts or replaces one entry and writes the file atomically.
    ///
    /// # Errors
    /// The file cannot be written.
    pub fn upsert_and_save(
        &mut self,
        cache_dir: &Path,
        source_path: &str,
        entry: ImportReportEntry,
    ) -> std::io::Result<()> {
        self.entries.insert(source_path.to_string(), entry);
        self.save(cache_dir)
    }

    /// Writes the document atomically (a temporary file beside the target, then a rename).
    ///
    /// # Errors
    /// The file cannot be written.
    pub fn save(&self, cache_dir: &Path) -> std::io::Result<()> {
        let path = cache_dir.join(IMPORT_REPORTS_FILE);
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        crate::cook_ledger::write_atomic(&path, text.as_bytes())
    }
}

/// Builds the Aseprite entry's `data` value from the import.
pub fn aseprite_report_data(imported: &crate::aseprite_import::ImportedAseprite) -> serde_json::Value {
    use serde_json::json;
    json!({
        "canvas": imported.canvas,
        "frame_count": imported.frames.len(),
        "frames": imported
            .frames
            .iter()
            .enumerate()
            .map(|(i, f)| json!({ "index": i, "duration_ms": f.duration_ms }))
            .collect::<Vec<_>>(),
        "layers": imported
            .layers
            .iter()
            .map(|l| json!({
                "name": l.name,
                "visible": l.visible,
                "tilemap": l.tilemap,
                "reference": l.reference,
                "skipped": l.blend_skipped,
            }))
            .collect::<Vec<_>>(),
        "tags": imported
            .tags
            .iter()
            .map(|t| json!({
                "name": t.name,
                "from": t.from,
                "to": t.to,
                "direction": match t.direction {
                    0 => "forward",
                    1 => "reverse",
                    2 => "ping_pong",
                    _ => "ping_pong_reverse",
                },
                "repeat": t.repeat,
            }))
            .collect::<Vec<_>>(),
        "clips": imported
            .clips
            .iter()
            .map(|c| json!({
                "name": c.name,
                "frames": c.frames.len(),
                "duration_ms_total": c.durations_ms.iter().sum::<u32>(),
                "repeat": c.repeat,
            }))
            .collect::<Vec<_>>(),
        "sprites": imported
            .sprites
            .iter()
            .map(|s| json!({
                "name": s.name,
                "size": [s.image.width, s.image.height],
                "pivot": s.pivot,
                "nine_slice": s.nine_slice.map(|n| [n.left, n.top, n.right, n.bottom]),
            }))
            .collect::<Vec<_>>(),
        "palette_size": imported.palette_size,
    })
}

/// Builds the LDtk entry's `data` value from the import.
pub fn ldtk_report_data(imported: &crate::ldtk_import::ImportedLdtk) -> serde_json::Value {
    use serde_json::json;
    json!({
        "size": [imported.merged.width, imported.merged.height],
        "tile_size": [imported.merged.tile_width, imported.merged.tile_height],
        "tilesets": imported
            .tilesets
            .iter()
            .map(|(name, first_gid, count)| json!({
                "name": name, "first_gid": first_gid, "tile_count": count,
            }))
            .collect::<Vec<_>>(),
        "levels": imported
            .level_infos
            .iter()
            .map(|l| json!({
                "identifier": l.identifier,
                "world_x": l.world_x,
                "world_y": l.world_y,
                "size_px": l.size_px,
                "layers": l.layers,
                "entities": l.entities,
            }))
            .collect::<Vec<_>>(),
    })
}
