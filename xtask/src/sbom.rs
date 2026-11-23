// SPDX-License-Identifier: MPL-2.0
//! `sbom`: a CycloneDX 1.5 JSON bill of materials (ADR-2760).
//!
//! Lists the third-party crates a Regia Engine binary can link (the same resolved trees
//! `cargo xtask notices` reads, through [`notices::third_party`]), the source ported from other
//! projects (`docs/ported-sources.toml`) and the binary-asset inventory (`docs/asset-licences.toml`).
//!
//! Two profiles:
//! - `engine`: every crate Regia Engine ships (engine, editor, CLI) with every optional feature: what
//!   `THIRD_PARTY_NOTICES.md` covers (ADR-5556).
//! - `game`: only what a shipped game links: the runtime crates and samples
//!   (`layers::Layer::ships_in_games`), so no editor or importer crate and none of their dependencies,
//!   resolved with each crate's default features plus the umbrella's media codecs, plus the features
//!   the game names (`--features`), or every feature (`--all-features`). A crate behind an
//!   off-by-default feature the game does not turn on is not in its bill (ADR-5903).
//!
//! The output is deterministic: sorted, no timestamp unless asked for, and the serial number is
//! derived from the content.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::graph::Graph;
use crate::notices::{self, AssetRow, PortedSource};

/// Which set of crates and assets the bill covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Profile {
    /// Everything Regia Engine ships: engine, editor and CLI.
    Engine,
    /// What a shipped game can link.
    Game,
}

impl Profile {
    /// The profile's name as written in the document.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Engine => "engine",
            Profile::Game => "game",
        }
    }
}

/// Everything the document is built from.
pub struct Inputs<'a> {
    /// Resolved trees of the profile's workspace crates (root is package 0).
    pub trees: &'a [Graph],
    /// Ported-source entries.
    pub ported: &'a [PortedSource],
    /// Asset rows with the number of tracked files each covers.
    pub assets: &'a [(AssetRow, usize)],
    /// `(name, version)` -> SHA-256 from `Cargo.lock`.
    pub checksums: &'a BTreeMap<(String, String), String>,
    /// Workspace version.
    pub version: &'a str,
    /// The feature set the trees were resolved with, as written in `metadata.properties`
    /// (`regia:features`): `all`, or `default` and the added `crate/feature` names.
    pub features: &'a str,
    /// ISO 8601 timestamp, only when asked for.
    pub timestamp: Option<String>,
}

/// SHA-256 checksums of the registry packages in a `Cargo.lock` text.
pub fn lock_checksums(lock: &str) -> Result<BTreeMap<(String, String), String>> {
    let value: toml::Value = toml::from_str(lock).context("parsing Cargo.lock")?;
    let mut out = BTreeMap::new();
    for pkg in value
        .get("package")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
    {
        let field = |k: &str| pkg.get(k).and_then(|v| v.as_str());
        if let (Some(name), Some(version), Some(sum)) = (field("name"), field("version"), field("checksum")) {
            out.insert((name.to_string(), version.to_string()), sum.to_string());
        }
    }
    Ok(out)
}

fn purl(name: &str, version: &str) -> String {
    format!("pkg:cargo/{name}@{version}")
}

/// Whether a ported source can ship in a game build: at least one `files_to` path lives in a
/// runtime-or-sample crate. Paths outside `crates/` (shaders, data files) keep the entry —
/// over-disclosure is safe.
fn ships_in_games_ported(s: &PortedSource) -> bool {
    s.files_to.iter().any(|to| {
        let krate = to.strip_prefix("crates/").and_then(|rest| rest.split('/').next());
        match krate {
            Some(name) => crate::layers::layer_of(name).is_none_or(|l| l.ships_in_games()),
            None => true,
        }
    })
}

/// The CycloneDX `licenses` array for an SPDX expression (Cargo's old `A/B` spelling becomes `A OR B`).
fn licences(expr: &str) -> Value {
    let expr = expr.trim();
    if expr.is_empty() || expr.starts_with("UNKNOWN") || expr == notices::UNKNOWN_LICENCE {
        return json!([{ "license": { "name": "UNKNOWN" } }]);
    }
    let expr = if expr.contains('/') && !expr.contains(' ') {
        expr.replace('/', " OR ")
    } else {
        expr.to_string()
    };
    json!([{ "expression": expr }])
}

