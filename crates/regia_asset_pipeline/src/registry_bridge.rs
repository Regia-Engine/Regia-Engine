// SPDX-License-Identifier: MPL-2.0
//! Bridge from the cooked asset registry to an [`AssetSet`] (spec 32 § 4.2, ADR-1668), so the
//! validators, the reference graph and the size map serve the editor, the CLI and CI from one code path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regia_asset::registry::AssetRegistry;

use crate::asset_graph::{AssetEntry, AssetKind, AssetSet};
use crate::lod_classes::glob_match;
use crate::validators::COOKED_TYPE_TAG;

/// The `AssetEntry::references` string of a dependency id the registry has no record of.
pub const UNRESOLVED_PREFIX: &str = "unresolved:";

/// Tag of a UI layout entry ([`AssetSet::with_ui_layouts`], ADR-3148): `root` for a top-level
/// `assets/ui/*.ui.ron`, which the runtime loads at start (so it is an orphan-check root), `nested`
/// for one in a subfolder.
pub const UI_LAYOUT_TAG: &str = "ui_layout";

/// Tag carrying why a UI layout did not parse (its references are then unknown).
pub const UI_LAYOUT_ERROR_TAG: &str = "ui_layout_error";

/// A project-relative asset path in manifest form: `/` separators and the `assets/` prefix
/// (`assets://ui/a.png`, `ui/a.png` and `assets/ui/a.png` all name `assets/ui/a.png`).
#[must_use]
pub fn manifest_asset_path(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.strip_prefix("assets://").unwrap_or(&p);
    let p = p.strip_prefix("./").unwrap_or(p);
    if p.starts_with("assets/") {
        p.to_owned()
    } else {
        format!("assets/{p}")
    }
}

/// The asset paths a `.ui.ron` document names for its images and masks (ADR-3148), in manifest form,
/// sorted and deduplicated: the layout's cook dependencies. Icon names and renderer ids are not
/// assets and are not listed.
///
/// # Errors
///
/// The text does not parse as a `.ui.ron` (the loader's message).
pub fn ui_layout_dependencies(text: &str, path: &str) -> Result<Vec<String>, String> {
    let file =
        regia_ui::parse_ui_ron(text, path, &regia_ui::UiMigrators::default()).map_err(|e| e.to_string())?;
    let mut deps: Vec<String> = regia_ui::ui_asset_dependencies(&file)
        .iter()
        .map(|p| manifest_asset_path(p))
        .collect();
    deps.sort();
    deps.dedup();
    Ok(deps)
}

/// The [`AssetKind`] of a cooked type path, from its last `::` segment.
#[must_use]
pub fn kind_of_type_path(type_path: &str) -> AssetKind {
    let last = type_path.rsplit("::").next().unwrap_or(type_path);
    if last.contains("Scene") {
        AssetKind::Scene
    } else if last.contains("Mesh") {
        AssetKind::Mesh
    } else if last.contains("Texture") {
        AssetKind::Texture
    } else if last.contains("Material") {
        AssetKind::Material
    } else if last.contains("Audio") || last.contains("Sound") {
        AssetKind::Audio
    } else {
        AssetKind::Other
    }
}

impl AssetSet {
    /// The set of a cooked registry: ids and paths as recorded, `cooked_bytes` = the registry size,
    /// references = the paths of the dependency ids (`unresolved:<id>` for an id with no record), tags
    /// copied. `texture_size` and `triangles` stay `None` (the registry does not store them).
    /// [`apply_texture_size_evidence`] fills a known nonzero size afterwards and never writes
    /// zero for an unknown measurement, so a missing size still reports `not_validated`.
    /// Roots are the scene assets.
    #[must_use]
    pub fn from_registry(registry: &AssetRegistry) -> Self {
        Self::from_registry_with_roots(registry, &[])
    }

