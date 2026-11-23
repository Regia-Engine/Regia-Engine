// SPDX-License-Identifier: MPL-2.0
//! `manual`: build the Regia Engine Manual from `book/`.
//!
//! 1. **Generated reference pages** — `book/reference/agent-methods.md` (from
//!    [`regia_agent::schema::METHODS`] and the `book/COVERAGE.md` page map),
//!    `book/reference/formats.md` (from the frozen-format registry `check-formats` uses) and
//!    `book/reference/render-profiles.md` (from [`regia_render::RenderProfile::from_preset`] and
//!    the post/fog defaults). They are rewritten from the code, so they cannot drift from it.
//! 2. **`docs/manual/manual.html`** — every chapter of `book/SUMMARY.md` in one self-contained HTML
//!    file: sidebar table of contents, a search box, AMOLED Night / Paper theme, highlighted code,
//!    embedded PiercingXX fonts, no network requests. Markdown goes through the in-house renderer
//!    ([`crate::markdown`]); links between pages become in-file anchors; code is highlighted here
//!    (no script library). The output depends only on the book, so two runs on the same tree write
//!    identical bytes. `book/theme/piercing.css` is the same stylesheet, for the mdBook site.
//! 3. **The mdBook site** — `mdbook build book` (output `book/build/`) when `mdbook` is on `PATH`.
//!
//! `--check` writes nothing and fails when a generated page or `manual.html` is out of date.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

/// The manual's output path relative to the workspace root.
pub const MANUAL_HTML: &str = "docs/manual/manual.html";

/// The generated reference pages, relative to `book/`.
pub const GENERATED_PAGES: [&str; 3] = [
    "reference/agent-methods.md",
    "reference/formats.md",
    "reference/render-profiles.md",
];

/// What `cargo xtask manual` should do.
#[derive(Debug, Clone, Copy, Default)]
pub struct ManualOptions {
    /// Verify instead of writing.
    pub check: bool,
    /// Skip `mdbook build` even when `mdbook` is available.
    pub no_mdbook: bool,
}

