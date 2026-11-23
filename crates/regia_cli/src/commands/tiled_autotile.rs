// SPDX-License-Identifier: MPL-2.0
//! `regia tiled-autotile`: import the Wang sets and terrains of a Tiled tileset (`.tsx`, `.tsj`)
//! or map (`.tmx`, `.tmj`) as `.autotile.ron` rules (ADR-2597, `book/user-guide/2d.md`).
//!
//! One `<tileset name>.autotile.ron` is written per tileset that has usable terrains, into
//! `--out-dir` (default: next to the source). The import's warnings (a mask without a tile
//! borrowing its nearest neighbour, a color with no tiles) are printed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use regia_asset_pipeline::tiled_wang::wang_to_autotile_ron;
use serde_json::json;

/// `regia tiled-autotile` arguments.
#[derive(Debug, Args)]
pub struct TiledAutotileArgs {
    /// The Tiled `.tsx` / `.tsj` tileset or `.tmx` / `.tmj` map to read the Wang sets from.
    pub source: PathBuf,
    /// Directory the `.autotile.ron` files are written to (default: the source's directory).
    #[arg(long, value_name = "DIR")]
    pub out_dir: Option<PathBuf>,
    /// Print the result as JSON on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// A file name that is safe on every platform: letters, digits, `-`, `_` and `.` only.
fn safe_file_name(tileset: &str) -> String {
    let name: String = tileset
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() { "tileset".into() } else { name }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

/// Runs `regia tiled-autotile`; returns the files written.
pub fn run(args: &TiledAutotileArgs) -> Result<Vec<PathBuf>> {
    let imported = wang_to_autotile_ron(&args.source).map_err(|e| anyhow!("{e}"))?;
    if imported.is_empty() {
        bail!(
            "{} has no Wang sets or terrains with usable tiles",
            args.source.display()
        );
    }
    let dir = args
        .out_dir
        .clone()
        .or_else(|| args.source.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut written = Vec::new();
    let mut report = Vec::new();
    for (tileset, text, warnings) in imported {
        let path = dir.join(format!("{}.autotile.ron", safe_file_name(&tileset)));
        write_atomic(&path, text.as_bytes())?;
        if !args.json {
            println!("{} ({tileset})", path.display());
            for warning in &warnings {
                println!("  warning: {warning}");
            }
        }
        report.push(json!({ "tileset": tileset, "path": path.display().to_string(), "warnings": warnings }));
        written.push(path);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&json!({ "written": report }))?);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../regia_asset_pipeline/tests/fixtures/tiled")
    }

    #[test]
    fn writes_one_autotile_ron_per_tileset_that_parses_back() {
        let dir = regia_test_utils::unique_temp_dir("tiled-autotile");
        let written = run(&TiledAutotileArgs {
            source: fixtures().join("wang_corner.tsx"),
            out_dir: Some(dir.clone()),
            json: true,
        })
        .expect("imports");
        assert_eq!(written, vec![dir.join("wangcorner.autotile.ron")]);
        let text = std::fs::read_to_string(&written[0]).expect("read");
        let set = regia::two_d::AutoTileSet::from_ron(&text).expect("parses");
        assert_eq!(set.tileset, "wangcorner");
        assert!(!dir.join("wangcorner.autotile.ron.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_source_without_wang_sets_is_an_error() {
        let dir = regia_test_utils::unique_temp_dir("tiled-autotile-none");
        let err = run(&TiledAutotileArgs {
            source: fixtures().join("sample.tmx"),
            out_dir: Some(dir.clone()),
            json: false,
        })
        .expect_err("no wang sets");
        assert!(err.to_string().contains("no Wang sets"), "{err}");
        assert!(
            run(&TiledAutotileArgs {
                source: fixtures().join("tileset.png"),
                out_dir: None,
                json: false
            })
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_names_are_sanitized() {
        assert_eq!(safe_file_name("../evil name"), "___evil_name");
        assert_eq!(safe_file_name(""), "tileset");
    }
}
