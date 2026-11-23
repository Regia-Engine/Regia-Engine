// SPDX-License-Identifier: MPL-2.0
//! glTF importer settings from `.meta` (`docs/specs/12-assets.md` § 7).

use serde::{Deserialize, Serialize};

use crate::mesh_cook::MeshLodSettings;

/// Parsed glTF importer settings (RON in `.meta` `settings` field).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", default)]
pub struct GltfImporterSettings {
    /// Optional LOD generation block.
    pub generate_lods: Option<GenerateLodsSettings>,
    /// Cook a cluster hierarchy (`.fmlt` sub-asset per mesh primitive) for the
    /// visibility-buffer path (`meshlet_cook`). Off by default: cooking costs seconds per
    /// million triangles.
    pub meshlets: bool,
    /// Drop glTF `doubleSided` on materials that only closed, consistently wound, opaque meshes
    /// use ([`crate::closed_mesh`]): from outside such a mesh its back faces never show, and the
    /// cull-none draws cost GPU time for nothing. Off by default: a camera inside the volume
    /// (a cockpit inside a hull) would see the inside culled.
    pub drop_double_sided_on_closed: bool,
    /// Treat `COLOR_0` alpha as a per-vertex ambient occlusion factor (ADR-1512): the alpha is
    /// clamped to 0..1 (a non-finite value becomes 1) before it is cooked into the mesh. The
    /// material still needs `LabMaterial::vertex_ao` on at runtime to read it as occlusion. Off by
    /// default: a mesh cooks its vertex colors as authored.
    pub vertex_ao: bool,
    /// Joint influences per vertex skinned meshes draw (ADR-1622): 4 (the default; the four
    /// strongest) or 8 (opt-in: a skin with a second influence set, `JOINTS_1`/`WEIGHTS_1`, is
    /// cooked with `skin_influences` 8 and draws all eight on the compute skinning path). The
    /// cooked influence streams are the same either way; any value other than 8 means 4.
    pub skin_influences: u8,
    /// Cook pre-fractured chunks of the mesh (`fracture_cook`, ADR-1967): a mesh per chunk and the
    /// runtime destructible asset. Off by default.
    pub fracture: Option<crate::fracture_cook::FractureCookSettings>,
    /// Collision cooked from each mesh primitive (ADR-2301): `None` (the default) or
    /// `ConvexDecomposition`, which writes a hull-set sub-asset `collision/<node>/primitive_<i>`
    /// per primitive, tuned by [`Self::collision_hulls`].
    pub collision: CollisionImport,
    /// Settings of `collision: ConvexDecomposition`.
    pub collision_hulls: CollisionHullSettings,
    /// Cook a sparse-brick voxel far proxy (`.fvox` sub-asset `voxels/<node>/primitive_<i>`) per
    /// mesh primitive (`voxel_far_cook`, ADR-2445): `voxel_far: Some((resolution: 32))`. The
    /// runtime stacks it on the primitive's cluster DAG past the last cluster LOD. Off (`None`) by
    /// default.
    pub voxel_far: Option<crate::voxel_far_cook::VoxelFarCookSettings>,
    /// Axis bake named in the `.meta` (`docs/formats/gltf-conventions.md`). Omitted means the
    /// caller's [`crate::Forward`]: a cook passes `Convert`, and `import_gltf_path` keeps the
    /// `Forward` it was given. `Keep` skips the 180° yaw.
    pub forward: GltfForward,
    /// Uniform scale baked with that axis into meshes, skins and animation tracks. `1.0` leaves
    /// lengths unchanged. A non-finite or non-positive value warns and is treated as `1.0`.
    pub scale: f32,
    /// Keep every animation clip of the file lossless at cook (`.meta` v2, ADR-7819): each clip is
    /// a `Raw` `cooked-clip` v3 body, bit-exact to the imported clip, instead of the compressed
    /// one. Off by default. FBX and USD sources read it from their `.meta` too.
    pub lossless_clips: bool,
    /// Per-animation clip options (the import dialog, ADR-7904): skip an animation, rename its
    /// clip, or split one take into several clips by time range. An animation no entry names
    /// imports as one clip under its own name. Empty by default.
    pub clips: Vec<ClipImportSettings>,
    /// Cook foot-contact curves `contact.<foot joint>` onto every skinned clip of the file
    /// (ADR-8308, `docs/specs/17-animation.md` § 6): `contact_curves: Some(())` labels the humanoid
    /// feet with the motion-matching defaults; `contact_curves: Some((feet: ["foot_l", "foot_r"]))`
    /// names them. Off (`None`) by default. An additive key: an older importer ignores it (the clip then
    /// has no contact curves), so the `.meta` version stays.
    pub contact_curves: Option<regia_animation::foot_ik::contact::ContactCurveSettings>,
    /// Per-mesh options (the import dialog, ADR-8376): a mesh-bearing node the import leaves out,
    /// named by its mesh sub-asset key (`mesh/<key>/primitive_<i>`, [`gltf_part_keys`]). A node no
    /// entry names imports. Empty by default.
    pub meshes: Vec<MeshImportSettings>,
    /// Per-material options (the import dialog, ADR-8376): draw the model's parts with an extracted
    /// or another project material instead of the embedded one, named by the material sub-asset
    /// key (`material/<key>`). A material no entry names draws embedded. Empty by default.
    pub materials: Vec<MaterialImportSettings>,
}