/// A deterministic RFC 4122 style serial number from the document content.
fn serial(content: &str) -> String {
    let hash = blake3::hash(content.as_bytes());
    let b = hash.as_bytes();
    let mut hex = String::new();
    for byte in &b[..16] {
        let _ = write!(hex, "{byte:02x}");
    }
    // Version nibble 5 (name based) and variant 10xx.
    let variant = ['8', '9', 'a', 'b'][usize::from(b[8] & 3)];
    format!(
        "urn:uuid:{}-{}-5{}-{}{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[13..16],
        variant,
        &hex[17..20],
        &hex[20..32]
    )
}

/// Builds the document.
pub fn build(profile: Profile, input: &Inputs<'_>) -> Value {
    let crates = notices::third_party(input.trees);

    // Dependency edges between third-party packages; the root depends on what workspace crates use.
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut root_deps: BTreeSet<String> = BTreeSet::new();
    for graph in input.trees {
        for (idx, pkg) in graph.packages.iter().enumerate() {
            for edge in graph.edges[idx].iter().filter(|e| !e.dev_only) {
                let to = &graph.packages[edge.to];
                if to.workspace {
                    continue;
                }
                let to_ref = purl(&to.name, &to.version);
                if pkg.workspace {
                    root_deps.insert(to_ref);
                } else {
                    edges
                        .entry(purl(&pkg.name, &pkg.version))
                        .or_default()
                        .insert(to_ref);
                }
            }
        }
    }

    let mut components: Vec<Value> = Vec::new();
    for ((name, version), (licence, repo)) in &crates {
        let mut c = json!({
            "type": "library",
            "bom-ref": purl(name, version),
            "name": name,
            "version": version,
            "purl": purl(name, version),
            "scope": "required",
            "licenses": licences(licence),
        });
        if let Some(sum) = input.checksums.get(&(name.clone(), version.clone())) {
            c["hashes"] = json!([{ "alg": "SHA-256", "content": sum }]);
        }
        if !repo.is_empty() {
            c["externalReferences"] = json!([{ "type": "vcs", "url": repo }]);
        }
        components.push(c);
    }

    let mut ported: Vec<&PortedSource> = input.ported.iter().collect();
    // Sort by the files too: same (project, commit) pairs occur in docs/ported-sources.toml
    // (several file sets ported from one upstream commit), and the tie-break keeps the output
    // deterministic regardless of the file's row order.
    ported.sort_by(|a, b| (&a.project, &a.commit, &a.files_to).cmp(&(&b.project, &b.commit, &b.files_to)));
    // CycloneDX requires unique bom-refs: disambiguate same project+commit entries with a
    // stable `#N` suffix (review fixrev-03-k finding 3).
    let mut ported_seen: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for s in ported {
        // The game profile omits ported sources that live only in editor/pipeline crates — the
        // same layer filter the crate trees get — so the bill states "no editor or importer
        // crate" truthfully (review fixrev-03-k finding 5; over-disclosure would also be safe).
        if profile == Profile::Game && !ships_in_games_ported(s) {
            continue;
        }
        let key = (s.project.as_str(), s.commit.as_str());
        let ordinal = ported_seen.entry(key).and_modify(|n| *n += 1).or_insert(0);
        let bom_ref = if *ordinal == 0 {
            format!("ported:{}@{}", s.project, s.commit)
        } else {
            format!("ported:{}@{}#{}", s.project, s.commit, *ordinal + 1)
        };
        components.push(json!({
            "type": "library",
            "bom-ref": bom_ref,
            "name": s.project,
            "version": s.commit,
            "description": "Source files ported into Regia Engine (docs/ported-sources.toml)",
            "copyright": s.copyright,
            "licenses": licences(&s.licence),
            "externalReferences": [{ "type": "vcs", "url": s.url }],
            "properties": [
                { "name": "regia:kind", "value": "ported-source" },
                { "name": "regia:files", "value": s.files_to.join(", ") },
            ],
        }));
    }

    let mut assets: Vec<&(AssetRow, usize)> = input
        .assets
        .iter()
        .filter(|(row, _)| profile == Profile::Engine || row.ships)
        .collect();
    assets.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    for (row, files) in assets {
        components.push(json!({
            "type": "data",
            "bom-ref": format!("asset:{}", row.path),
            "name": row.path,
            "description": row.source,
            "licenses": licences(&row.licence),
            "properties": [
                { "name": "regia:kind", "value": "asset" },
                { "name": "regia:origin", "value": row.origin },
                { "name": "regia:ships", "value": row.ships.to_string() },
                { "name": "regia:files", "value": files.to_string() },
            ],
        }));
    }

    let root_ref = format!("regia-engine-{}", profile.name());
    let mut dependencies = vec![json!({ "ref": root_ref, "dependsOn": root_deps })];
    for (name, version) in crates.keys() {
        let r = purl(name, version);
        dependencies.push(json!({ "ref": r, "dependsOn": edges.get(&r).cloned().unwrap_or_default() }));
    }

    let mut metadata = json!({
        "component": {
            "type": if profile == Profile::Game { "application" } else { "framework" },
            "bom-ref": root_ref,
            "name": format!("regia-engine ({})", profile.name()),
            "version": input.version,
            "licenses": [{ "license": { "id": "MPL-2.0" } }],
        },
        "properties": [
            { "name": "regia:profile", "value": profile.name() },
            { "name": "regia:generator", "value": "cargo xtask sbom" },
            { "name": "regia:features", "value": input.features },
        ],
    });
    if let Some(ts) = &input.timestamp {
        metadata["timestamp"] = json!(ts);
    }
    let mut doc = json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": metadata,
        "components": components,
        "dependencies": dependencies,
    });
    let serial_number = serial(&doc.to_string());
    doc["serialNumber"] = json!(serial_number);
    doc
}