/// Runs the command: regenerate the reference pages, render the HTML manual, build the mdBook site.
pub fn run(root: &Path, options: ManualOptions) -> Result<ExitCode> {
    let book = root.join(crate::book::BOOK_DIR);
    let mut stale = Vec::new();

    for (rel, text) in generated_pages(&book)? {
        let path = book.join(&rel);
        if write_or_check(&path, &text, options.check)? {
            stale.push(format!("book/{rel}"));
        }
    }

    let css = theme_css(&book)?;
    if write_or_check(&book.join("theme/piercing.css"), &css, options.check)? {
        stale.push("book/theme/piercing.css".to_string());
    }

    let html = render_manual(&book)?;
    let out = root.join(MANUAL_HTML);
    if write_or_check(&out, &html, options.check)? {
        stale.push(MANUAL_HTML.to_string());
    }

    if options.check {
        if stale.is_empty() {
            println!("manual: up to date");
            return Ok(ExitCode::SUCCESS);
        }
        for path in &stale {
            eprintln!("error: {path} is out of date; run `cargo xtask manual`");
        }
        return Ok(ExitCode::FAILURE);
    }
    for path in &stale {
        println!("manual: wrote {path}");
    }
    println!("manual: {} ({} KiB)", MANUAL_HTML, html.len() / 1024);

    if !options.no_mdbook {
        match std::process::Command::new("mdbook")
            .arg("build")
            .arg(&book)
            .status()
        {
            Ok(status) if status.success() => println!("manual: mdBook site in book/build/"),
            Ok(status) => {
                eprintln!("error: `mdbook build book` failed ({status})");
                return Ok(ExitCode::FAILURE);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("manual: `mdbook` not on PATH, multi-page site skipped (`cargo install mdbook`)");
            }
            Err(e) => return Err(e).context("running mdbook"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Writes `text` to `path` when it differs. In check mode only compares. Returns whether the file
/// was (or, in check mode, would be) changed.
fn write_or_check(path: &Path, text: &str, check: bool) -> Result<bool> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current == text {
        return Ok(false);
    }
    if !check {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(true)
}

/// Latin woff2 subsets under `book/theme/fonts`, both OFL-1.1.
const FONT_SPACE_400: &str = "theme/fonts/SpaceMono-400-latin.woff2";
const FONT_SPACE_700: &str = "theme/fonts/SpaceMono-700-latin.woff2";
const FONT_JETBRAINS: &str = "theme/fonts/JetBrainsMono-latin.woff2";

/// Stylesheet source. `cargo xtask manual` writes the substituted result to `theme/piercing.css`.
const BRAND_CSS: &str = "theme/brand.css";

/// The underlined-XX mark, inlined into the manual. Strokes stay Signal on an Ink tile.
const LOGOMARK: &str = "theme/logomark.svg";

/// Encodes `data` as standard base64 (no newlines).
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = (u32::from(data[i]) << 16) | (u32::from(data[i + 1]) << 8) | u32::from(data[i + 2]);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    let rest = data.len() - i;
    if rest == 1 {
        let n = u32::from(data[i]) << 16;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let n = (u32::from(data[i]) << 16) | (u32::from(data[i + 1]) << 8);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

/// `data:font/woff2;base64,...` for a font file under `book/`.
fn font_data_uri(book: &Path, rel: &str) -> Result<String> {
    let path = book.join(rel);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() < 4 || &bytes[..4] != b"wOF2" {
        bail!("{} is not a woff2 font", path.display());
    }
    Ok(format!("data:font/woff2;base64,{}", base64_encode(&bytes)))
}

/// The manual stylesheet with the three font files embedded. No network URLs.
fn theme_css(book: &Path) -> Result<String> {
    let path = book.join(BRAND_CSS);
    let template = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let css = template
        .replace("__SPACE400__", &font_data_uri(book, FONT_SPACE_400)?)
        .replace("__SPACE700__", &font_data_uri(book, FONT_SPACE_700)?)
        .replace("__JETBRAINS__", &font_data_uri(book, FONT_JETBRAINS)?);
    if css.contains("__SPACE") || css.contains("__JETBRAINS__") {
        bail!("brand.css still has a font placeholder");
    }
    if css.contains("url(http") || css.contains("@import") {
        bail!("brand.css requests the network");
    }
    Ok(css)
}

/// The `<svg>…</svg>` mark, without a leading XML declaration.
fn logomark_svg(book: &Path) -> Result<String> {
    let path = book.join(LOGOMARK);
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let Some(start) = raw.find("<svg") else {
        bail!("{} has no <svg> element", path.display());
    };
    Ok(raw[start..].trim().to_string())
}

/// Tags blockquotes as note / warning / planned callouts, and marks the word `planned`.
fn tag_callouts(html: String) -> String {
    const OPEN: &str = "<blockquote>";
    const CLOSE: &str = "</blockquote>";
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html.as_str();
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        let after = &rest[i + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else {
            out.push_str(&rest[i..]);
            return finish_callouts(out);
        };
        let class = quote_class(&after[..end]);
        out.push_str("<blockquote class=\"");
        out.push_str(class);
        out.push_str("\">");
        out.push_str(&after[..end]);
        rest = &after[end..];
    }
    out.push_str(rest);
    finish_callouts(out)
}

fn finish_callouts(html: String) -> String {
    html.replace(
        "<strong>planned</strong>",
        "<strong class=\"status planned\">planned</strong>",
    )
}

fn quote_class(inner: &str) -> &'static str {
    let lower = inner.to_ascii_lowercase();
    if lower.contains("planned") {
        "callout planned"
    } else if lower.contains("warning") || lower.contains("caution") {
        "callout warn"
    } else {
        "callout note"
    }
}

/// The generated pages that differ from what is on disk (for `book-check`'s staleness warning).
pub fn stale_generated_pages(book: &Path) -> Result<Vec<String>> {
    let mut stale = Vec::new();
    for (rel, text) in generated_pages(book)? {
        if std::fs::read_to_string(book.join(&rel)).unwrap_or_default() != text {
            stale.push(rel);
        }
    }
    Ok(stale)
}

// ---------------------------------------------------------------------------
// Generated reference pages
// ---------------------------------------------------------------------------

/// Every generated page as `(path relative to book/, markdown)`.
pub fn generated_pages(book: &Path) -> Result<Vec<(String, String)>> {
    let coverage = std::fs::read_to_string(book.join(crate::book::COVERAGE)).unwrap_or_default();
    Ok(vec![
        (GENERATED_PAGES[0].to_string(), agent_methods_page(&coverage)),
        (GENERATED_PAGES[1].to_string(), formats_page()),
        (GENERATED_PAGES[2].to_string(), render_profiles_page()),
    ])
}

/// The banner every generated page starts with.
fn generated_banner(source: &str) -> String {
    format!("<!-- Generated by `cargo xtask manual` from {source}. Do not edit by hand. -->\n\n")
}

/// Escapes a markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// `book/reference/agent-methods.md`: every protocol method, grouped by area.
fn agent_methods_page(coverage: &str) -> String {
    let (rows, _) = crate::book::coverage_table(coverage);
    let pages: BTreeMap<&str, &str> = rows
        .iter()
        .map(|r| (r.method.as_str(), r.page.as_str()))
        .collect();
    let methods = regia_agent::schema::METHODS;

    let mut out = generated_banner("`regia_agent::schema::METHODS` and `book/COVERAGE.md`");
    out.push_str("# Agent methods\n\n");
    let _ = writeln!(
        out,
        "Every method of the agent protocol ({} methods), grouped by area, in the order the schema \
         lists them. `agent.schema` returns the same table at runtime, and `regia mcp` exposes \
         each method that has an MCP tool name as that tool. *Mutates* methods are rate-limited \
         and need the permission shown in `.regia/agent.toml` (see [Safety](../agents/safety.md)). \
         Parameters marked `*` are required. The *Guide* column links the agent-guide page that \
         documents the method with examples.\n",
        methods.len()
    );
    let _ = writeln!(
        out,
        "Each area's second table is the method contract (ADR-1034): a typical call's params, the \
         error codes the method can answer with besides the universal ones ({}), the limits the \
         dispatcher checks before the handler runs, and the cost. Array and string params without \
         their own limit hold at most {} items or {} bytes. *Heavy* calls also count against \
         `heavy_rate_limit_per_second`. [Errors](../agents/protocol.md#errors) explains every code.\n",
        regia_agent::contract::UNIVERSAL_ERRORS
            .iter()
            .map(|code| format!("`{code}`"))
            .collect::<Vec<_>>()
            .join(", "),
        regia_agent::contract::DEFAULT_MAX_ITEMS,
        regia_agent::contract::DEFAULT_MAX_BYTES
    );

    let mut areas: Vec<&str> = Vec::new();
    for m in methods {
        let area = m.name.split('.').next().unwrap_or(m.name);
        if !areas.contains(&area) {
            areas.push(area);
        }
    }
    for area in areas {
        let _ = writeln!(out, "## `{area}.*`\n");
        out.push_str("| Method | MCP tool | Permission | Mutates | Parameters | Description | Guide |\n");
        out.push_str("|---|---|---|---|---|---|---|\n");
        for m in methods.iter().filter(|m| m.name.split('.').next() == Some(area)) {
            let tool = if m.tool.is_empty() {
                "—".to_string()
            } else {
                format!("`{}`", m.tool)
            };
            let params = if m.params.is_empty() {
                "—".to_string()
            } else {
                m.params
                    .iter()
                    .map(|p| format!("`{}`{} ({})", p.name, if p.required { "*" } else { "" }, p.kind))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let guide = pages.get(m.name).map_or_else(
                || "—".to_string(),
                |page| {
                    let label = Path::new(page)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(page);
                    format!("[{label}](../{page})")
                },
            );
            let _ = writeln!(
                out,
                "| `{}` | {} | `{}` | {} | {} | {} | {} |",
                m.name,
                tool,
                m.permission.name(),
                if m.mutating { "yes" } else { "no" },
                cell(&params),
                cell(m.description),
                guide
            );
        }
        out.push('\n');
        out.push_str("| Method | Example params | Error codes | Limits | Cost |\n");
        out.push_str("|---|---|---|---|---|\n");
        for m in methods.iter().filter(|m| m.name.split('.').next() == Some(area)) {
            let Some(contract) = regia_agent::contract::contract(m.name) else {
                let _ = writeln!(out, "| `{}` | — | — | — | — |", m.name);
                continue;
            };
            let example = contract.example_json().to_string();
            let errors = if contract.errors.is_empty() {
                "—".to_string()
            } else {
                contract
                    .errors
                    .iter()
                    .map(|code| format!("`{code}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let limits = if contract.limits.is_empty() {
                "—".to_string()
            } else {
                contract
                    .limits
                    .iter()
                    .map(regia_agent::contract::Limit::describe)
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            let _ = writeln!(
                out,
                "| `{}` | `{}` | {} | {} | {} |",
                m.name,
                cell(&example),
                errors,
                cell(&limits),
                contract.cost.name()
            );
        }
        out.push('\n');
    }
    out
}

/// File pattern, owning crate and specification of a frozen format (by its registry name).
fn format_facts(name: &str) -> (&'static str, &'static str, &'static str) {
    match name {
        "meta" => ("`<asset>.meta`", "regia_asset", "`docs/formats/meta.schema.json`"),
        "material" => (
            "`.material.ron`",
            "regia_asset",
            "`docs/formats/material.ron.schema.json`, [Materials](../user-guide/materials.md)",
        ),
        "scene" => (
            "`.level.ron`, `.prefab.ron`",
            "regia_scene",
            "`docs/formats/scene-files.md`, [Scenes](../user-guide/scenes.md)",
        ),
        "ui" => (
            "`.ui.ron`",
            "regia_ui",
            "`docs/formats/ui-ron.md`, [UI](../user-guide/ui.md)",
        ),
        "uisettings" => (
            "`.uisettings.ron`",
            "regia_ui",
            "`docs/formats/uisettings-ron.md`, [UI](../user-guide/ui.md)",
        ),
        "theme" => (
            "`.theme.ron`",
            "regia_ui",
            "`docs/formats/theme-ron.md`, [UI](../user-guide/ui.md)",
        ),
        "vfx" => (
            "`.vfx.ron`",
            "regia_vfx",
            "`docs/formats/vfx-ron.md`, [VFX and decals](../systems/vfx.md)",
        ),
        "animgraph" => (
            "`.animgraph.ron`",
            "regia_animation",
            "`docs/formats/animgraph.schema.json`, [Animation](../user-guide/animation.md)",
        ),
        "montage" => (
            "`.montage.ron`",
            "regia_animation",
            "`docs/formats/montage-ron.md`, [Montages](../user-guide/montages.md)",
        ),
        "sync" => (
            "`.sync.ron`",
            "regia_animation",
            "`docs/formats/sync-ron.md`, [Montages](../user-guide/montages.md)",
        ),
        "montage-variant" => (
            "`.montagevar.ron`",
            "regia_animation",
            "`docs/formats/montage-variant-ron.md`, [Montages](../user-guide/montages.md)",
        ),
        "slots" => (
            "`.slots.ron`",
            "regia_animation",
            "`docs/formats/slots-ron.md`, [Montages](../user-guide/montages.md)",
        ),
        "render-job" => (
            "`.render_job.ron`",
            "regia_agent",
            "`docs/formats/render-job-ron.md`, [Render jobs](../agents/render-jobs.md)",
        ),
        "shot" => (
            "`.shot.ron`",
            "regia_agent",
            "`docs/formats/shot-ron.md`, [Plates and the reference overlay](../agents/plates.md)",
        ),
        "pgraph" => (
            "`.pgraph.ron`",
            "regia_procgen",
            "[Procedural generators](../agents/procgen.md)",
        ),
        "pgvar" => (
            "`.pgvar.ron`",
            "regia_procgen",
            "[Procedural generators](../agents/procgen.md)",
        ),
        "pgout" => (
            "`.pgout`",
            "regia_procgen",
            "[Procedural generators](../agents/procgen.md#bake-and-verify)",
        ),
        "procgen-instanced-set" => (
            "`InstancedMeshSet` in `.level.ron`",
            "regia_procgen",
            "[Procedural generators](../agents/procgen.md#detach)",
        ),
        "procgen-anchor" => (
            "`ProcGenAnchor` in `.level.ron`",
            "regia_procgen",
            "[Procedural generators](../agents/procgen.md#tiles-and-anchors)",
        ),
        "procgen-generator" => (
            "`ProcGenerator` in `.level.ron`",
            "regia_procgen",
            "[Procedural generators](../agents/procgen.md)",
        ),
        "procgen-painted-points" => (
            "`PaintedPoints` in `.level.ron`",
            "regia_procgen",
            "[Procedural graph editor](../agents/procgen-editor.md#placement-tools)",
        ),
        "luamod" => (
            "`luamod.ron`",
            "regia_script_lua",
            "[Lua mods](../agents/lua-mods.md#a-mod)",
        ),
        "luastate" => (
            "the section `regia_script_lua::LuaState` in a save file",
            "regia_script_lua",
            "`docs/formats/save-format.md`, [Lua mods](../agents/lua-mods.md#state-in-save-games)",
        ),
        "functional-test" => (
            "`FunctionalTest` component in `.level.ron`",
            "regia_framework",
            "`docs/formats/functional-test.md`, [Functional tests](../agents/functional-tests.md)",
        ),
        "functest-script" => (
            "`.functest.ron`",
            "regia_framework",
            "`docs/formats/functest-ron.md`, [Functional tests](../agents/functional-tests.md)",
        ),
        "test-report" => (
            "`TestReport` JSON",
            "regia_framework",
            "`docs/formats/test-report.md`, [Functional tests](../agents/functional-tests.md)",
        ),
        "expected-values" => (
            "`<level>.expected.ron`",
            "regia_framework",
            "`docs/formats/expected-ron.md`, [Functional tests](../agents/functional-tests.md)",
        ),
        "biome" => (
            "`.biome.ron`",
            "regia_procgen",
            "`docs/formats/biome-ron.md`, [Biomes](../agents/biomes.md)",
        ),
        "biome-area" => (
            "`BiomeArea` in `.level.ron`",
            "regia_procgen",
            "[Biomes](../agents/biomes.md)",
        ),
        "plants-index" => (
            "`<name>.plants.ron`",
            "regia_procgen",
            "[Procedural generators](../agents/procgen.md#plants)",
        ),
        "cue" => (
            "`.cue.ron`",
            "regia_audio",
            "`docs/formats/audio-cue-v1.md`, [Audio](../user-guide/audio.md)",
        ),
        "music" => (
            "`.music.ron`",
            "regia_audio",
            "`docs/formats/music-ron.md`, [Audio](../user-guide/audio.md)",
        ),
        "audiopatch" => (
            "`.audiopatch.ron`",
            "regia_audio",
            "`docs/formats/audio-patch-v1.md`, [Audio](../user-guide/audio.md)",
        ),
        "loopgen" => (
            "`.loopgen.ron`",
            "regia_audio",
            "`docs/formats/loopgen-ron.md`, [Audio](../user-guide/audio.md)",
        ),
        "loudness-table" => (
            "`loudness.ron`",
            "regia_audio",
            "`docs/formats/loudness-ron.md`, [Loudness](../user-guide/loudness.md)",
        ),
        "terrain-layers" => (
            "`.terrain.layers.ron`",
            "regia_terrain",
            "`docs/formats/terrain.md`, [Photoreal terrain](../user-guide/terrain-photoreal.md)",
        ),
        "terrain-holes" => (
            "`<map>.holes.ron`",
            "regia_terrain",
            "`docs/formats/terrain.md`, [Tunnels and terrain holes](../agents/tunnels.md)",
        ),
        "terrain-editlayers" => (
            "`<map>.terrain.editlayers`",
            "regia_terrain",
            "`docs/formats/terrain-editlayers.md`, [Terrain editing](../agents/terrain-editing.md)",
        ),
        "scatter" => (
            "`.scatter.ron`",
            "regia_terrain",
            "`docs/formats/terrain.md`, [Terrain scatter](../agents/terrain-scatter.md)",
        ),
        "scatter-paint" => (
            "`<map>.scatterpaint.bin`",
            "regia_terrain",
            "`docs/formats/terrain.md`, [Painted scatter](../agents/terrain-scatter-paint.md)",
        ),
        "matgraph" => (
            "`.matgraph.ron`",
            "regia_matgraph",
            "`docs/formats/matgraph-ron.md`, [Material graphs](../systems/material-graph.md)",
        ),
        "matinst" => (
            "`.matinst.ron`",
            "regia_matgraph",
            "`docs/formats/matgraph-ron.md`, [Material graphs](../systems/material-graph.md)",
        ),
        "matfunc" => (
            "`.matfunc.ron`",
            "regia_matgraph",
            "`docs/formats/matgraph-ron.md`, [Material graphs](../systems/material-graph.md)",
        ),
        "save" => ("save slots", "regia_framework", "`docs/formats/save-format.md`"),
        "pak" => (
            "`.fpak`",
            "regia_asset",
            "`docs/formats/pak.md`, [Packaging](../user-guide/packaging.md)",
        ),
        "cooked-mesh" => (
            "cooked `.fckd` meshes",
            "regia_asset",
            "`docs/formats/cooked-mesh.md`",
        ),
        "bake" => (
            "`.bake.ron`",
            "regia_pt",
            "`docs/formats/bake-ron.md`, [Baking GI from meshes](../agents/baked-gi-bake.md)",
        ),
        "spatialquery" => (
            "`.spatialquery.ron`",
            "regia_ai",
            "[Spatial queries](../agents/spatial-queries.md)",
        ),
        "motiondb" => (
            "`.motiondb.ron`",
            "regia_animation",
            "`docs/formats/motiondb-ron.md`, [Motion matching](../user-guide/motion-matching.md)",
        ),
        "weather" => (
            "`config/weather.ron`",
            "regia_vfx",
            "`docs/formats/weather-ron.md`, `docs/formats/weather.ron.schema.json`, [Rain, snow and wet surfaces](../agents/weather-precipitation.md)",
        ),
        "server-players" => (
            "the section `regia_framework::server_persistence::players` in a dedicated server's world save",
            "regia_framework",
            "`docs/formats/server-players.md`, [Server world persistence](../agents/net-server-persistence.md)",
        ),
        "match-result" => (
            "finished-match records `<root>/<utc-date>/<match_id>.json` a host writes",
            "regia_framework",
            "`docs/formats/match-result.md`, [Match results](../agents/match-results.md)",
        ),

        "viewer-package" => (
            "product viewer packages (`manifest.json`, `mesh.rvm`)",
            "regia_viewer_pack",
            "`docs/formats/viewer-package.md`, [Product viewer packages](../agents/viewer-pack.md)",
        ),
        // The editor's own files (ADR-7902).
        "editor-keymap" => (
            "`editor_keymap.ron` (user config)",
            "regia_editor",
            "`docs/formats/editor-keymap-ron.md`",
        ),
        "editor-layout" => (
            "`editor_layout.ron` (user config), `.regia/editor_layout.ron`, `layouts/*.editor_layout.ron`",
            "regia_editor",
            "`docs/formats/editor-layout-ron.md`",
        ),
        "editor-prefs" => (
            "`editor_prefs.ron` (user config)",
            "regia_editor",
            "`docs/formats/editor-prefs-ron.md`",
        ),
        "editor-panel" => (
            "`assets/**/*.editorpanel.ron`",
            "regia_editor",
            "`docs/formats/editor-panel-ron.md`",
        ),
        "editor-profile" => (
            "`.regia/editor_profile.ron`, `profiles/*.editor_profile.ron`",
            "regia_editor",
            "`docs/formats/editor-profile-ron.md`",
        ),
        "editor-theme" => (
            "`themes/*.editor_theme.ron` (user config)",
            "regia_editor",
            "`docs/formats/editor-theme-ron.md`",
        ),
        "editor-view-state" => (
            "`.regia/editor_view.ron`",
            "regia_editor",
            "`docs/formats/editor-view-state-ron.md`",
        ),
        "editor-window-state" => (
            "`editor_window_state.ron` (user config)",
            "regia_editor",
            "`docs/formats/editor-window-state-ron.md`",
        ),
        "inspector-pins" => (
            "`inspector_pins.ron` (user config)",
            "regia_editor",
            "`docs/formats/inspector-pins-ron.md`",
        ),
        "saved-searches" => (
            "`saved_searches.ron` (user config), `config/saved_searches.ron`",
            "regia_editor",
            "`docs/formats/saved-searches-ron.md`",
        ),
        "viewport-snap" => (
            "`.regia/viewport_snap.ron`",
            "regia_editor",
            "`docs/formats/viewport-snap-ron.md`",
        ),
        _ => ("—", "—", "—"),
    }
}

/// `book/reference/formats.md`: every frozen content format with its version and migrators.
fn formats_page() -> String {
    let mut out = generated_banner("the `check-formats` registry (`xtask/src/check_formats.rs`)");
    out.push_str("# Content formats\n\n");
    out.push_str(
        "Regia Engine's content formats are **frozen**: a format's serde shape may only change together \
         with a version bump and a registered migrator, and `cargo xtask check-formats` fails the \
         merge gate otherwise (see [Changing a content format](../changing-a-content-format.md)). \
         Every loader accepts every older version and upgrades it in memory. \
         `regia upgrade-project --write` rewrites the files of every format with a registered \
         migrator (material, level, prefab, terrain-layer, scatter, UI, theme, VFX and the others \
         in the table below) and cooked meshes in the dev cache on disk (keeping a `.bak` of each \
         original).\n\n",
    );
    out.push_str("| Format | Files | Crate | Current version | Migrators | Specification |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    let mut formats = crate::check_formats::live_formats();
    formats.sort_by(|a, b| a.name.cmp(b.name));
    for f in &formats {
        let (files, krate, spec) = format_facts(f.name);
        let mut sources = f.migration_sources.clone();
        sources.sort_unstable();
        sources.dedup();
        let migrators = if sources.is_empty() {
            "—".to_string()
        } else {
            sources
                .iter()
                .map(|v| format!("v{v} → v{}", v + 1))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(
            out,
            "| `{}` | {} | `{}` | {} | {} | {} |",
            f.name, files, krate, f.version, migrators, spec
        );
    }
    out.push_str(
        "\nThe schema hashes the gate compares are in `docs/formats/format-hashes.ron`. A `.meta` \
         sidecar records the format versions an asset was imported with, so the cook re-imports \
         when a version moves.\n",
    );
    out
}

/// One row of the render-profile table: a field name, its description and the value per preset.
fn profile_rows() -> Vec<(&'static str, &'static str, Vec<String>)> {
    use regia_render::{RenderProfile, RenderProfilePreset as P};
    let presets = [
        P::MobileLow,
        P::MobileHigh,
        P::DesktopLow,
        P::DesktopHigh,
        P::DesktopUltra,
        P::Deck,
    ];
    let profiles: Vec<RenderProfile> = presets.iter().map(|p| RenderProfile::from_preset(*p)).collect();
    let col = |f: &dyn Fn(&RenderProfile) -> String| profiles.iter().map(f).collect::<Vec<_>>();
    vec![
        (
            "hdr_target",
            "HDR colour target format.",
            col(&|p| format!("{:?}", p.hdr_target)),
        ),
        (
            "render_scale",
            "Internal resolution relative to the output (TAAU upsamples).",
            col(&|p| format!("{}", p.render_scale)),
        ),
        (
            "anti_aliasing",
            "Anti-aliasing mode.",
            col(&|p| format!("{:?}", p.anti_aliasing)),
        ),
        (
            "depth_prepass",
            "Depth prepass before the forward pass.",
            col(&|p| format!("{}", p.depth_prepass)),
        ),
        (
            "csm_cascade_count",
            "Sun shadow cascades.",
            col(&|p| format!("{}", p.csm_cascade_count)),
        ),
        (
            "csm_resolution",
            "Shadow map edge per cascade (px).",
            col(&|p| format!("{}", p.csm_resolution)),
        ),
        (
            "local_shadow_atlas_size",
            "Local-light shadow atlas edge (0 = off).",
            col(&|p| format!("{}", p.local_shadow_atlas_size)),
        ),
        (
            "local_shadow_max_lights",
            "Local lights with shadows in the atlas.",
            col(&|p| format!("{}", p.local_shadow_max_lights)),
        ),
        (
            "shadow_filter",
            "Shadow filter kernel.",
            col(&|p| format!("{:?}", p.shadow_filter)),
        ),
        (
            "ssao",
            "Ambient occlusion mode.",
            col(&|p| format!("{:?}", p.ssao)),
        ),
        ("bloom", "Bloom.", col(&|p| format!("{}", p.bloom))),
        (
            "bloom_mip_count",
            "Bloom mips.",
            col(&|p| format!("{}", p.bloom_mip_count)),
        ),
        (
            "auto_exposure",
            "Histogram auto exposure.",
            col(&|p| format!("{}", p.auto_exposure)),
        ),
        (
            "max_reflection_probes",
            "Blended reflection probes per cluster.",
            col(&|p| format!("{}", p.max_reflection_probes)),
        ),
        (
            "max_lights_per_cluster",
            "Point/spot lights per froxel cluster.",
            col(&|p| format!("{}", p.max_lights_per_cluster)),
        ),
        (
            "max_visible_lights",
            "Point/spot lights uploaded per view (ranked; drops are logged).",
            col(&|p| format!("{}", p.max_visible_lights)),
        ),
        (
            "frame_cap_fps",
            "Frame cap the profile installs.",
            col(&|p| {
                p.frame_cap_fps
                    .map_or_else(|| "none".to_string(), |f| f.to_string())
            }),
        ),
        (
            "gpu_driven",
            "GPU frustum + Hi-Z culling, multi-draw-indirect.",
            col(&|p| format!("{}", p.gpu_driven)),
        ),
        (
            "horizontal_plus",
            "Horizontal-plus FOV for non-16:9 aspects.",
            col(&|p| format!("{}", p.horizontal_plus)),
        ),
        (
            "physical_atmosphere",
            "Physically based atmosphere LUTs.",
            col(&|p| format!("{}", p.physical_atmosphere)),
        ),
        (
            "volumetric_fog",
            "Froxel volumetric fog.",
            col(&|p| format!("{}", p.volumetric_fog)),
        ),
        (
            "volumetric_clouds",
            "Raymarched volumetric clouds.",
            col(&|p| format!("{}", p.volumetric_clouds)),
        ),
        (
            "paged_sun_shadows",
            "Cached paged sun shadow maps.",
            col(&|p| format!("{}", p.paged_sun_shadows)),
        ),
        (
            "translucent_shadow_cascades",
            "Sun cascades with a translucency map (glass casts tinted shadows).",
            col(&|p| format!("{}", p.translucent_shadow_cascades)),
        ),
        (
            "translucent_local_shadows",
            "Local-light shadows see translucent casters.",
            col(&|p| format!("{}", p.translucent_local_shadows)),
        ),
        ("gi", "Diffuse GI tier.", col(&|p| format!("{:?}", p.gi))),
        (
            "screen_space_gi",
            "Screen-space colour bleeding.",
            col(&|p| format!("{}", p.screen_space_gi)),
        ),
        ("ssr", "Screen-space reflections.", col(&|p| format!("{}", p.ssr))),
        (
            "reflection_probes",
            "Captured reflection probes.",
            col(&|p| format!("{}", p.reflection_probes)),
        ),
        (
            "parallax_occlusion",
            "Parallax occlusion mapping (material scale must be set too).",
            col(&|p| format!("{}", p.parallax_occlusion)),
        ),
        (
            "terrain_budget",
            "Opt-in cheaper terrain shading (off in every preset).",
            col(&|p| {
                if p.terrain_budget.is_off() {
                    "off".to_string()
                } else {
                    "on".to_string()
                }
            }),
        ),
        (
            "depth_velocity",
            "Static TAAU motion reconstructed from depth.",
            col(&|p| format!("{}", p.depth_velocity)),
        ),
    ]
}

/// `book/reference/render-profiles.md`: the preset table and the post/fog defaults.
fn render_profiles_page() -> String {
    let mut out =
        generated_banner("`regia_render::RenderProfile::from_preset`, `PostSettings` and `FogSettings`");
    out.push_str("# Render profiles and settings\n\n");
    out.push_str(
        "`RenderSettings` (a resource) holds the active `RenderProfile`, the `PostSettings` and the \
         `FogSettings`. `RenderSettings::from_profile(preset)` builds it from a \
         `RenderProfilePreset`; `RenderSettings::default()` is `RenderProfilePreset::runtime_default` \
         (Deck on a Steam Deck, Mobile High on Android, Desktop High elsewhere). The profile decides \
         *what runs*; the post settings hold the user-facing values, and \
         `RenderSettings::resolve_post` applies the profile's bloom/AA/auto-exposure switches over \
         them. See [Rendering](../user-guide/rendering.md) and \
         [The default look](../user-guide/default-look.md).\n\n",
    );
    out.push_str("## Presets\n\n");
    out.push_str(
        "| Field | Meaning | MobileLow | MobileHigh | DesktopLow | DesktopHigh | DesktopUltra | Deck |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for (field, meaning, values) in profile_rows() {
        let _ = writeln!(out, "| `{field}` | {meaning} | {} |", values.join(" | "));
    }

    let post =
        regia_render::RenderSettings::from_profile(regia_render::RenderProfilePreset::DesktopHigh).post;
    out.push_str("\n## Post settings (Desktop High defaults)\n\n");
    out.push_str("| Field | Default | Meaning |\n|---|---|---|\n");
    let rows: Vec<(&str, String, &str)> = vec![
        (
            "enabled",
            post.enabled.to_string(),
            "Master switch for the post stack.",
        ),
        (
            "bloom",
            post.bloom.to_string(),
            "Bloom (the profile can turn it off).",
        ),
        ("bloom_mips", post.bloom_mips.to_string(), "Bloom mip count."),
        (
            "bloom_intensity",
            post.bloom_intensity.to_string(),
            "Energy-conserving blend toward the blurred image.",
        ),
        (
            "auto_exposure",
            post.auto_exposure.to_string(),
            "Histogram auto exposure.",
        ),
        (
            "exposure_ev",
            post.exposure_ev.to_string(),
            "Fixed EV100 when auto exposure is off.",
        ),
        (
            "exposure_compensation",
            post.exposure_compensation.to_string(),
            "Stops added to auto or fixed exposure.",
        ),
        (
            "auto_exposure_min_ev100",
            post.auto_exposure_min_ev100.to_string(),
            "Darkest scene auto exposure fully compensates.",
        ),
        (
            "auto_exposure_max_ev100",
            post.auto_exposure_max_ev100.to_string(),
            "Brightest scene auto exposure fully compensates.",
        ),
        (
            "tonemap",
            format!("{:?}", post.tonemap),
            "`AgX`, `AcesFitted` or `Neutral`.",
        ),
        (
            "contrast",
            post.contrast.to_string(),
            "Default-look S-curve (1 = bare AgX).",
        ),
        (
            "saturation",
            post.saturation.to_string(),
            "Default-look saturation (1 = bare AgX).",
        ),
        (
            "fxaa",
            post.fxaa.to_string(),
            "FXAA after the tonemap (set from the profile).",
        ),
        (
            "taa",
            post.taa.to_string(),
            "Temporal AA / upscale (set from the profile).",
        ),
        ("vignette", post.vignette.to_string(), "Vignette strength 0–1."),
        (
            "film_grain",
            post.film_grain.to_string(),
            "Film grain 0–1 (off for goldens).",
        ),
        (
            "use_physical_camera",
            post.use_physical_camera.to_string(),
            "Derive EV100 from aperture/shutter/ISO.",
        ),
        (
            "aperture",
            post.aperture.to_string(),
            "f-stops (physical camera).",
        ),
        (
            "shutter_seconds",
            post.shutter_seconds.to_string(),
            "Shutter time (physical camera).",
        ),
        ("iso", post.iso.to_string(), "ISO (physical camera)."),
        (
            "grade.white_balance_kelvin",
            post.grade.white_balance_kelvin.to_string(),
            "White balance (6500 = neutral).",
        ),
        ("grade.tint", post.grade.tint.to_string(), "Green–magenta tint."),
        (
            "meter.low_percentile",
            post.meter.low_percentile.to_string(),
            "Histogram percentile the meter starts at.",
        ),
        (
            "meter.high_percentile",
            post.meter.high_percentile.to_string(),
            "Histogram percentile the meter ends at.",
        ),
        (
            "meter.center_weight",
            post.meter.center_weight.to_string(),
            "Centre weighting of the meter.",
        ),
        (
            "lut",
            if post.lut.is_some() {
                "set".to_string()
            } else {
                "none".to_string()
            },
            "3D colour LUT after the grade.",
        ),
    ];
    for (field, value, meaning) in rows {
        let _ = writeln!(out, "| `{field}` | `{value}` | {meaning} |");
    }

    out.push_str("\n## Fog presets\n\n");
    out.push_str("| Preset | enabled | density | height_falloff | base_height | color | sky_inscatter |\n");
    out.push_str("|---|---|---|---|---|---|---|\n");
    for (name, fog) in [
        (
            "`FogSettings::aerial()` (default)",
            regia_render::FogSettings::aerial(),
        ),
        (
            "`FogSettings::outdoor_haze()`",
            regia_render::FogSettings::outdoor_haze(),
        ),
    ] {
        let _ = writeln!(
            out,
            "| {name} | {} | {} | {} | {} | {:?} | {} |",
            fog.enabled, fog.density, fog.height_falloff, fog.base_height, fog.color, fog.sky_inscatter
        );
    }
    out
}

// ---------------------------------------------------------------------------
// SUMMARY.md
// ---------------------------------------------------------------------------

/// One line of the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryItem {
    /// A part title (`# Part`).
    Part(String),
    /// A chapter: title, path relative to `book/`, nesting depth (0 = top level).
    Chapter {
        /// Title as written in the summary.
        title: String,
        /// Page path relative to `book/`.
        path: String,
        /// Nesting depth.
        depth: usize,
    },
}

/// Parses `book/SUMMARY.md` into parts and chapters, in order. Draft chapters (empty links) and
/// separators are dropped; the leading `# Summary` title is skipped.
pub fn parse_summary(text: &str) -> Vec<SummaryItem> {
    let mut items = Vec::new();
    let mut indents: Vec<usize> = Vec::new();
    let mut seen_title = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(title) = trimmed.strip_prefix("# ") {
            if !seen_title {
                seen_title = true;
                continue;
            }
            items.push(SummaryItem::Part(title.trim().to_string()));
            indents.clear();
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* "));
        let (entry, depth) = match body {
            Some(rest) => {
                while indents.last().is_some_and(|&i| i > indent) {
                    indents.pop();
                }
                if indents.last() != Some(&indent) {
                    indents.push(indent);
                }
                (rest, indents.len() - 1)
            }
            None if trimmed.starts_with('[') => {
                indents.clear();
                (trimmed, 0)
            }
            None => continue,
        };
        let Some(close) = entry.find("](") else { continue };
        let title = entry[1..close].to_string();
        let rest = &entry[close + 2..];
        let Some(end) = rest.find(')') else { continue };
        let path = rest[..end].trim().to_string();
        if path.is_empty() {
            continue;
        }
        items.push(SummaryItem::Chapter { title, path, depth });
    }
    items
}

// ---------------------------------------------------------------------------
// Markdown → HTML
// ---------------------------------------------------------------------------

/// The anchor id of a page: its path with separators and the extension folded into dashes. A
/// format page's key (`../docs/formats/<name>.md`) drops its leading `../`.
pub fn page_id(path: &str) -> String {
    let path = path.trim_start_matches("../");
    let stem = path.strip_suffix(".md").unwrap_or(path);
    let mut id = String::from("p-");
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch.to_ascii_lowercase());
        } else {
            id.push('-');
        }
    }
    id
}

/// The mdBook heading slug: lower-case alphanumerics, `_` and `-` kept, whitespace becomes `-`,
/// everything else is dropped.
pub fn slug(text: &str) -> String {
    text.chars()
        .filter_map(|ch| {
            if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                Some(ch.to_lowercase().next().unwrap_or(ch))
            } else if ch.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

/// Normalises `a/b/../c` to `a/c` (no filesystem access). Returns `None` when it climbs out of
/// the book root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// Where the format pages live, relative to `book/`. A book page that links to one of them pulls
/// it into the manual (after the summary chapters), keyed by this prefix plus the file name.
const FORMATS_KEY_PREFIX: &str = "../docs/formats/";

/// The key of the page a link destination path names, seen from `page`: the book-relative path
/// for a book page, `../docs/formats/<name>.md` for a format page. `None` when the path climbs
/// out of the repository or names any other file outside the book.
fn link_target(path: &str, page: &str) -> Option<String> {
    let dir = page.rsplit_once('/').map_or("", |(d, _)| d);
    let repo = normalize(&if dir.is_empty() {
        format!("book/{path}")
    } else {
        format!("book/{dir}/{path}")
    })?;
    if let Some(rel) = repo.strip_prefix("book/") {
        return Some(rel.to_string());
    }
    let name = repo.strip_prefix("docs/formats/")?;
    (name.ends_with(".md") && !name.contains('/')).then(|| format!("{FORMATS_KEY_PREFIX}{name}"))
}

/// Resolves a link found on `page` to an in-manual anchor, when it points at a page in the
/// manual (a summary chapter or an included format page).
fn resolve_link(dest: &str, page: &str, pages: &BTreeSet<String>) -> Option<String> {
    if dest.contains("://") || dest.starts_with("mailto:") {
        return None;
    }
    let (path, fragment) = match dest.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (dest, None),
    };
    let target = if path.is_empty() {
        page.to_string()
    } else {
        link_target(path, page)?
    };
    if !pages.contains(&target) {
        return None;
    }
    let id = page_id(&target);
    Some(match fragment {
        Some(f) if !f.is_empty() => format!("#{id}--{}", slug(f)),
        _ => format!("#{id}"),
    })
}

/// A rendered page: its HTML and its second-level headings (for the sidebar).
struct RenderedPage {
    html: String,
    headings: Vec<(String, String)>,
}

/// The manual's rendering decisions for one page: in-manual links, page-prefixed heading ids,
/// highlighted code.
struct PageHooks<'a> {
    page: &'a str,
    pid: String,
    pages: &'a BTreeSet<String>,
    used_ids: BTreeMap<String, usize>,
    headings: Vec<(String, String)>,
}

impl crate::markdown::Hooks for PageHooks<'_> {
    fn link(&mut self, dest: &str) -> String {
        if let Some(fragment) = dest.strip_prefix('#') {
            return format!("#{}--{}", self.pid, slug(fragment));
        }
        resolve_link(dest, self.page, self.pages).unwrap_or_else(|| dest.to_string())
    }

    fn heading_id(&mut self, level: u8, text: &str) -> Option<String> {
        let base = slug(text);
        let n = self.used_ids.entry(base.clone()).or_insert(0);
        let unique = if *n == 0 { base } else { format!("{base}-{n}") };
        *n += 1;
        let full = format!("{}--{unique}", self.pid);
        if level == 2 {
            self.headings.push((full.clone(), text.to_string()));
        }
        Some(full)
    }

    fn code_block(&mut self, info: &str, code: &str) -> String {
        let lang = info.split([',', ' ']).next().unwrap_or("").trim();
        let label = if lang.is_empty() { "text" } else { lang };
        format!(
            "<pre class=\"code\" data-lang=\"{}\"><code>{}</code></pre>\n",
            escape(label),
            highlight(lang, code)
        )
    }
}

/// Hooks that only record the format pages a page links to (`../docs/formats/<name>.md` keys).
struct FormatLinks<'a> {
    page: &'a str,
    found: BTreeSet<String>,
}

impl crate::markdown::Hooks for FormatLinks<'_> {
    fn link(&mut self, dest: &str) -> String {
        if !dest.contains("://") && !dest.starts_with("mailto:") {
            let path = dest.split_once('#').map_or(dest, |(p, _)| p);
            if let Some(key) = link_target(path, self.page).filter(|k| k.starts_with(FORMATS_KEY_PREFIX)) {
                self.found.insert(key);
            }
        }
        dest.to_string()
    }
}

/// The format pages linked from `markdown` (a page with key `page`).
fn format_links(markdown: &str, page: &str) -> BTreeSet<String> {
    let mut hooks = FormatLinks {
        page,
        found: BTreeSet::new(),
    };
    let _ = crate::markdown::render(markdown, &mut hooks);
    hooks.found
}

/// The title of an included page: its first `# ` heading without inline code ticks, else its
/// file name.
fn page_title(markdown: &str, key: &str) -> String {
    markdown
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .map(|t| t.replace('`', "").trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| key.rsplit('/').next().unwrap_or(key).to_string())
}

/// Renders one page's markdown with in-manual links, prefixed heading ids and highlighted code.
fn render_page(markdown: &str, page: &str, pages: &BTreeSet<String>) -> RenderedPage {
    let mut hooks = PageHooks {
        page,
        pid: page_id(page),
        pages,
        used_ids: BTreeMap::new(),
        headings: Vec::new(),
    };
    let html = crate::markdown::render(markdown, &mut hooks);
    // Wrap tables so wide ones scroll instead of widening the page.
    let html = html
        .replace("<table>", "<div class=\"table\"><table>")
        .replace("</table>", "</table></div>");
    let html = tag_callouts(html);
    RenderedPage {
        html,
        headings: hooks.headings,
    }
}

/// HTML-escapes text.
pub fn escape(text: &str) -> String {
    crate::markdown::escape(text)
}

// ---------------------------------------------------------------------------
// Syntax highlighting
// ---------------------------------------------------------------------------

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "static", "struct", "super", "trait", "type", "unsafe", "use", "where", "while", "self",
];
const RUST_LITERALS: &[&str] = &["true", "false", "Some", "None", "Ok", "Err", "Self"];
const WGSL_KEYWORDS: &[&str] = &[
    "fn", "let", "var", "const", "struct", "return", "if", "else", "for", "loop", "while", "break",
    "continue", "switch", "case", "default", "override", "alias", "discard", "enable",
];
const RON_LITERALS: &[&str] = &["true", "false", "Some", "None"];
const JSON_LITERALS: &[&str] = &["true", "false", "null"];

/// Highlights `code` for a fence language. Unknown languages are escaped only.
pub fn highlight(lang: &str, code: &str) -> String {
    match lang {
        "rust" | "rs" => lex_clike(code, RUST_KEYWORDS, RUST_LITERALS, Flavor::Rust),
        "wgsl" => lex_clike(code, WGSL_KEYWORDS, &["true", "false"], Flavor::Wgsl),
        "ron" => lex_clike(code, &[], RON_LITERALS, Flavor::Ron),
        "json" | "jsonc" => lex_clike(code, &[], JSON_LITERALS, Flavor::Json),
        "toml" | "ini" => lex_toml(code),
        "console" | "sh" | "bash" | "shell" | "fish" => lex_console(code, lang == "console"),
        _ => escape(code),
    }
}

/// Lexer variant for [`lex_clike`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Rust,
    Wgsl,
    Ron,
    Json,
}

/// Appends `text` wrapped in a span of `class`.
fn span(out: &mut String, class: &str, text: &str) {
    let _ = write!(out, "<span class=\"{class}\">{}</span>", escape(text));
}

/// A small lexer for C-family syntax: comments, strings, numbers, keywords, types, calls.
fn lex_clike(code: &str, keywords: &[&str], literals: &[&str], flavor: Flavor) -> String {
    let chars: Vec<char> = code.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(code.len() * 2);
    let mut i = 0;
    let text = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    while i < n {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // Comments.
        if c == '/' && next == Some('/') && flavor != Flavor::Json {
            let start = i;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            span(&mut out, "com", &text(start, i));
            continue;
        }
        if c == '/' && next == Some('*') {
            let start = i;
            i += 2;
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(n);
            span(&mut out, "com", &text(start, i));
            continue;
        }
        // Rust attributes.
        if flavor == Flavor::Rust
            && c == '#'
            && (next == Some('[') || (next == Some('!') && chars.get(i + 2) == Some(&'[')))
        {
            let start = i;
            let mut depth = 0i32;
            while i < n {
                match chars[i] {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    '\n' if depth <= 0 => break,
                    _ => {}
                }
                i += 1;
            }
            span(&mut out, "attr", &text(start, i));
            continue;
        }
        // Raw strings r"…", r#"…"#, br"…".
        if flavor == Flavor::Rust && (c == 'r' || (c == 'b' && next == Some('r'))) {
            let mut j = i + if c == 'b' { 2 } else { 1 };
            let mut hashes = 0;
            while j < n && chars[j] == '#' {
                hashes += 1;
                j += 1;
            }
            if j < n && chars[j] == '"' && (hashes > 0 || j == i + 1 || (c == 'b' && j == i + 2)) {
                let start = i;
                j += 1;
                'scan: while j < n {
                    if chars[j] == '"' {
                        let mut k = 0;
                        while k < hashes && j + 1 + k < n && chars[j + 1 + k] == '#' {
                            k += 1;
                        }
                        if k == hashes {
                            j += 1 + hashes;
                            break 'scan;
                        }
                    }
                    j += 1;
                }
                i = j.min(n);
                span(&mut out, "str", &text(start, i));
                continue;
            }
        }
        // Strings (and byte strings).
        if c == '"' || (flavor == Flavor::Rust && c == 'b' && next == Some('"')) {
            let start = i;
            i += if c == 'b' { 2 } else { 1 };
            while i < n && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(n);
            span(&mut out, "str", &text(start, i));
            continue;
        }
        // Char literals and lifetimes.
        if c == '\'' && matches!(flavor, Flavor::Rust | Flavor::Ron) {
            let is_char = matches!(
                (chars.get(i + 1), chars.get(i + 2)),
                (Some('\\'), _) | (Some(_), Some('\''))
            );
            if is_char {
                let start = i;
                i += 1;
                while i < n && chars[i] != '\'' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(n);
                span(&mut out, "str", &text(start, i));
                continue;
            }
            if flavor == Flavor::Rust
                && chars
                    .get(i + 1)
                    .is_some_and(|ch| ch.is_alphabetic() || *ch == '_')
            {
                let start = i;
                i += 1;
                while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                span(&mut out, "life", &text(start, i));
                continue;
            }
        }
        // Numbers.
        if c.is_ascii_digit()
            || (c == '-'
                && flavor != Flavor::Rust
                && next.is_some_and(|d| d.is_ascii_digit())
                && !prev_is_word(&chars, i))
        {
            let start = i;
            i += 1;
            while i < n {
                let ch = chars[i];
                let continues = ch.is_ascii_alphanumeric()
                    || ch == '_'
                    || (ch == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
                    || ((ch == '+' || ch == '-') && matches!(chars[i - 1], 'e' | 'E'));
                if !continues {
                    break;
                }
                i += 1;
            }
            span(&mut out, "num", &text(start, i));
            continue;
        }
        // Identifiers.
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word = text(start, i);
            let after = chars[i..].iter().find(|ch| **ch != ' ').copied();
            let class = if keywords.contains(&word.as_str()) {
                Some("kw")
            } else if literals.contains(&word.as_str()) {
                Some("lit")
            } else if flavor == Flavor::Rust && chars.get(i) == Some(&'!') && chars.get(i + 1) != Some(&'=') {
                i += 1;
                span(&mut out, "mac", &format!("{word}!"));
                continue;
            } else if flavor == Flavor::Ron && after == Some(':') && chars.get(i + 1) != Some(&':') {
                Some("field")
            } else if word.chars().next().is_some_and(char::is_uppercase) {
                Some("ty")
            } else if after == Some('(') && flavor != Flavor::Ron {
                Some("fn")
            } else {
                None
            };
            match class {
                Some(class) => span(&mut out, class, &word),
                None => out.push_str(&escape(&word)),
            }
            continue;
        }
        // JSON keys are strings followed by ':' — already coloured as strings.
        out.push_str(&escape(&c.to_string()));
        i += 1;
    }
    out
}