/// One glTF animation's clip options in the `.meta` settings (`clips: [(animation: "Take 001",
/// ranges: [(name: "run", start: 0.0, end: 1.2)])]`, ADR-7904).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ClipImportSettings {
    /// The animation's name in the file (`anim_<index>` for an unnamed one).
    pub animation: String,
    /// Import no clip from this animation.
    pub skip: bool,
    /// The clip's name; `None` (or empty) keeps the animation's name.
    pub name: Option<String>,
    /// Split the animation into these clips. Empty imports one clip.
    pub ranges: Vec<ClipRangeSettings>,
}

/// One clip cut from an animation by time range (seconds from the animation's start).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ClipRangeSettings {
    /// The clip's name (`<clip>_<index>` when empty).
    pub name: String,
    /// Start, in seconds.
    pub start: f32,
    /// End, in seconds (greater than `start`).
    pub end: f32,
}

impl ClipRangeSettings {
    /// Why the range cannot cut a clip: a non-finite or negative start, or an end not after the
    /// start. `None` for a usable range.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if !(self.start.is_finite() && self.end.is_finite()) || self.start < 0.0 {
            return Some(format!(
                "range `{}`: start and end must be finite seconds, start >= 0",
                self.name
            ));
        }
        (self.end <= self.start).then(|| {
            format!(
                "range `{}`: end {} is not after start {}",
                self.name, self.end, self.start
            )
        })
    }
}

/// Axis bake stored in a glTF `.meta` (`forward: Convert` or `forward: Keep`).
///
/// A separate type from [`crate::Forward`] so the public import argument does not grow a serde
/// impl. [`Self::to_forward`] is the value the importer bakes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum GltfForward {
    /// 180° yaw about +Y (the project default).
    #[default]
    Convert,
    /// Leave the file's axes unchanged.
    Keep,
}

impl GltfForward {
    /// The [`crate::Forward`] this settings value bakes.
    #[must_use]
    pub fn to_forward(self) -> crate::Forward {
        match self {
            Self::Convert => crate::Forward::Convert,
            Self::Keep => crate::Forward::Keep,
        }
    }
}

/// Top-level `.meta` keys [`GltfImporterSettings`] reads. Anything else is ignored, with a warning.
const KNOWN_GLTF_SETTING_KEYS: &[&str] = &[
    "generate_lods",
    "meshlets",
    "drop_double_sided_on_closed",
    "vertex_ao",
    "skin_influences",
    "fracture",
    "collision",
    "collision_hulls",
    "voxel_far",
    "forward",
    "scale",
    "lossless_clips",
    "clips",
    "contact_curves",
    "meshes",
    "materials",
];

/// What collision the importer cooks from the meshes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum CollisionImport {
    /// No cooked collision.
    #[default]
    None,
    /// Convex hulls from approximate convex decomposition.
    ConvexDecomposition,
}

/// Settings of the importer's convex decomposition; the fields are those of
/// `regia_physics::convex_decomposition::DecompositionParams` and share its defaults and ranges.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct CollisionHullSettings {
    /// Most hulls per primitive (1 to 64).
    pub max_hulls: u32,
    /// Voxel cells along the longest axis (8 to 256).
    pub resolution: u32,
    /// Concavity a part may keep, as a fraction of the volume (0 to 1).
    pub max_concavity: f32,
    /// Most cuts to any one part (0 to 16).
    pub max_depth: u32,
    /// Most vertices per hull (0 for no cap, else 4 to 4096).
    pub max_hull_vertices: u32,
}

impl Default for CollisionHullSettings {
    fn default() -> Self {
        let d = regia_physics::convex_decomposition::DecompositionParams::default();
        Self {
            max_hulls: d.max_hulls,
            resolution: d.resolution,
            max_concavity: d.max_concavity,
            max_depth: d.max_depth,
            max_hull_vertices: d.max_hull_vertices,
        }
    }
}

impl Default for GltfImporterSettings {
    fn default() -> Self {
        Self {
            generate_lods: None,
            meshlets: false,
            drop_double_sided_on_closed: false,
            vertex_ao: false,
            skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
            fracture: None,
            collision: CollisionImport::None,
            collision_hulls: CollisionHullSettings::default(),
            voxel_far: None,
            forward: GltfForward::Convert,
            scale: 1.0,
            lossless_clips: false,
            clips: Vec::new(),
            contact_curves: None,
            meshes: Vec::new(),
            materials: Vec::new(),
        }
    }
}

/// Marks a skinned primitive whose skin has a second influence set as drawing all eight
/// (`skin_influences: 8`). A skin with four influences stays at four, with a warning naming
/// `node`; a rigid primitive is untouched.
pub(crate) fn opt_into_eight_influences(mesh: &mut regia_asset::CpuMesh, node: &str) {
    let Some(skin) = mesh.skin.as_mut() else {
        return;
    };
    if skin.joints1.is_empty() {
        tracing::warn!(
            target: "regia_asset_pipeline",
            node,
            "skin_influences: 8, but the skin has no JOINTS_1/WEIGHTS_1; it draws four influences"
        );
        return;
    }
    skin.eight_influences = true;
}