/// Seconds since the Unix epoch as `YYYY-MM-DDThh:mm:ssZ` (proleptic Gregorian, civil-from-days).
pub fn iso8601(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Optional features the umbrella resolves in every game bill (ADR-2760): the media codecs and their
/// x86 assembly, so a media-enabled game's codec crates (`rav1d`, `unsafe-libopus`, `lewton`) are
/// listed.
pub const GAME_BASE_FEATURES: &[(&str, &str)] = &[("regia", "media-codecs"), ("regia", "media-codecs-asm")];

/// The features a `game` bill resolves on top of each runtime crate's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameFeatures {
    /// Every optional feature of every runtime crate (`--all-features`), the notices set.
    pub all: bool,
    /// `crate/feature` names the game turns on (`--features`), e.g. `regia_steam/steamworks`.
    pub features: Vec<String>,
}

impl GameFeatures {
    /// The `regia:features` property: `all`, or `default` followed by every added feature.
    pub fn describe(&self) -> String {
        if self.all {
            return "all".into();
        }
        let mut added: Vec<String> = GAME_BASE_FEATURES
            .iter()
            .map(|(c, f)| format!("{c}/{f}"))
            .chain(self.features.iter().cloned())
            .collect();
        added.sort();
        added.dedup();
        format!("default + {}", added.join(", "))
    }
}

/// The `cargo tree` feature arguments of runtime crate `package` in a `game` bill: `--all-features`,
/// or `--features` with the umbrella's [`GAME_BASE_FEATURES`] and the game's features for this crate
/// (nothing when there are none, so cargo resolves the defaults).
pub fn game_tree_args(package: &str, selection: &GameFeatures) -> Vec<String> {
    if selection.all {
        return vec!["--all-features".into()];
    }
    let mut features: Vec<&str> = GAME_BASE_FEATURES
        .iter()
        .filter(|(c, _)| *c == package)
        .map(|(_, f)| *f)
        .chain(
            selection
                .features
                .iter()
                .filter_map(|cf| cf.split_once('/'))
                .filter(|(c, _)| *c == package)
                .map(|(_, f)| f),
        )
        .collect();
    features.sort_unstable();
    features.dedup();
    if features.is_empty() {
        Vec::new()
    } else {
        vec!["--features".into(), features.join(",")]
    }
}