/// Whether the character before `i` continues a word (so `-` is an operator, not a sign).
fn prev_is_word(chars: &[char], i: usize) -> bool {
    i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_' || chars[i - 1] == ')')
}

/// TOML: section headers, keys, strings, numbers, booleans, comments.
fn lex_toml(code: &str) -> String {
    let mut out = String::with_capacity(code.len() * 2);
    for (index, line) in code.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        out.push_str(indent);
        if trimmed.starts_with('#') {
            span(&mut out, "com", trimmed);
        } else if trimmed.starts_with('[') {
            let (head, comment) = split_comment(trimmed);
            span(&mut out, "ty", head);
            if !comment.is_empty() {
                span(&mut out, "com", comment);
            }
        } else if let Some(eq) = trimmed.find('=') {
            span(&mut out, "field", &trimmed[..eq]);
            out.push('=');
            let (value, comment) = split_comment(&trimmed[eq + 1..]);
            out.push_str(&lex_clike(value, &[], &["true", "false"], Flavor::Json));
            if !comment.is_empty() {
                span(&mut out, "com", comment);
            }
        } else {
            out.push_str(&escape(trimmed));
        }
    }
    out
}

/// Splits a line at a `#` comment that is not inside a string.
fn split_comment(line: &str) -> (&str, &str) {
    let mut in_str = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_str = !in_str,
            '#' if !in_str => return (&line[..i], &line[i..]),
            _ => {}
        }
    }
    (line, "")
}