/// Clamps the alpha of every `COLOR_0` entry to 0..1 and maps a non-finite alpha to 1 (the
/// `vertex_ao` importer setting). RGB is untouched.
pub(crate) fn clamp_vertex_ao(colors: &mut [[f32; 4]]) {
    for c in colors {
        c[3] = if c[3].is_finite() {
            c[3].clamp(0.0, 1.0)
        } else {
            1.0
        };
    }
}

/// LOD generation (`specs/12` § 7.2).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct GenerateLodsSettings {
    /// Total LOD levels including LOD0 (max 4).
    pub count: u32,
    /// Optional per-LOD triangle ratios (decreasing; a level that is not coarser than the previous one is skipped).
    #[serde(default)]
    pub ratios: Vec<f32>,
}

impl GltfImporterSettings {
    /// Parse settings from the `.meta` settings string (RON). Missing or blank settings are the
    /// defaults; a malformed string is logged and also gives the defaults.
    pub fn from_meta_string(settings: Option<&str>) -> Self {
        match Self::try_from_meta_string(settings) {
            Ok(parsed) => {
                if let Some(text) = settings.filter(|s| !s.trim().is_empty()) {
                    for key in unknown_setting_keys(text) {
                        tracing::warn!(
                            target: "regia_asset_pipeline",
                            key,
                            "glTF .meta settings: unknown key ignored"
                        );
                    }
                }
                parsed
            }
            Err(e) => {
                tracing::warn!(target: "regia_asset_pipeline", "glTF .meta settings {settings:?}: {e}; using defaults");
                Self::default()
            }
        }
    }

    /// As [`Self::from_meta_string`], for a GLB another importer converted (FBX/OBJ): the settings
    /// string is the SOURCE importer's, which already warned about the keys it does not know, so
    /// keys the glTF importer does not know are dropped silently instead of double-warning on
    /// every cook (corr-low pipeline-a #24b).
    pub fn from_meta_string_converted(settings: Option<&str>) -> Self {
        match Self::try_from_meta_string(settings) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!(target: "regia_asset_pipeline", "converted .meta settings {settings:?}: {e}; using defaults");
                Self::default()
            }
        }
    }

    /// [`Self::from_meta_string`] that reports a malformed string instead of defaulting.
    pub(crate) fn try_from_meta_string(settings: Option<&str>) -> Result<Self, ron::error::SpannedError> {
        match settings.filter(|s| !s.trim().is_empty()) {
            Some(s) => ron::from_str::<Self>(s),
            None => Ok(Self::default()),
        }
    }

    /// The `clips` options as the animation importer's [`crate::gltf_anim::ClipOptions`]. A range
    /// that cannot cut a clip ([`ClipRangeSettings::problem`]) warns and is dropped, so a bad
    /// range never fails the cook of the rest.
    #[must_use]
    pub fn clip_options(&self) -> Vec<crate::gltf_anim::ClipOptions> {
        self.clips
            .iter()
            .map(|clip| crate::gltf_anim::ClipOptions {
                animation: clip.animation.clone(),
                skip: clip.skip,
                name: clip.name.clone().filter(|n| !n.trim().is_empty()),
                ranges: clip
                    .ranges
                    .iter()
                    .filter(|range| match range.problem() {
                        Some(problem) => {
                            tracing::warn!(
                                target: "regia_asset_pipeline",
                                animation = %clip.animation,
                                "glTF .meta clips: {problem}; the range is ignored"
                            );
                            false
                        }
                        None => true,
                    })
                    .map(|range| crate::gltf_anim::ClipRange {
                        name: range.name.clone(),
                        start: range.start,
                        end: range.end,
                    })
                    .collect(),
            })
            .collect()
    }

    /// Whether skinned meshes opt into eight influences (`skin_influences: 8`).
    #[must_use]
    pub fn eight_skin_influences(&self) -> bool {
        self.skin_influences == regia_asset::EIGHT_SKIN_INFLUENCES
    }

    /// Convert to mesh LOD cook settings when present.
    pub fn mesh_lods(&self) -> Option<MeshLodSettings> {
        self.generate_lods.as_ref().map(|g| MeshLodSettings {
            count: g.count.clamp(1, 4),
            ratios: g.ratios.clone(),
        })
    }

    /// The uniform scale to bake. A non-finite or non-positive [`Self::scale`] warns and returns
    /// `1.0` (a negative scale would flip winding; the cook does not treat it as a protocol error).
    #[must_use]
    pub fn import_scale(&self) -> f32 {
        if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            tracing::warn!(
                target: "regia_asset_pipeline",
                scale = self.scale,
                "glTF .meta scale must be finite and > 0; using 1"
            );
            1.0
        }
    }
}