    /// Like [`AssetSet::from_registry`] with the orphan-check roots given as path globs (empty: scenes).
    #[must_use]
    pub fn from_registry_with_roots(registry: &AssetRegistry, root_globs: &[String]) -> Self {
        let mut set = AssetSet::new();
        let bases: BTreeSet<&str> = registry
            .entries()
            .filter_map(|e| e.tags.get("localized_of").map(String::as_str))
            .collect();
        for entry in registry.entries() {
            let references = entry
                .dependencies
                .iter()
                .map(|dep| match registry.by_id(*dep) {
                    Some(target) => target.path.clone(),
                    None => format!("{UNRESOLVED_PREFIX}{dep}"),
                })
                .collect::<Vec<_>>();
            let mut asset = AssetEntry::new(entry.path.clone(), kind_of_type_path(&entry.type_path))
                .with_id(entry.id.to_string())
                .with_references(references)
                .with_cooked_bytes(entry.size);
            asset.tags = entry
                .tags
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>();
            // `AssetKind` is coarse: the twin check needs the exact type of a variant and its base.
            if entry.tags.contains_key("localized_of") || bases.contains(entry.path.as_str()) {
                asset
                    .tags
                    .insert(COOKED_TYPE_TAG.to_owned(), entry.type_path.clone());
            }
            set.push(asset);
        }
        set.assets.sort_by(|a, b| a.path.cmp(&b.path));
        set.apply_root_globs(root_globs);
        set
    }

    /// Set [`AssetSet::roots`] to the asset paths `root_globs` match. Empty globs leave `roots`
    /// unchanged (an empty list still means every scene is a root). A non-empty list that matches
    /// nothing clears `roots`, and the orphan check then falls back to the scene assets, the same
    /// as an empty list. This is the expansion [`AssetSet::from_registry_with_roots`] uses.
    pub fn apply_root_globs(&mut self, root_globs: &[String]) {
        if root_globs.is_empty() {
            return;
        }
        self.roots = self
            .assets
            .iter()
            .filter(|asset| root_globs.iter().any(|glob| glob_match(glob, &asset.path)))
            .map(|asset| asset.path.clone())
            .collect();
    }
}

/// Records a known texture size from lint evidence onto `asset`.
///
/// Both sides must be nonzero and no larger than the decoder edge (`1 << 15`). Unknown,
/// zero, and an edge past that limit do not write. An unknown measurement is not stored
/// as zero, and a size already on `asset` is left in place.
pub fn apply_texture_size_evidence(asset: &mut AssetEntry, width: Option<u32>, height: Option<u32>) {
    const MAX_EDGE: u32 = 1 << 15;
    if let (Some(width), Some(height)) = (width, height)
        && width > 0
        && height > 0
        && width <= MAX_EDGE
        && height <= MAX_EDGE
    {
        asset.texture_size = Some([width, height]);
    }
}