/// Shell sessions: `$ ` prompts with the command highlighted, output dimmed. Without prompts
/// (`sh` fences) every line is a command.
fn lex_console(code: &str, prompts: bool) -> String {
    let mut out = String::with_capacity(code.len() * 2);
    let has_prompt = code.lines().any(|l| l.starts_with("$ "));
    for (index, line) in code.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if let Some(cmd) = line.strip_prefix("$ ") {
            span(&mut out, "prompt", "$ ");
            out.push_str(&shell_command(cmd));
        } else if (prompts && has_prompt)
            || (prompts
                && !line.starts_with(char::is_alphanumeric)
                && !line.is_empty()
                && !line.starts_with("cargo")
                && !line.starts_with("regia"))
        {
            span(&mut out, "out", line);
        } else if line.trim_start().starts_with('#') {
            span(&mut out, "com", line);
        } else {
            out.push_str(&shell_command(line));
        }
    }
    out
}

/// One shell command line: the program, flags, strings and a trailing comment.
fn shell_command(line: &str) -> String {
    let mut out = String::new();
    let (cmd, comment) = match line.find(" #") {
        Some(i) => (&line[..i], &line[i..]),
        None => (line, ""),
    };
    let mut first = true;
    let mut in_str: Option<char> = None;
    let mut word = String::new();
    let flush = |out: &mut String, word: &mut String, first: &mut bool| {
        if word.is_empty() {
            return;
        }
        if *first && !word.contains('=') {
            span(out, "kw", word);
            *first = false;
        } else if word.starts_with('-') {
            span(out, "attr", word);
        } else if word.starts_with('"') || word.starts_with('\'') {
            span(out, "str", word);
        } else {
            out.push_str(&escape(word));
        }
        word.clear();
    };
    for ch in cmd.chars() {
        match in_str {
            Some(q) => {
                word.push(ch);
                if ch == q {
                    in_str = None;
                }
            }
            None if ch == '"' || ch == '\'' => {
                in_str = Some(ch);
                word.push(ch);
            }
            None if ch == ' ' => {
                flush(&mut out, &mut word, &mut first);
                out.push(' ');
            }
            None if ch == '|' || ch == '&' || ch == ';' => {
                flush(&mut out, &mut word, &mut first);
                out.push_str(&escape(&ch.to_string()));
                first = true;
            }
            None => word.push(ch),
        }
    }
    flush(&mut out, &mut word, &mut first);
    if !comment.is_empty() {
        span(&mut out, "com", comment);
    }
    out
}