/// The `forward` key when `settings` names one. Omitted, blank or malformed is `None`: the
/// caller's [`crate::Forward`] stands. A present key overrides that argument.
///
/// The value is the raw RON token (`Keep`, not `Some(Keep)`). An `Option` field would require
/// the `Some` wrapper and would reject the settings spelling.
pub(crate) fn explicit_forward(settings: Option<&str>) -> Option<crate::Forward> {
    let text = settings.filter(|s| !s.trim().is_empty())?;
    // A string the importer cannot parse gives the defaults, `forward` included.
    GltfImporterSettings::try_from_meta_string(Some(text)).ok()?;
    let Ok(object) = regia_asset::import_rules::SettingsObject::parse(text) else {
        return None;
    };
    let value = object.fields.into_iter().find(|(name, _)| name == "forward")?.1;
    ron::from_str::<GltfForward>(value.trim())
        .ok()
        .map(GltfForward::to_forward)
}

/// Top-level keys in `settings` that [`GltfImporterSettings`] does not read. A string that is not
/// a RON object returns nothing (the parse warning is [`GltfImporterSettings::from_meta_string`]'s).
pub(crate) fn unknown_setting_keys(settings: &str) -> Vec<String> {
    let Ok(object) = regia_asset::import_rules::SettingsObject::parse(settings) else {
        return Vec::new();
    };
    object
        .fields
        .into_iter()
        .filter_map(|(name, _)| (!KNOWN_GLTF_SETTING_KEYS.contains(&name.as_str())).then_some(name))
        .collect()
}

/// One mesh-bearing node's import option in the `.meta` settings (`meshes: [(mesh: "body", skip:
/// true)]`, ADR-8376).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct MeshImportSettings {
    /// The node's mesh sub-asset key: the `<key>` of its `mesh/<key>/primitive_<i>` sub-assets
    /// (the node name, `~<node index>` for a later node of the same name).
    pub mesh: String,
    /// Leave the node's mesh out of the import: no mesh sub-asset, no model part.
    pub skip: bool,
}

/// One glTF material's option in the `.meta` settings (`materials: [(material: "paint", mode:
/// Replaced(asset: "<id>", path: "assets/materials/red.material.ron"))]`, ADR-8376).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct MaterialImportSettings {
    /// The material's sub-asset key: the `<key>` of its `material/<key>` sub-asset.
    pub material: String,
    /// What the model's parts that use it draw with.
    pub mode: MaterialImportMode,
}

/// What a glTF material's parts draw with (ADR-8376). The embedded material sub-asset is cooked
/// in every mode, so its id and the references to it stay valid; only the model's parts move.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum MaterialImportMode {
    /// The material cooked from the file (`file#material/<key>`).
    #[default]
    Embedded,
    /// A `.material.ron` the import dialog extracted from the embedded material; the parts draw
    /// with that project asset.
    Extracted {
        /// The extracted asset's id (its `.meta` id).
        asset: regia_asset::AssetId,
        /// Its project-relative path (for the dialog; the cook binds by `asset`).
        path: String,
    },
    /// Another project material asset; the parts draw with it.
    Replaced {
        /// The chosen asset's id.
        asset: regia_asset::AssetId,
        /// Its project-relative path (for the dialog; the cook binds by `asset`).
        path: String,
    },
}

impl MaterialImportMode {
    /// The project asset the parts draw with, `None` for the embedded material.
    #[must_use]
    pub fn asset(&self) -> Option<regia_asset::AssetId> {
        match self {
            Self::Embedded => None,
            Self::Extracted { asset, .. } | Self::Replaced { asset, .. } => Some(*asset),
        }
    }
}

/// The per-mesh and per-material options the glTF importer applies (ADR-8376), by sub-asset key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartOptions {
    /// Mesh keys whose node is left out.
    pub skip_meshes: std::collections::BTreeSet<String>,
    /// Material key -> the project material the parts draw with instead.
    pub materials: std::collections::BTreeMap<String, regia_asset::AssetId>,
}

impl GltfImporterSettings {
    /// The `meshes` and `materials` options as the importer's [`PartOptions`].
    #[must_use]
    pub fn part_options(&self) -> PartOptions {
        PartOptions {
            skip_meshes: self
                .meshes
                .iter()
                .filter(|m| m.skip)
                .map(|m| m.mesh.clone())
                .collect(),
            materials: self
                .materials
                .iter()
                .filter_map(|m| m.mode.asset().map(|asset| (m.material.clone(), asset)))
                .collect(),
        }
    }
}

/// The mesh keys a `.meta` settings string skips (`meshes`, ADR-8376), read quietly: a string
/// the glTF settings do not parse skips nothing.
#[must_use]
pub(crate) fn skipped_mesh_keys(settings: Option<&str>) -> std::collections::BTreeSet<String> {
    GltfImporterSettings::try_from_meta_string(settings)
        .map(|s| s.part_options().skip_meshes)
        .unwrap_or_default()
}

/// The mesh sub-asset key of a `mesh/<key>/primitive_<i>` path (`None` for any other path).
#[must_use]
pub fn mesh_key_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("mesh/")?;
    let (key, tail) = rest.rsplit_once('/')?;
    tail.starts_with("primitive_").then_some(key)
}

/// The keys the glTF importer gives a file's mesh-bearing nodes and materials, in its own order
/// (ADR-8376): what the import dialog lists and what `meshes` / `materials` entries name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GltfPartKeys {
    /// `(key, node index, mesh index)` per mesh-bearing node visit, in import order (a node
    /// reached under two parents is listed twice, with two keys, as the importer cooks it).
    pub meshes: Vec<(String, usize, usize)>,
    /// The key of each material, by document index.
    pub materials: Vec<String>,
}