/// Checks a feature selection against the profile and its crates: features apply to the `game`
/// profile only, each is `crate/feature` with a crate of the profile, and `all` excludes a list.
pub fn validate_features(profile: Profile, selection: &GameFeatures, crates: &[String]) -> Result<()> {
    if *selection == GameFeatures::default() {
        return Ok(());
    }
    if profile == Profile::Engine {
        anyhow::bail!(
            "--features/--all-features apply to the game profile; the engine bill always lists every \
             optional feature (the notices set, ADR-5556)"
        );
    }
    if selection.all && !selection.features.is_empty() {
        anyhow::bail!("--all-features already turns on every feature; drop --features");
    }
    for feature in &selection.features {
        let Some((krate, name)) = feature.split_once('/') else {
            anyhow::bail!(
                "feature `{feature}` must be written `crate/feature` (e.g. `regia_steam/steamworks`)"
            );
        };
        if name.is_empty() || name.contains('/') {
            anyhow::bail!(
                "feature `{feature}` must be written `crate/feature` (e.g. `regia_steam/steamworks`)"
            );
        }
        if !crates.iter().any(|c| c == krate) {
            anyhow::bail!("feature `{feature}`: `{krate}` is not a runtime crate of the game profile");
        }
    }
    Ok(())
}

/// Collects the inputs from the repository at `root` and returns the pretty-printed document, with
/// the default feature selection.
pub fn run(root: &Path, profile: Profile, with_timestamp: bool) -> Result<String> {
    run_with(root, profile, with_timestamp, &GameFeatures::default())
}