// ---------------------------------------------------------------------------
// The single-file manual
// ---------------------------------------------------------------------------

/// Renders every chapter of the summary into one self-contained HTML document.
pub fn render_manual(book: &Path) -> Result<String> {
    let css = theme_css(book)?;
    let mark = logomark_svg(book)?;
    let summary_text = std::fs::read_to_string(book.join(crate::book::SUMMARY))
        .with_context(|| format!("reading {}", book.join(crate::book::SUMMARY).display()))?;
    let items = parse_summary(&summary_text);
    let title = book_title(book);

    let chapters: Vec<(&str, &str, usize)> = items
        .iter()
        .filter_map(|item| match item {
            SummaryItem::Chapter { title, path, depth } => Some((title.as_str(), path.as_str(), *depth)),
            SummaryItem::Part(_) => None,
        })
        .collect();
    let mut pages: BTreeSet<String> = chapters.iter().map(|(_, p, _)| (*p).to_string()).collect();

    // Read every chapter, then pull in the format pages they link to (and the ones those link
    // to), so a link from a book page into `docs/formats/` lands on a section of this file.
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    for (_, path, _) in &chapters {
        if sources.contains_key(*path) {
            continue;
        }
        let file = book.join(path);
        let markdown =
            std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        sources.insert((*path).to_string(), markdown);
    }
    let mut pending: Vec<String> = sources
        .iter()
        .flat_map(|(path, markdown)| format_links(markdown, path))
        .collect();
    let mut appendix: BTreeMap<String, String> = BTreeMap::new();
    while let Some(key) = pending.pop() {
        if pages.contains(&key) || appendix.contains_key(&key) {
            continue;
        }
        let file = book.join(&key);
        let markdown =
            std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        pending.extend(format_links(&markdown, &key));
        sources.insert(key.clone(), markdown.clone());
        appendix.insert(key, markdown);
    }
    pages.extend(appendix.keys().cloned());
    let appendix: Vec<(String, String)> = appendix
        .iter()
        .map(|(key, markdown)| (page_title(markdown, key), key.clone()))
        .collect();

    let mut rendered: BTreeMap<String, RenderedPage> = BTreeMap::new();
    for (path, markdown) in &sources {
        rendered.insert(path.clone(), render_page(markdown, path, &pages));
    }

    // Sidebar.
    let mut nav = String::new();
    nav.push_str("<ul class=\"toc\">\n");
    for item in &items {
        match item {
            SummaryItem::Part(name) => {
                let _ = writeln!(nav, "<li class=\"part\">{}</li>", escape(name));
            }
            SummaryItem::Chapter { title, path, depth } => {
                let id = page_id(path);
                let _ = write!(
                    nav,
                    "<li class=\"chap depth{}\" data-page=\"{id}\"><a href=\"#{id}\">{}</a>",
                    depth.min(&3),
                    escape(title)
                );
                if let Some(page) = rendered.get(path)
                    && !page.headings.is_empty()
                {
                    nav.push_str("<ul class=\"sub\">");
                    for (hid, text) in &page.headings {
                        let _ = write!(nav, "<li><a href=\"#{hid}\">{}</a></li>", escape(text));
                    }
                    nav.push_str("</ul>");
                }
                nav.push_str("</li>\n");
            }
        }
    }
    if !appendix.is_empty() {
        nav.push_str("<li class=\"part\">Format pages</li>\n");
        for (title, key) in &appendix {
            let id = page_id(key);
            let _ = writeln!(
                nav,
                "<li class=\"chap depth0\" data-page=\"{id}\"><a href=\"#{id}\">{}</a></li>",
                escape(title)
            );
        }
    }
    nav.push_str("</ul>\n");

    // Chapters, then the included format pages, with previous/next links.
    let mut body = String::new();
    let mut seen = BTreeSet::new();
    let order: Vec<(&str, &str)> = chapters
        .iter()
        .filter(|(_, p, _)| seen.insert(*p))
        .map(|(t, p, _)| (*t, *p))
        .chain(appendix.iter().map(|(t, k)| (t.as_str(), k.as_str())))
        .collect();
    for (index, (chapter_title, path)) in order.iter().enumerate() {
        let id = page_id(path);
        let Some(page) = rendered.get(*path) else { continue };
        let _ = writeln!(
            body,
            "<section class=\"chapter\" id=\"{id}\" data-title=\"{}\">\n<div class=\"crumb\">{}</div>",
            escape(chapter_title),
            escape(path.trim_start_matches("../"))
        );
        if index == 0 {
            let _ = writeln!(
                body,
                "<header class=\"title-page\" aria-label=\"PiercingXX. Regia Engine.\">\
                 <a class=\"lockup\" href=\"#{id}\"><span class=\"mark-slot\" aria-hidden=\"true\">{mark}</span>\
                 <span class=\"wordmark\">Regia Engine</span></a>\
                 <p class=\"title-kicker\">Manual</p></header>"
            );
        }
        body.push_str(&page.html);
        body.push_str("<nav class=\"pager\">");
        if let Some((t, p)) = index.checked_sub(1).and_then(|i| order.get(i)) {
            let _ = write!(
                body,
                "<a class=\"prev\" href=\"#{}\">← {}</a>",
                page_id(p),
                escape(t)
            );
        }
        if let Some((t, p)) = order.get(index + 1) {
            let _ = write!(
                body,
                "<a class=\"next\" href=\"#{}\">{} →</a>",
                page_id(p),
                escape(t)
            );
        }
        body.push_str("</nav>\n</section>\n");
    }

    let mut html = String::with_capacity(body.len() + nav.len() + css.len() + 16 * 1024);
    let _ = write!(
        html,
        "<!DOCTYPE html>\n<html lang=\"en\" class=\"pxx-manual\" data-theme=\"dark\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"generator\" content=\"cargo xtask manual\">\n\
         <title>{title}</title>\n<script>{THEME_BOOT}</script>\n<style>{css}</style>\n</head>\n<body>\n\
         <aside id=\"sidebar\">\n<div class=\"brand\"><a class=\"lockup\" href=\"#{first}\">\
         <span class=\"mark-slot\" aria-hidden=\"true\">{mark}</span><span class=\"wordmark\">Regia Engine</span></a>\
         <button id=\"theme\" type=\"button\" title=\"Switch to Paper\" aria-label=\"Switch to Paper\">Paper</button></div>\n\
         <input id=\"search\" type=\"search\" placeholder=\"Search the manual  ( / )\" autocomplete=\"off\" aria-label=\"Search\">\n\
         <div id=\"results\" hidden></div>\n<nav id=\"nav\">\n{nav}</nav>\n</aside>\n\
         <button id=\"menu\" type=\"button\" aria-label=\"Table of contents\">☰</button>\n\
         <main id=\"content\">\n{body}</main>\n<script>{JS}</script>\n</body>\n</html>\n",
        title = escape(&title),
        first = order.first().map(|(_, p)| page_id(p)).unwrap_or_default(),
    );
    Ok(html)
}