/// The [`GltfPartKeys`] of a `.gltf`/`.glb` (or a converted GLB): the importer's naming rules
/// applied without cooking anything (`crate::gltf_import`'s node walk and material loop).
///
/// # Errors
///
/// The bytes do not parse as glTF.
pub fn gltf_part_keys(bytes: &[u8]) -> Result<GltfPartKeys, String> {
    let gltf = gltf::Gltf::from_slice_without_validation(bytes).map_err(|e| e.to_string())?;
    Ok(crate::gltf_import::part_keys(&gltf.document))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts `WARN` events; a minimal `tracing` subscriber so the test needs no extra
    /// dependency.
    struct WarnCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl tracing::Subscriber for WarnCounter {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// corr-low pipeline-a #24b: the converted path passes the SOURCE importer's settings string
    /// (FBX/OBJ keys): the glTF importer must not re-warn about keys it does not know on every
    /// cook, but it must still honour the keys both importers share.
    #[test]
    fn converted_settings_do_not_rewarn_unknown_keys() {
        // The unknown-key `warn!` callsite caches interest for the whole process. A sibling
        // test can hit it first with no subscriber, and this count then stays at zero.
        // The child installs its subscriber before that callsite is used.
        const CHILD_ENV: &str = "REGIA_ASSET_PIPELINE_CONVERTED_KEYS_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "gltf_settings::tests::converted_settings_do_not_rewarn_unknown_keys",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .output()
                .expect("run isolated unknown-key warning test");
            assert!(
                output.status.success(),
                "isolated unknown-key warning test failed: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "the child must execute the warning assertion: {}",
                String::from_utf8_lossy(&output.stdout),
            );
            return;
        }
        let warns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (converted, plain) = tracing::subscriber::with_default(WarnCounter(warns.clone()), || {
            let converted = GltfImporterSettings::from_meta_string_converted(Some(
                "(import_animation: true, meshlets: true)",
            ));
            // A real .glTF keeps the unknown-key warning: the check stays alive.
            let plain = GltfImporterSettings::from_meta_string(Some("(import_animation: true)"));
            (converted, plain)
        });
        assert!(converted.meshlets, "shared keys are still honoured");
        assert_eq!(
            warns.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly the plain path's unknown-key warning"
        );
        let _ = plain;
    }

    #[test]
    fn a_malformed_settings_string_is_an_error_not_silent_defaults() {
        assert!(GltfImporterSettings::try_from_meta_string(Some("(meshlets: tru)")).is_err());
        assert!(GltfImporterSettings::try_from_meta_string(Some("  ")).is_ok());
        assert!(GltfImporterSettings::try_from_meta_string(None).is_ok());
        // The lenient entry point still cooks with defaults.
        assert!(!GltfImporterSettings::from_meta_string(Some("(meshlets: tru)")).meshlets);
    }

    #[test]
    fn collision_is_off_by_default_and_parses_the_decomposition_block() {
        let none = GltfImporterSettings::from_meta_string(None);
        assert_eq!(none.collision, CollisionImport::None);
        let on = GltfImporterSettings::from_meta_string(Some(
            "(collision: ConvexDecomposition, collision_hulls: (max_hulls: 4, resolution: 32))",
        ));
        assert_eq!(on.collision, CollisionImport::ConvexDecomposition);
        assert_eq!(on.collision_hulls.max_hulls, 4);
        assert_eq!(on.collision_hulls.resolution, 32);
        assert_eq!(
            on.collision_hulls.max_depth,
            CollisionHullSettings::default().max_depth
        );
    }

    #[test]
    fn vertex_ao_is_opt_in_and_clamps_only_the_alpha() {
        assert!(!GltfImporterSettings::from_meta_string(None).vertex_ao);
        assert!(GltfImporterSettings::from_meta_string(Some("(vertex_ao: true)")).vertex_ao);
        let mut colors = [
            [2.0, 0.5, -1.0, 1.5],
            [1.0, 1.0, 1.0, -0.25],
            [0.1, 0.2, 0.3, f32::NAN],
            [1.0; 4],
        ];
        clamp_vertex_ao(&mut colors);
        assert_eq!(colors[0], [2.0, 0.5, -1.0, 1.0]);
        assert_eq!(colors[1], [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(colors[2], [0.1, 0.2, 0.3, 1.0]);
        assert_eq!(colors[3], [1.0; 4]);
    }

    #[test]
    fn eight_skin_influences_are_opt_in() {
        assert_eq!(GltfImporterSettings::default().skin_influences, 4);
        assert_eq!(GltfImporterSettings::from_meta_string(None).skin_influences, 4);
        assert_eq!(
            GltfImporterSettings::from_meta_string(Some("(meshlets: true)")).skin_influences,
            4,
            "absent reads as 4"
        );
        assert!(!GltfImporterSettings::from_meta_string(None).eight_skin_influences());
        assert!(
            !GltfImporterSettings::from_meta_string(Some("(skin_influences: 4)")).eight_skin_influences()
        );
        assert!(
            !GltfImporterSettings::from_meta_string(Some("(skin_influences: 6)")).eight_skin_influences()
        );
        assert!(GltfImporterSettings::from_meta_string(Some("(skin_influences: 8)")).eight_skin_influences());

        let skin = |second: bool| regia_asset::CpuSkin {
            joint_names: (0..8).map(|i| format!("j{i}")).collect(),
            joints: vec![[0, 1, 2, 3]],
            weights: if second {
                vec![[20000, 10000, 5000, 535]]
            } else {
                vec![[65535, 0, 0, 0]]
            },
            joints1: if second { vec![[4, 5, 6, 7]] } else { Vec::new() },
            weights1: if second {
                vec![[10000, 10000, 5000, 5000]]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        let mut eight = regia_asset::CpuMesh {
            skin: Some(skin(true)),
            ..Default::default()
        };
        let before = eight.clone();
        opt_into_eight_influences(&mut eight, "body");
        let after = eight.skin.as_ref().expect("skin");
        assert!(after.eight_influences);
        assert_eq!(
            after.joints1,
            before.skin.as_ref().expect("skin").joints1,
            "streams untouched"
        );
        let mut four = regia_asset::CpuMesh {
            skin: Some(skin(false)),
            ..Default::default()
        };
        opt_into_eight_influences(&mut four, "prop");
        assert!(
            !four.skin.as_ref().expect("skin").eight_influences,
            "no second set: stays at 4"
        );
        let mut rigid = regia_asset::CpuMesh::default();
        opt_into_eight_influences(&mut rigid, "rock");
        assert!(rigid.skin.is_none());
    }

    #[test]
    fn voxel_far_is_opt_in_and_parses() {
        assert!(GltfImporterSettings::from_meta_string(None).voxel_far.is_none());
        assert!(
            GltfImporterSettings::from_meta_string(Some("(meshlets: true)"))
                .voxel_far
                .is_none()
        );
        let on = GltfImporterSettings::from_meta_string(Some(
            "(meshlets: true, voxel_far: Some((resolution: 48)))",
        ));
        let voxel = on.voxel_far.expect("voxel_far block");
        assert_eq!(voxel.resolution, 48);
        assert_eq!(voxel.effective_resolution(), 64, "rounded up to a power of two");
        let defaults = GltfImporterSettings::from_meta_string(Some("(voxel_far: Some(()))"));
        assert_eq!(defaults.voxel_far.map(|v| v.effective_resolution()), Some(32));
        assert_eq!(
            crate::voxel_far_cook::VoxelFarCookSettings { resolution: 100_000 }.effective_resolution(),
            regia_render::visbuffer::MAX_VOXEL_FAR_RESOLUTION
        );
        assert!(
            GltfImporterSettings::try_from_meta_string(Some("(voxel_far: Some((resolution: -1)))")).is_err()
        );
    }

    #[test]
    fn meshlets_are_opt_in() {
        assert!(!GltfImporterSettings::from_meta_string(None).meshlets);
        assert!(!GltfImporterSettings::from_meta_string(Some("(generate_lods: (count: 2))")).meshlets);
        let on = GltfImporterSettings::from_meta_string(Some("(meshlets: true)"));
        assert!(on.meshlets);
        assert!(on.generate_lods.is_none());
    }

    #[test]
    fn dropping_double_sided_on_closed_meshes_is_opt_in() {
        assert!(!GltfImporterSettings::from_meta_string(None).drop_double_sided_on_closed);
        assert!(
            !GltfImporterSettings::from_meta_string(Some("(meshlets: true)")).drop_double_sided_on_closed
        );
        let on = GltfImporterSettings::from_meta_string(Some("(drop_double_sided_on_closed: true)"));
        assert!(on.drop_double_sided_on_closed);
    }

    #[test]
    fn forward_and_scale_parse_and_a_bad_scale_is_one() {
        let omitted = GltfImporterSettings::from_meta_string(Some("(meshlets: true)"));
        assert_eq!(omitted.forward, GltfForward::Convert);
        assert_eq!(omitted.import_scale(), 1.0);
        assert!(explicit_forward(Some("(meshlets: true)")).is_none());
        let kept = GltfImporterSettings::from_meta_string(Some("(forward: Keep, scale: 0.01)"));
        assert_eq!(kept.forward, GltfForward::Keep);
        assert_eq!(
            explicit_forward(Some("(forward: Keep, scale: 0.01)")),
            Some(crate::Forward::Keep)
        );
        assert!((kept.import_scale() - 0.01).abs() < 1e-6);
        assert_eq!(
            explicit_forward(Some("(forward: Convert)")),
            Some(crate::Forward::Convert)
        );
        for bad in ["(scale: 0.0)", "(scale: -2.0)", "(scale: NaN)"] {
            assert_eq!(
                GltfImporterSettings::from_meta_string(Some(bad)).import_scale(),
                1.0,
                "{bad}"
            );
        }
        assert!(explicit_forward(Some("(forward: Keep)")).is_some());
        assert!(explicit_forward(None).is_none());
        assert!(explicit_forward(Some("(meshlets: tru)")).is_none());
    }

    /// Corr-low (pipeline-a #24a): a malformed settings string gives the defaults (logged), so
    /// its `forward` must not be honoured either: `(forward: Keep, meshlets: tru)` baked Keep
    /// while every other key of the same string fell back to the defaults.
    #[test]
    fn corr_low_a_malformed_string_does_not_select_forward() {
        let text = "(forward: Keep, meshlets: tru)";
        assert!(GltfImporterSettings::try_from_meta_string(Some(text)).is_err());
        assert_eq!(
            GltfImporterSettings::from_meta_string(Some(text)).forward,
            GltfForward::Convert
        );
        assert_eq!(explicit_forward(Some(text)), None);
    }

    #[test]
    fn an_unknown_settings_key_is_named_and_known_keys_are_not() {
        assert_eq!(
            unknown_setting_keys("(meshlets: true, not_a_setting: 1)"),
            vec!["not_a_setting".to_string()]
        );
        let known = "(generate_lods: (count: 2), meshlets: true, drop_double_sided_on_closed: true, \
            vertex_ao: true, skin_influences: 8, fracture: None, collision: ConvexDecomposition, \
            collision_hulls: (), voxel_far: None, forward: Keep, scale: 1.0)";
        assert!(
            unknown_setting_keys(known).is_empty(),
            "{:?}",
            unknown_setting_keys(known)
        );
        let parsed = GltfImporterSettings::from_meta_string(Some("(meshlets: true, not_a_setting: 1)"));
        assert!(
            parsed.meshlets,
            "an unknown key does not drop the rest of the settings"
        );
    }

    /// ADR-7904: `clips` is a known key that parses into the animation importer's options; a
    /// range that cannot cut a clip is dropped (with a warning), the rest of the entry stays.
    #[test]
    fn clip_options_parse_and_drop_bad_ranges() {
        let text = "(clips: [(animation: \"Take 001\", name: Some(\"run\"), ranges: [\
            (name: \"start\", start: 0.0, end: 0.4), (name: \"bad\", start: 1.0, end: 0.5)]), \
            (animation: \"idle\", skip: true)])";
        assert!(
            unknown_setting_keys(text).is_empty(),
            "{:?}",
            unknown_setting_keys(text)
        );
        let settings = GltfImporterSettings::try_from_meta_string(Some(text)).expect("parse");
        let options = settings.clip_options();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].animation, "Take 001");
        assert_eq!(options[0].name.as_deref(), Some("run"));
        assert_eq!(options[0].ranges.len(), 1, "the reversed range is dropped");
        assert_eq!(options[0].ranges[0].name, "start");
        assert!(options[1].skip);
        assert!(
            ClipRangeSettings {
                name: "n".into(),
                start: f32::NAN,
                end: 1.0
            }
            .problem()
            .is_some()
        );
        assert!(GltfImporterSettings::default().clip_options().is_empty());
    }

    /// GLB: three mesh nodes over one triangle: `body` (mesh 0, material `paint`), a second
    /// `body` (mesh 1, material `trim`) and `prop` (mesh 1, `trim`).
    fn three_node_glb() -> Vec<u8> {
        let mut bin: Vec<u8> = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        for i in [0u32, 1, 2] {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0,1,2]}],
            "nodes":[{"name":"body","mesh":0},{"name":"body","mesh":1},{"name":"prop","mesh":1}],
            "meshes":[{"name":"m0","primitives":[{"attributes":{"POSITION":0},"indices":1,"material":0}]},
                {"name":"m1","primitives":[{"attributes":{"POSITION":0},"indices":1,"material":1}]}],
            "materials":[{"name":"paint"},{"name":"trim"}],
            "accessors":[
                {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[1.0,1.0,0.0]},
                {"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}],
            "bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":12}],
            "buffers":[{"byteLength":48}]}"#;
        let mut json = json.as_bytes().to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&((12 + 8 + json.len() + 8 + bin.len()) as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    fn asset_id(value: u128) -> regia_asset::AssetId {
        regia_asset::AssetId(regia_core::id::Guid(value))
    }

    fn import_with(settings: Option<&str>) -> crate::GltfImportResult {
        crate::import_gltf_glb_bytes_with_settings(
            &three_node_glb(),
            asset_id(0x5eed),
            crate::Forward::Convert,
            "linux",
            crate::project_texture_quality::default_texture_quality(),
            settings,
        )
        .expect("import")
    }

    fn paths(imported: &crate::GltfImportResult, prefix: &str) -> Vec<String> {
        imported
            .sub_assets
            .iter()
            .filter(|s| s.path.starts_with(prefix))
            .map(|s| s.path.clone())
            .collect()
    }

    fn model(imported: &crate::GltfImportResult) -> regia_asset::CookedModel {
        let sub = imported
            .sub_assets
            .iter()
            .find(|s| s.path == "model")
            .expect("model sub-asset");
        regia_asset::CookedModel::from_postcard(&sub.payload).expect("model")
    }

    /// ADR-8376: the keys the dialog lists are the importer's own (the later `body` carries its
    /// node index; the materials their names).
    #[test]
    fn gltf_part_keys_match_the_imported_mesh_and_material_paths() {
        let keys = gltf_part_keys(&three_node_glb()).expect("keys");
        let listed: Vec<&str> = keys.meshes.iter().map(|(k, _, _)| k.as_str()).collect();
        assert_eq!(listed, ["body", "body~1", "prop"]);
        assert_eq!(keys.materials, ["paint", "trim"]);
        let imported = import_with(None);
        let cooked: Vec<String> = paths(&imported, "mesh/")
            .iter()
            .filter_map(|p| mesh_key_of(p).map(str::to_string))
            .collect();
        assert_eq!(cooked, listed);
        assert_eq!(paths(&imported, "material/"), ["material/paint", "material/trim"]);
    }

    /// P-151 / ADR-8376 (pipeline half of `a_skipped_mesh_is_not_cooked`): a skipped node cooks
    /// no mesh and no model part, and the other `body` keeps its `~1` path (and so its id).
    #[test]
    fn a_skipped_gltf_mesh_is_not_imported_and_no_other_path_moves() {
        let all = import_with(None);
        let skipped = import_with(Some("(meshes: [(mesh: \"body\", skip: true)])"));
        assert_eq!(
            paths(&skipped, "mesh/"),
            ["mesh/body~1/primitive_0", "mesh/prop/primitive_0"]
        );
        let id_of = |imported: &crate::GltfImportResult, path: &str| {
            imported.sub_assets.iter().find(|s| s.path == path).map(|s| s.id)
        };
        assert_eq!(
            id_of(&skipped, "mesh/body~1/primitive_0"),
            id_of(&all, "mesh/body~1/primitive_0")
        );
        assert_eq!(model(&all).parts.len(), 3);
        let parts = model(&skipped).parts;
        assert_eq!(parts.len(), 2);
        assert!(
            parts
                .iter()
                .all(|p| Some(p.mesh) != id_of(&all, "mesh/body/primitive_0"))
        );
        let every = crate::import_gltf_glb_bytes_with_settings(
            &three_node_glb(),
            asset_id(0x5eed),
            crate::Forward::Convert,
            "linux",
            crate::project_texture_quality::default_texture_quality(),
            Some(
                "(meshes: [(mesh: \"body\", skip: true), (mesh: \"body~1\", skip: true), (mesh: \"prop\", skip: true)])",
            ),
        );
        assert!(
            matches!(every, Err(crate::ImportError::NoMesh)),
            "skipping every mesh leaves nothing to import"
        );
    }

    /// ADR-8376 (pipeline half of `a_replaced_material_points_at_the_chosen_asset`): the parts
    /// drawn with `trim` point at the chosen project material; `trim` itself is still cooked and
    /// `paint` is untouched.
    #[test]
    fn a_replaced_gltf_material_rebinds_the_model_parts() {
        let chosen = asset_id(0xabc);
        let text = format!(
            "(materials: [(material: \"trim\", mode: Replaced(asset: \"{chosen}\", path: \"assets/red.material.ron\"))])"
        );
        assert!(
            unknown_setting_keys(&text).is_empty(),
            "{:?}",
            unknown_setting_keys(&text)
        );
        let all = import_with(None);
        let replaced = import_with(Some(&text));
        let trim = |imported: &crate::GltfImportResult| {
            imported
                .sub_assets
                .iter()
                .find(|s| s.path == "material/trim")
                .map(|s| s.id)
        };
        assert!(trim(&replaced).is_some(), "the embedded material is still cooked");
        let before: Vec<_> = model(&all).parts.iter().map(|p| p.material).collect();
        let after: Vec<_> = model(&replaced).parts.iter().map(|p| p.material).collect();
        assert_eq!(before[0], after[0], "paint is untouched");
        assert_eq!(before[1], trim(&all));
        assert_eq!(after[1..], [Some(chosen), Some(chosen)]);
        let sub = replaced
            .sub_assets
            .iter()
            .find(|s| s.path == "model")
            .expect("model");
        assert!(
            sub.dependencies.contains(&chosen),
            "the model depends on the chosen material"
        );
    }

    /// ADR-8376: `meshes` and `materials` are known keys; the modes parse and name their asset.
    #[test]
    fn mesh_and_material_options_parse() {
        let id = asset_id(7);
        let text = format!(
            "(meshes: [(mesh: \"a\", skip: true), (mesh: \"b\")], materials: [(material: \"m\", mode: Extracted(asset: \"{id}\", path: \"assets/m.material.ron\")), (material: \"n\")])"
        );
        assert!(unknown_setting_keys(&text).is_empty());
        let settings = GltfImporterSettings::try_from_meta_string(Some(&text)).expect("parse");
        let options = settings.part_options();
        assert_eq!(options.skip_meshes.iter().collect::<Vec<_>>(), ["a"]);
        assert_eq!(options.materials.get("m"), Some(&id));
        assert!(!options.materials.contains_key("n"), "embedded binds nothing");
        assert_eq!(skipped_mesh_keys(Some(&text)).len(), 1);
        assert!(skipped_mesh_keys(Some("(not ron")).is_empty());
        assert_eq!(mesh_key_of("mesh/a~3/primitive_12"), Some("a~3"));
        assert_eq!(mesh_key_of("material/a"), None);
    }
}