/// Opens a cooked registry for the cook or package gate (ADR-8390).
///
/// A missing file is `Ok(None)` when `required` is false. A missing required file is an error.
/// A file that exists but does not decode is always an error. The error text does not say the
/// check was skipped.
///
/// # Errors
/// The registry is required and missing, or the file exists but cannot be read or decoded.
pub fn open_cook_registry(path: &Path, required: bool) -> Result<Option<AssetRegistry>, String> {
    match AssetRegistry::read_from(path) {
        Ok(None) if required => Err(format!(
            "required asset registry is missing at {}",
            path.display()
        )),
        Ok(value) => Ok(value),
        Err(error) => Err(format!(
            "asset registry at {} is malformed: {error}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validators::{Outcome, ValidationConfig, ValidatorRegistry};
    use regia_asset::AssetId;
    use regia_asset::registry::RegistryEntry;

    fn entry(n: u128, path: &str, ty: &str, deps: &[AssetId], size: u64) -> RegistryEntry {
        RegistryEntry {
            id: AssetId(regia_core::id::Guid::from_u128(n)),
            path: path.to_owned(),
            type_path: ty.to_owned(),
            dependencies: deps.to_vec(),
            tags: BTreeMap::new(),
            size,
        }
    }

    /// A `.ui.ron` joins the set through the one layout indexer ([`AssetSet::with_ui_layouts`],
    /// paths relative to `assets/`) with the asset paths its images and masks name (icons and ids
    /// are not assets) and its component `use` paths: the texture it names is reachable (no orphan),
    /// a picture path no asset matches is `ui_image_missing`, a missing `use` is
    /// `missing_references` (not `ui_image_missing`), and a layout that does not parse is
    /// `ui_layout_invalid` (ADR-3147, ADR-3148).
    #[test]
    fn ui_layouts_join_the_set_and_report_missing_pictures() {
        use crate::validation_rules::ValidationFile;
        let dir = regia_test_utils::unique_temp_dir("ui-layout-set");
        let assets = dir.join("assets");
        let ui = assets.join("ui");
        std::fs::create_dir_all(ui.join("menus")).expect("dirs");
        std::fs::write(
            ui.join("hud.ui.ron"),
            r#"(version: 9, root: (children: [
                (image: (asset: "assets/ui/panel.png")),
                (image: (icon: "gear")),
                (image: (id: 7)),
                (mask: (asset: "ui/gone.png")),
                (use: "ui/parts/missing_bar.ui.ron"),
            ]))"#,
        )
        .expect("hud");
        std::fs::write(
            ui.join("menus").join("broken.ui.ron"),
            "(version: 9, root: (nope: 1))",
        )
        .expect("broken");
        let panel = entry(1, "ui/panel.png", "x::CookedTexture", &[], 64);
        let set = AssetSet::from_registry(&AssetRegistry::from_entries([panel])).with_ui_layouts(&assets);
        let hud = set.get("ui/hud.ui.ron").expect("hud entry");
        let mut refs = hud.references.clone();
        refs.sort();
        assert_eq!(
            refs,
            vec!["ui/gone.png", "ui/panel.png", "ui/parts/missing_bar.ui.ron"]
        );
        assert_eq!(hud.tags.get(UI_LAYOUT_TAG).map(String::as_str), Some("root"));
        let broken = set.get("ui/menus/broken.ui.ron").expect("broken entry");
        assert_eq!(broken.tags.get(UI_LAYOUT_TAG).map(String::as_str), Some("nested"));
        assert!(broken.tags.contains_key(UI_LAYOUT_ERROR_TAG));
        assert_eq!(set.effective_roots(), vec!["ui/hud.ui.ron"]);

        let rep = ValidatorRegistry::with_rules(&ValidationFile::default()).run(&set);
        let row = |v: &str, asset: &str| {
            rep.results
                .iter()
                .find(|r| r.validator == v && r.asset == asset)
                .cloned()
                .unwrap_or_else(|| panic!("no {v} row for {asset}"))
        };
        let missing = row("ui_image_missing", "ui/hud.ui.ron");
        assert_eq!(missing.outcome, Outcome::Fail);
        let codes: Vec<&str> = missing.findings.iter().map(|f| f.code.as_str()).collect();
        assert_eq!(codes, ["ui_image_missing"], "pictures only: {missing:?}");
        assert!(missing.findings[0].text.contains("ui/gone.png"), "{missing:?}");
        let uses = row("missing_references", "ui/hud.ui.ron");
        assert_eq!(uses.outcome, Outcome::Fail, "a missing `use` still fails");
        assert_eq!(
            uses.findings.len(),
            1,
            "the picture is not reported twice: {uses:?}"
        );
        assert!(
            uses.findings[0].text.contains("ui/parts/missing_bar.ui.ron"),
            "{uses:?}"
        );
        assert_eq!(
            row("ui_image_missing", "ui/menus/broken.ui.ron").findings[0].code,
            "ui_layout_invalid"
        );
        assert_eq!(
            row("ui_image_missing", "ui/panel.png").outcome,
            Outcome::NotValidated
        );
        assert_eq!(
            row("orphaned_assets", "ui/panel.png").outcome,
            Outcome::Pass,
            "the layout keeps its picture reachable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_paths_take_the_assets_prefix_once() {
        assert_eq!(manifest_asset_path("ui/a.png"), "assets/ui/a.png");
        assert_eq!(manifest_asset_path("assets/ui/a.png"), "assets/ui/a.png");
        assert_eq!(manifest_asset_path("assets://ui/a.png"), "assets/ui/a.png");
        assert_eq!(manifest_asset_path("ui\\a.png"), "assets/ui/a.png");
    }

    #[test]
    fn kinds_follow_the_type_path() {
        assert_eq!(kind_of_type_path("regia_render::CookedMesh"), AssetKind::Mesh);
        assert_eq!(kind_of_type_path("x::CookedTexture"), AssetKind::Texture);
        assert_eq!(kind_of_type_path("x::SceneAsset"), AssetKind::Scene);
        assert_eq!(kind_of_type_path("x::CookedMaterial"), AssetKind::Material);
        assert_eq!(kind_of_type_path("x::CookedAudio"), AssetKind::Audio);
        assert_eq!(kind_of_type_path("x::Thing"), AssetKind::Other);
    }

    #[test]
    fn a_variant_and_its_base_carry_their_cooked_type() {
        let base = entry(1, "ui/sign.tex", "x::CookedTexture", &[], 10);
        let mut variant = entry(2, "localized/fr/ui/sign.tex", "x::CookedTable", &[], 10);
        variant.tags.insert("localized_of".into(), "ui/sign.tex".into());
        let other = entry(3, "ui/other.tex", "x::CookedTexture", &[], 10);
        let set = AssetSet::from_registry(&AssetRegistry::from_entries([base, variant, other]));
        let ty = |path: &str| set.get(path).and_then(|a| a.tags.get(COOKED_TYPE_TAG)).cloned();
        assert_eq!(ty("ui/sign.tex").as_deref(), Some("x::CookedTexture"));
        assert_eq!(ty("localized/fr/ui/sign.tex").as_deref(), Some("x::CookedTable"));
        assert_eq!(ty("ui/other.tex"), None);
    }

    #[test]
    fn bridge_maps_ids_paths_sizes_and_unresolved_ids() {
        let tex = entry(2, "tex/a.png", "x::CookedTexture", &[], 40);
        let ghost = AssetId(regia_core::id::Guid::from_u128(99));
        let mut mesh = entry(3, "mesh/m.mesh", "x::CookedMesh", &[tex.id, ghost], 100);
        mesh.tags.insert("team".into(), "art".into());
        let scene = entry(1, "scene/s.scene", "x::SceneAsset", &[mesh.id], 7);
        let reg = AssetRegistry::from_entries([scene.clone(), mesh.clone(), tex.clone()]);
        let set = AssetSet::from_registry(&reg);
        assert_eq!(set.assets.len(), 3);
        let m = set.get("mesh/m.mesh").expect("mesh present");
        assert_eq!(m.id.as_deref(), Some(mesh.id.to_string().as_str()));
        assert_eq!(m.cooked_bytes, 100);
        assert_eq!(m.kind, AssetKind::Mesh);
        assert_eq!(m.tags.get("team").map(String::as_str), Some("art"));
        assert!(m.references.contains(&"tex/a.png".to_owned()));
        assert!(m.references.contains(&format!("{UNRESOLVED_PREFIX}{ghost}")));
        assert_eq!(m.triangles, None);
        assert_eq!(set.effective_roots(), vec!["scene/s.scene"]);
        // missing_references fails for the ghost; the triangle budget is not validated.
        let rep = ValidatorRegistry::with_builtins(&ValidationConfig::default()).run(&set);
        let row = |v: &str| {
            rep.results
                .iter()
                .find(|r| r.validator == v && r.asset == "mesh/m.mesh")
                .map(|r| r.outcome)
        };
        assert_eq!(row("missing_references"), Some(Outcome::Fail));
        assert_eq!(row("mesh_triangle_budget"), Some(Outcome::NotValidated));
    }

    #[test]
    fn root_globs_replace_scene_roots() {
        let a = entry(1, "levels/a.scene", "x::SceneAsset", &[], 1);
        let b = entry(2, "ui/b.ui", "x::Other", &[], 1);
        let reg = AssetRegistry::from_entries([a, b]);
        let set = AssetSet::from_registry_with_roots(&reg, &["ui/**".to_owned()]);
        assert_eq!(set.roots, vec!["ui/b.ui"]);
    }
}