/// The `title` from `book/book.toml`, or a default.
fn book_title(book: &Path) -> String {
    std::fs::read_to_string(book.join("book.toml"))
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let line = line.trim();
                let value = line.strip_prefix("title")?.trim_start().strip_prefix('=')?.trim();
                Some(value.trim_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| "Regia Engine Manual".to_string())
}

/// Sets the theme before first paint. Stored choice wins; otherwise AMOLED Night.
const THEME_BOOT: &str = "(function(){var t=null;try{t=localStorage.getItem('regia-manual-theme');}catch(e){}\
if(t!=='dark'&&t!=='light')t='dark';\
document.documentElement.setAttribute('data-theme',t);document.documentElement.classList.add('js');})();";

/// Routing (one chapter shown at a time, from the URL hash), search and the theme switch.
const JS: &str = r##"
(function(){
var root=document.documentElement;
var chapters=[].slice.call(document.querySelectorAll('section.chapter'));
var navItems={};[].forEach.call(document.querySelectorAll('.toc .chap'),function(li){navItems[li.getAttribute('data-page')]=li;});
var current=null;
function show(hash,scroll){
  var id=decodeURIComponent((hash||'').replace(/^#/,''));
  var el=id?document.getElementById(id):null;
  var sec=el?(el.closest?el.closest('section.chapter'):null):null;
  if(!sec){sec=chapters[0];el=null;}
  if(!sec)return;
  if(current!==sec){
    if(current){current.classList.remove('active');var o=navItems[current.id];if(o)o.classList.remove('active');}
    sec.classList.add('active');current=sec;
    var li=navItems[sec.id];if(li){li.classList.add('active');if(li.scrollIntoView&&!isVisible(li))li.scrollIntoView({block:'center'});}
    document.title=sec.getAttribute('data-title')+' — Regia Engine Manual';
  }
  if(scroll!==false){if(el&&el!==sec){el.scrollIntoView();}else{window.scrollTo(0,0);}}
  document.body.classList.remove('open');
}
function isVisible(li){var r=li.getBoundingClientRect();var s=document.getElementById('sidebar').getBoundingClientRect();return r.top>=s.top&&r.bottom<=s.bottom;}
window.addEventListener('hashchange',function(){show(location.hash);});
show(location.hash);
function paintTheme(){
  var light=root.getAttribute('data-theme')==='light';
  var btn=document.getElementById('theme');
  var label=light?'Night':'Paper';
  btn.textContent=label;
  btn.setAttribute('title',light?'Switch to AMOLED Night':'Switch to Paper');
  btn.setAttribute('aria-label',btn.getAttribute('title'));
}
paintTheme();
document.getElementById('theme').addEventListener('click',function(){
  var t=root.getAttribute('data-theme')==='dark'?'light':'dark';root.setAttribute('data-theme',t);
  try{localStorage.setItem('regia-manual-theme',t);}catch(e){}
  paintTheme();
});
document.getElementById('menu').addEventListener('click',function(){document.body.classList.toggle('open');});
var index=null;
function buildIndex(){
  index=[];
  chapters.forEach(function(sec){
    var page=sec.getAttribute('data-title');var entry=null;
    [].forEach.call(sec.children,function(node){
      if(/^H[1-4]$/.test(node.tagName)){entry={id:node.id||sec.id,title:node.textContent,page:page,text:''};index.push(entry);}
      else if(node.classList&&(node.classList.contains('crumb')||node.classList.contains('pager')||node.classList.contains('title-page'))){}
      else{if(!entry){entry={id:sec.id,title:page,page:page,text:''};index.push(entry);}entry.text+=' '+node.textContent;}
    });
  });
  index.forEach(function(e){e.text=e.text.replace(/\s+/g,' ').trim();e.lt=e.title.toLowerCase();e.lx=e.text.toLowerCase();});
}
function esc(s){return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');}
function hl(s,terms){var out=esc(s);terms.forEach(function(t){if(!t)return;var re=new RegExp('('+esc(t).replace(/[.*+?^${}()|[\]\\]/g,'\\$&')+')','gi');out=out.replace(re,'<mark>$1</mark>');});return out;}
var input=document.getElementById('search'),results=document.getElementById('results'),nav=document.getElementById('nav');
function search(){
  var q=input.value.trim().toLowerCase();
  if(!q){results.hidden=true;nav.hidden=false;return;}
  if(!index)buildIndex();
  var terms=q.split(/\s+/).filter(Boolean);var hits=[];
  index.forEach(function(e){
    var score=0;
    for(var i=0;i<terms.length;i++){var t=terms[i];var inT=e.lt.indexOf(t)>=0;var inX=e.lx.indexOf(t)>=0;if(!inT&&!inX)return;score+=(inT?20:0)+(inX?Math.min(10,e.lx.split(t).length-1):0);}
    if(e.lt===q)score+=50;hits.push({e:e,s:score});
  });
  hits.sort(function(a,b){return b.s-a.s;});hits=hits.slice(0,40);
  var html='';
  hits.forEach(function(h){
    var e=h.e;var p=e.lx.indexOf(terms[0]);var start=Math.max(0,p-70);var snip=p>=0?(start>0?'… ':'')+e.text.substr(start,180)+(e.text.length>start+180?' …':''):e.text.substr(0,160);
    html+='<a href="#'+e.id+'"><div class="rt">'+hl(e.title,terms)+'</div><div class="rp">'+esc(e.page)+'</div><div class="rs">'+hl(snip,terms)+'</div></a>';
  });
  results.innerHTML=html||'<div class="none">No matches.</div>';results.hidden=false;nav.hidden=true;
}
input.addEventListener('input',search);
input.addEventListener('keydown',function(ev){
  if(ev.key==='Escape'){input.value='';search();input.blur();}
  if(ev.key==='Enter'){var a=results.querySelector('a');if(a){location.hash=a.getAttribute('href');}}
});
results.addEventListener('click',function(ev){var a=ev.target.closest('a');if(a&&a.getAttribute('href')===location.hash){show(location.hash);}});
document.addEventListener('keydown',function(ev){if(ev.key==='/'&&document.activeElement!==input){ev.preventDefault();input.focus();}});
})();
"##;

/// Rewrites the paths of every chapter file (for tests).
#[must_use]
pub fn chapter_paths(summary: &str) -> Vec<PathBuf> {
    parse_summary(summary)
        .into_iter()
        .filter_map(|item| match item {
            SummaryItem::Chapter { path, .. } => Some(PathBuf::from(path)),
            SummaryItem::Part(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_parts_prefix_and_nested_chapters() {
        let text = "# Summary\n\n[Intro](intro.md)\n\n# Guide\n\n- [A](a.md)\n  - [B](a/b.md)\n    - [C](a/c.md)\n- [D](d.md)\n- [Draft]()\n\n---\n";
        let items = parse_summary(text);
        assert_eq!(
            items,
            vec![
                SummaryItem::Chapter {
                    title: "Intro".into(),
                    path: "intro.md".into(),
                    depth: 0
                },
                SummaryItem::Part("Guide".into()),
                SummaryItem::Chapter {
                    title: "A".into(),
                    path: "a.md".into(),
                    depth: 0
                },
                SummaryItem::Chapter {
                    title: "B".into(),
                    path: "a/b.md".into(),
                    depth: 1
                },
                SummaryItem::Chapter {
                    title: "C".into(),
                    path: "a/c.md".into(),
                    depth: 2
                },
                SummaryItem::Chapter {
                    title: "D".into(),
                    path: "d.md".into(),
                    depth: 0
                },
            ]
        );
        assert_eq!(chapter_paths(text).len(), 5);
    }

    #[test]
    fn slugs_match_mdbook() {
        assert_eq!(slug("Hello, World!"), "hello-world");
        assert_eq!(slug("The `scene.*` methods"), "the-scene-methods");
        assert_eq!(
            slug("Glass, IOR and emission strength (format version 3)"),
            "glass-ior-and-emission-strength-format-version-3"
        );
        assert_eq!(page_id("user-guide/scenes.md"), "p-user-guide-scenes");
    }

    #[test]
    fn links_resolve_to_anchors() {
        let pages: BTreeSet<String> = ["user-guide/scenes.md", "agents/scene.md", "intro.md"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(
            resolve_link("../agents/scene.md#query", "user-guide/scenes.md", &pages),
            Some("#p-agents-scene--query".into())
        );
        assert_eq!(
            resolve_link("user-guide/scenes.md", "intro.md", &pages),
            Some("#p-user-guide-scenes".into())
        );
        assert_eq!(resolve_link("https://x.y/a.md", "intro.md", &pages), None);
        assert_eq!(resolve_link("missing.md", "intro.md", &pages), None);
        assert_eq!(resolve_link("../../outside.md", "intro.md", &pages), None);
    }

    #[test]
    fn links_into_docs_formats_resolve_to_included_pages() {
        let pages: BTreeSet<String> = [
            "agents/functional-tests.md",
            "../docs/formats/functional-test.md",
            "../docs/formats/functest-ron.md",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        assert_eq!(
            resolve_link(
                "../../docs/formats/functional-test.md",
                "agents/functional-tests.md",
                &pages
            ),
            Some("#p-docs-formats-functional-test".into())
        );
        // A format page linking a sibling, and back into the book.
        assert_eq!(
            resolve_link(
                "functest-ron.md#fields",
                "../docs/formats/functional-test.md",
                &pages
            ),
            Some("#p-docs-formats-functest-ron--fields".into())
        );
        assert_eq!(
            resolve_link(
                "../../book/agents/functional-tests.md",
                "../docs/formats/functest-ron.md",
                &pages
            ),
            Some("#p-agents-functional-tests".into())
        );
        // Only `docs/formats/*.md` is pulled in; other docs and nested paths are not pages.
        assert_eq!(
            link_target("../../docs/specs/41-x.md", "agents/functional-tests.md"),
            None
        );
        assert_eq!(
            link_target("../../docs/formats/sub/a.md", "agents/functional-tests.md"),
            None
        );
        assert_eq!(
            link_target("../../docs/formats/a.schema.json", "agents/functional-tests.md"),
            None
        );
        assert_eq!(link_target("../../../x.md", "agents/functional-tests.md"), None);
        assert_eq!(
            resolve_link(
                "../../docs/formats/expected-ron.md",
                "agents/functional-tests.md",
                &pages
            ),
            None,
            "a format page the manual did not include stays a plain link"
        );
        let found = format_links(
            "See [a](../../docs/formats/a.md#x), [b](../agents/b.md), [s](../../docs/specs/s.md) and \
             `[c](../../docs/formats/c.md)`.\n",
            "agents/functional-tests.md",
        );
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec!["../docs/formats/a.md".to_string()]
        );
        assert_eq!(
            page_id("../docs/formats/test-report.md"),
            "p-docs-formats-test-report"
        );
        assert_eq!(
            page_title(
                "# `TestReport` JSON (v1)\n\nx\n",
                "../docs/formats/test-report.md"
            ),
            "TestReport JSON (v1)"
        );
        assert_eq!(
            page_title("no heading\n", "../docs/formats/test-report.md"),
            "test-report.md"
        );
    }

    #[test]
    fn page_render_prefixes_ids_and_highlights() {
        let pages: BTreeSet<String> = ["a.md", "b.md"].iter().map(|s| (*s).to_string()).collect();
        let md = "# Title\n\n## Part one\n\nSee [b](b.md#x) and [here](#part-one).\n\n## Part one\n\n```rust\nfn main() { let s = \"<x>\"; } // c\n```\n";
        let page = render_page(md, "a.md", &pages);
        assert!(page.html.contains("id=\"p-a--title\""), "{}", page.html);
        assert!(page.html.contains("id=\"p-a--part-one\""));
        assert!(
            page.html.contains("id=\"p-a--part-one-1\""),
            "duplicate headings get a suffix"
        );
        assert!(page.html.contains("href=\"#p-b--x\""));
        assert!(page.html.contains("href=\"#p-a--part-one\""));
        assert!(page.html.contains("<span class=\"kw\">fn</span>"));
        assert!(
            page.html
                .contains("<span class=\"str\">&quot;&lt;x&gt;&quot;</span>")
        );
        assert!(page.html.contains("<span class=\"com\">// c</span>"));
        assert_eq!(page.headings.len(), 2);
    }

    #[test]
    fn highlighters_escape_everything() {
        for lang in ["rust", "ron", "toml", "console", "wgsl", "json", "text"] {
            let html = highlight(
                lang,
                "a < b && \"c\" > 'd' # e\n$ regia new \"x\" --template blank",
            );
            assert!(!html.contains("< b"), "{lang}: {html}");
            assert!(!html.contains("\"c\" >"), "{lang}: {html}");
        }
        let console = highlight("console", "$ cargo run -p game\nCompiling…");
        assert!(console.contains("<span class=\"prompt\">$ </span><span class=\"kw\">cargo</span>"));
        assert!(console.contains("<span class=\"attr\">-p</span>"));
        assert!(console.contains("<span class=\"out\">Compiling…</span>"));
        let ron = highlight("ron", "(name: \"x\", value: Some(1.5))");
        assert!(ron.contains("<span class=\"field\">name</span>"));
        assert!(ron.contains("<span class=\"lit\">Some</span>"));
        let toml = highlight("toml", "[project]\nname = \"x\" # c");
        assert!(toml.contains("<span class=\"ty\">[project]</span>"));
        assert!(toml.contains("<span class=\"com\"># c</span>"));
        let rust = highlight(
            "rust",
            "#[derive(Debug)]\nfn f<'a>(x: &'a str) -> char { println!(\"{x}\"); 'c' }",
        );
        assert!(rust.contains("<span class=\"attr\">#[derive(Debug)]</span>"));
        assert!(
            rust.contains("<span class=\"life\">&#39;a</span>")
                || rust.contains("<span class=\"life\">'a</span>")
        );
        assert!(rust.contains("<span class=\"mac\">println!</span>"));
        assert!(rust.contains("<span class=\"str\">'c'</span>"));
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert!(base64_encode(b"wOF2").starts_with("d09GMg"));
    }

    #[test]
    fn callouts_use_status_classes() {
        let html = tag_callouts(
            "<blockquote>\n<p><strong>planned</strong> work.</p>\n</blockquote>\n\
             <blockquote>\n<p>Warning: do not.</p>\n</blockquote>\n\
             <blockquote>\n<p>A fact.</p>\n</blockquote>\n\
             <p><strong>planned</strong></p>\n"
                .into(),
        );
        assert!(html.contains("<blockquote class=\"callout planned\">"));
        assert!(html.contains("<blockquote class=\"callout warn\">"));
        assert!(html.contains("<blockquote class=\"callout note\">"));
        assert_eq!(html.matches("class=\"status planned\"").count(), 2);
        assert!(!html.contains("<strong>planned</strong>"));
    }
}