/// [`run`] with the `game` bill's feature selection (`--features`, `--all-features`).
pub fn run_with(
    root: &Path,
    profile: Profile,
    with_timestamp: bool,
    selection: &GameFeatures,
) -> Result<String> {
    let declared = Graph::declared()?;
    let names: Vec<String> = match profile {
        Profile::Engine => declared
            .packages
            .iter()
            .filter(|p| p.workspace && notices::shipped(&p.name))
            .map(|p| p.name.clone())
            .collect(),
        Profile::Game => crate::layers::runtime_members(&declared),
    };
    validate_features(profile, selection, &names)?;
    let trees = names
        .iter()
        .map(|n| match profile {
            Profile::Engine => notices::shipping_tree(root, n),
            Profile::Game => {
                let args = game_tree_args(n, selection);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                Graph::tree_offline_first(root, n, &args)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let features = match profile {
        Profile::Engine => "all".to_string(),
        Profile::Game => selection.describe(),
    };
    let ported = notices::load_ported(root)?;
    let rows = notices::load_assets(root)?;
    let files = notices::tracked_assets(root)?;
    let assets: Vec<(AssetRow, usize)> = rows
        .iter()
        .map(|row| {
            let n = files
                .iter()
                .filter(|f| notices::asset_row(&rows, f).is_some_and(|r| std::ptr::eq(r, row)))
                .count();
            (row.clone(), n)
        })
        .collect();
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).context("reading Cargo.lock")?;
    let checksums = lock_checksums(&lock)?;
    let timestamp = with_timestamp.then(|| {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        iso8601(secs)
    });
    let version = env!("CARGO_PKG_VERSION");
    let doc = build(
        profile,
        &Inputs {
            trees: &trees,
            ported: &ported,
            assets: &assets,
            checksums: &checksums,
            version,
            features: &features,
            timestamp,
        },
    );
    let mut text = serde_json::to_string_pretty(&doc).context("serialising the SBOM")?;
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree with a runtime crate (`regia_core`) and an editor crate (`regia_editor`).
    fn tree(root: &str, deps: &[(&str, &str, &str)]) -> Graph {
        let mut g = Graph::default();
        g.add(root, "0.2.14", true);
        for (name, version, licence) in deps {
            let i = g.add(name, version, false);
            g.packages[i].license = Some((*licence).into());
            g.packages[i].repository = Some(format!("https://example.test/{name}"));
            g.link(0, i, false);
        }
        g
    }

    fn doc(profile: Profile, trees: &[Graph], timestamp: Option<String>) -> Value {
        let checksums = BTreeMap::from([(("glam".to_string(), "0.33.12".to_string()), "ab".repeat(32))]);
        let row = AssetRow {
            path: "fonts/*.ttf".into(),
            origin: "third_party".into(),
            source: "google/fonts".into(),
            licence: "OFL-1.1".into(),
            ships: true,
        };
        let test_row = AssetRow {
            path: "goldens/*.png".into(),
            origin: "generated".into(),
            source: "renders".into(),
            licence: "LicenseRef-Regia-Engine".into(),
            ships: false,
        };
        build(
            profile,
            &Inputs {
                trees,
                ported: &[],
                assets: &[(row, 3), (test_row, 9)],
                checksums: &checksums,
                version: "0.2.14",
                features: "all",
                timestamp,
            },
        )
    }

    fn names(doc: &Value) -> Vec<String> {
        doc["components"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(|c| c["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn ported_source(project: &str, commit: &str, files_to: &[&str]) -> PortedSource {
        PortedSource {
            project: project.into(),
            url: format!("https://example.test/{project}"),
            licence: "MIT".into(),
            copyright: format!("Copyright (c) {project} contributors"),
            commit: commit.into(),
            files_from: vec![format!("src/{project}.rs")],
            files_to: files_to.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn bom_refs(doc: &Value) -> Vec<String> {
        doc["components"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(|c| c["bom-ref"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn ported_bom_refs_are_unique_for_the_same_project_and_commit() {
        // docs/ported-sources.toml holds several (project, commit) pairs with different
        // file sets; CycloneDX bom-refs must stay unique or consumers merge the components and
        // lose the file attribution (review fixrev-03-k finding 3).
        let trees = [tree("regia_core", &[("glam", "0.33.12", "MIT")])];
        let ported = [
            ported_source("Bevy", "fd98063", &["crates/regia_render/a.rs"]),
            ported_source("Bevy", "fd98063", &["crates/regia_ui/b.rs"]),
            ported_source("Fyrox", "0876f67", &["crates/regia_scene/c.rs"]),
        ];
        let checksums = BTreeMap::new();
        let d = build(
            Profile::Engine,
            &Inputs {
                trees: &trees,
                ported: &ported,
                assets: &[],
                checksums: &checksums,
                version: "0.2.14",
                features: "all",
                timestamp: None,
            },
        );
        let refs = bom_refs(&d);
        let mut sorted = refs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(refs.len(), sorted.len(), "bom-refs must be unique: {refs:?}");
        assert!(
            refs.iter().any(|r| r == "ported:Bevy@fd98063"),
            "the first entry keeps the plain ref: {refs:?}"
        );
        assert!(
            refs.iter().any(|r| r == "ported:Bevy@fd98063#2"),
            "the second same-key entry is disambiguated: {refs:?}"
        );
    }

    #[test]
    fn game_profile_omits_editor_only_ported_sources() {
        let trees = [tree("regia_core", &[("glam", "0.33.12", "MIT")])];
        let ported = [
            ported_source("macroquad", "8d602d3", &["crates/regia_2d/sprite.rs"]),
            ported_source("inspector", "abc123", &["crates/regia_editor/panel.rs"]),
            ported_source("xatlas", "def456", &["crates/regia_asset_pipeline/x.rs"]),
        ];
        let checksums = BTreeMap::new();
        for (profile, expect) in [
            (Profile::Engine, vec!["glam", "inspector", "macroquad", "xatlas"]),
            (Profile::Game, vec!["glam", "macroquad"]),
        ] {
            let d = build(
                profile,
                &Inputs {
                    trees: &trees,
                    ported: &ported,
                    assets: &[],
                    checksums: &checksums,
                    version: "0.2.14",
                    features: "all",
                    timestamp: None,
                },
            );
            assert_eq!(names(&d), expect, "{profile:?}");
        }
    }

    #[test]
    fn required_cyclonedx_fields_and_a_known_licence() {
        let trees = [tree("regia_core", &[("glam", "0.33.12", "MIT OR Apache-2.0")])];
        let d = doc(Profile::Engine, &trees, None);
        assert_eq!(d["bomFormat"], "CycloneDX");
        assert_eq!(d["specVersion"], "1.5");
        assert_eq!(d["version"], 1);
        let serial = d["serialNumber"].as_str().unwrap_or_default();
        assert!(
            serial.starts_with("urn:uuid:") && serial.len() == 9 + 36,
            "{serial}"
        );
        assert!(
            d["metadata"].get("timestamp").is_none(),
            "no timestamp unless asked"
        );
        let glam = d["components"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["name"] == "glam"))
            .cloned()
            .unwrap_or_default();
        assert_eq!(glam["type"], "library");
        assert_eq!(glam["version"], "0.33.12");
        assert_eq!(glam["purl"], "pkg:cargo/glam@0.33.12");
        assert_eq!(glam["bom-ref"], "pkg:cargo/glam@0.33.12");
        assert_eq!(glam["licenses"][0]["expression"], "MIT OR Apache-2.0");
        assert_eq!(glam["hashes"][0]["alg"], "SHA-256");
        assert_eq!(glam["externalReferences"][0]["url"], "https://example.test/glam");
        assert_eq!(d["dependencies"][0]["dependsOn"][0], "pkg:cargo/glam@0.33.12");
    }

    #[test]
    fn output_is_deterministic_and_timestamp_is_opt_in() {
        let trees = [tree(
            "regia_core",
            &[("glam", "0.33.12", "MIT"), ("anyhow", "1.0.0", "MIT")],
        )];
        let reversed = [tree(
            "regia_core",
            &[("anyhow", "1.0.0", "MIT"), ("glam", "0.33.12", "MIT")],
        )];
        let a = doc(Profile::Engine, &trees, None);
        assert_eq!(a, doc(Profile::Engine, &trees, None));
        assert_eq!(
            a,
            doc(Profile::Engine, &reversed, None),
            "input order does not matter"
        );
        let order = names(&a);
        assert!(order.iter().position(|n| n == "anyhow") < order.iter().position(|n| n == "glam"));
        assert!(
            a["metadata"]["properties"].as_array().is_some_and(|p| p
                .iter()
                .any(|p| p["name"] == "regia:features" && p["value"] == "all")),
            "the bill names its feature set"
        );
        let stamped = doc(Profile::Engine, &trees, Some("2026-11-05T00:00:00Z".into()));
        assert_eq!(stamped["metadata"]["timestamp"], "2026-11-05T00:00:00Z");
    }

    #[test]
    fn the_game_profile_has_no_editor_crates_and_only_shipped_assets() {
        // The caller picks the trees by profile; the engine bill sees the editor's dependencies,
        // the game bill does not.
        let runtime = tree("regia_core", &[("glam", "0.33.12", "MIT")]);
        let editor = tree("regia_editor", &[("egui_dock", "0.1.0", "MIT")]);
        let engine = doc(Profile::Engine, &[runtime, editor], None);
        assert!(names(&engine).contains(&"egui_dock".to_string()));
        assert!(names(&engine).contains(&"goldens/*.png".to_string()));
        let game = doc(
            Profile::Game,
            &[tree("regia_core", &[("glam", "0.33.12", "MIT")])],
            None,
        );
        assert!(!names(&game).contains(&"egui_dock".to_string()));
        assert!(names(&game).contains(&"fonts/*.ttf".to_string()));
        assert!(
            !names(&game).contains(&"goldens/*.png".to_string()),
            "test assets do not ship"
        );
    }

    #[test]
    fn game_profile_selects_runtime_crates_only() {
        let mut declared = Graph::default();
        for name in [
            "regia_core",
            "regia_render",
            "regia_editor",
            "regia_asset_pipeline",
            "regia_cli",
            "xtask",
        ] {
            declared.add(name, "0.2.14", true);
        }
        let game = crate::layers::runtime_members(&declared);
        assert!(game.contains(&"regia_render".to_string()));
        for banned in ["regia_editor", "regia_asset_pipeline", "regia_cli", "xtask"] {
            assert!(!game.contains(&banned.to_string()), "{banned}");
        }
    }

    #[test]
    fn licence_forms() {
        assert_eq!(licences("MIT/Apache-2.0")[0]["expression"], "MIT OR Apache-2.0");
        assert_eq!(licences("UNKNOWN (see crate)")[0]["license"]["name"], "UNKNOWN");
        assert_eq!(licences("unknown")[0]["license"]["name"], "UNKNOWN");
    }

    #[test]
    fn reads_lock_checksums() {
        let lock = "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\nchecksum = \"abc\"\n\n\
                    [[package]]\nname = \"local\"\nversion = \"0.1.0\"\n";
        let sums = lock_checksums(lock).unwrap_or_default();
        assert_eq!(sums.len(), 1);
        assert_eq!(
            sums.get(&("a".to_string(), "1.0.0".to_string()))
                .map(String::as_str),
            Some("abc")
        );
    }

    /// Review fixrev4 finding 1: the game bill describes what a game build links. A crate behind an
    /// off-by-default feature (`regia_steam/steamworks` -> steamworks-sys, `regia_physics/physics2d`
    /// -> rapier2d, `regia_net/net-quic` -> quinn) is not in it unless the game turns the feature on;
    /// the umbrella's media codecs stay (ADR-2760, the book's profile table).
    #[test]
    fn the_game_bill_lists_the_shipped_feature_set() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let text = run(root, Profile::Game, false).expect("game bill");
        for absent in ["steamworks-sys", "rapier2d", "quinn"] {
            assert!(
                !text.contains(&format!("\"pkg:cargo/{absent}@")),
                "{absent} is behind an off-by-default feature and must not be in the default game bill"
            );
        }
        assert!(
            text.contains("\"pkg:cargo/rav1d@"),
            "the umbrella's media codecs stay listed"
        );
    }

    #[test]
    fn game_tree_args_add_only_the_named_features() {
        let none = GameFeatures::default();
        assert_eq!(
            game_tree_args("regia", &none),
            vec!["--features", "media-codecs,media-codecs-asm"],
            "the umbrella keeps its codecs"
        );
        assert!(game_tree_args("regia_steam", &none).is_empty(), "defaults only");
        let steam = GameFeatures {
            all: false,
            features: vec!["regia_steam/steamworks".into(), "regia/net".into()],
        };
        assert_eq!(
            game_tree_args("regia_steam", &steam),
            vec!["--features", "steamworks"]
        );
        assert_eq!(
            game_tree_args("regia", &steam),
            vec!["--features", "media-codecs,media-codecs-asm,net"]
        );
        assert!(game_tree_args("regia_core", &steam).is_empty());
        let all = GameFeatures {
            all: true,
            features: Vec::new(),
        };
        assert_eq!(game_tree_args("regia_core", &all), vec!["--all-features"]);
        assert_eq!(all.describe(), "all");
        assert_eq!(
            steam.describe(),
            "default + regia/media-codecs, regia/media-codecs-asm, regia/net, regia_steam/steamworks"
        );
    }

    #[test]
    fn feature_selection_is_checked_against_the_profile() {
        let crates = vec!["regia".to_string(), "regia_steam".to_string()];
        let pick = |features: &[&str]| GameFeatures {
            all: false,
            features: features.iter().map(|f| (*f).to_string()).collect(),
        };
        assert!(validate_features(Profile::Engine, &GameFeatures::default(), &crates).is_ok());
        assert!(validate_features(Profile::Game, &pick(&["regia_steam/steamworks"]), &crates).is_ok());
        let err = |profile, selection: &GameFeatures| {
            validate_features(profile, selection, &crates)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        assert!(err(Profile::Engine, &pick(&["regia_steam/steamworks"])).contains("game profile"));
        assert!(err(Profile::Game, &pick(&["steamworks"])).contains("crate/feature"));
        assert!(err(Profile::Game, &pick(&["regia_steam/"])).contains("crate/feature"));
        assert!(err(Profile::Game, &pick(&["regia_editor/x"])).contains("not a runtime crate"));
        let both = GameFeatures {
            all: true,
            features: vec!["regia_steam/steamworks".into()],
        };
        assert!(err(Profile::Game, &both).contains("--all-features"));
    }

    /// A game that turns a feature on gets its crates in the bill, and the bill says which set it
    /// describes.
    #[test]
    fn a_game_feature_adds_its_crates_to_the_bill() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let tree = |selection: &GameFeatures| {
            let args = game_tree_args("regia_steam", selection);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            Graph::tree_offline_first(root, "regia_steam", &args).expect("regia_steam tree")
        };
        let has = |g: &Graph, name: &str| g.packages.iter().any(|p| p.name == name);
        assert!(!has(&tree(&GameFeatures::default()), "steamworks-sys"));
        let steam = GameFeatures {
            all: false,
            features: vec!["regia_steam/steamworks".into()],
        };
        assert!(has(&tree(&steam), "steamworks-sys"));
    }

    #[test]
    fn formats_timestamps() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_782_864_000), "2026-07-01T00:00:00Z");
        assert_eq!(iso8601(951_782_400 + 3_661), "2000-02-29T01:01:01Z");
    }
}
