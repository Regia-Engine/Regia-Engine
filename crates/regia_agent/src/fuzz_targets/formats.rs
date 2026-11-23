// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for content formats: every frozen format the agent crate can reach, plus decals and data
//! tables. Each fixture is a checked-in example (`docs/formats/`, a crate's test fixtures, or the format
//! doc's example extracted into `fixtures/`) or a value the format's own code builds.

use std::fmt::Display;

use regia_ecs::prelude::{PartialReflect, Reflect, TypeRegistry};
use regia_ecs::reflect::TypePath;
use regia_ecs::serialize::ReflectCodec;

use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

/// The cooked form of the `data-table` fixture.
/// A GPU-ready mesh payload: a quad, two LODs, two submeshes, and the version 3 trailing block (an
/// eight-influence skin and a sparse morph target, ADR-10080).
fn gpu_mesh_fixture() -> Vec<u8> {
    let info = regia_asset::GpuMeshInfo {
        aabb_min: [0.0; 3],
        aabb_max: [1.0, 1.0, 0.0],
        bounding_sphere: [0.5, 0.5, 0.0, 0.75],
        lods: vec![
            regia_asset::GpuMeshLod {
                screen_relative_size: 1.0,
                first_index: 0,
                index_count: 6,
            },
            regia_asset::GpuMeshLod {
                screen_relative_size: 0.5,
                first_index: 0,
                index_count: 3,
            },
        ],
    };
    let submeshes = [
        regia_asset::GpuMeshSubmesh {
            first_index: 0,
            index_count: 3,
            material_slot: 0,
        },
        regia_asset::GpuMeshSubmesh {
            first_index: 3,
            index_count: 3,
            material_slot: 1,
        },
    ];
    let skin = regia_asset::CpuSkin {
        joint_names: vec!["root".into(), "arm".into(), "hand".into()],
        joints: vec![[0, 1, 2, 0]; 4],
        weights: vec![[30000, 10000, 5000, 5000]; 4],
        joints1: vec![[2, 1, 0, 2]; 4],
        weights1: vec![[10000, 5000, 535, 0]; 4],
        eight_influences: true,
    }
    .pack(4);
    let morph = regia_asset::CpuMorphTarget {
        name: "smile".into(),
        vertex_indices: vec![1, 3],
        position_deltas: vec![[0.0, 0.125, 0.0], [0.5, -0.25, 1.0]],
        normal_deltas: vec![[0.0, 0.0, 0.5]; 2],
        tangent_deltas: Vec::new(),
    }
    .pack(4);
    let (Ok(skin), Ok(morph)) = (skin, morph) else {
        return Vec::new();
    };
    let extras = regia_asset::GpuMeshExtras {
        skin: Some(skin),
        skin_influences: regia_asset::EIGHT_SKIN_INFLUENCES,
        morph_targets: vec![morph],
    };
    let vertices = vec![0u8; regia_asset::GPU_VERTEX_STRIDE as usize * 4];
    regia_asset::encode_gpu_mesh_parts(
        &regia_asset::GpuMeshParts {
            info: &info,
            submeshes: &submeshes,
            vertices: &vertices,
            indices: &[0, 1, 2, 0, 2, 3],
            extras: Some(&extras),
        },
        0,
    )
    .unwrap_or_default()
}

/// A GPU-ready mesh payload: parsed and checked (every LOD and submesh index against the vertex count, the
/// trailing block unpacked as `to_cpu_mesh` does), then encoded again from what it holds.
fn gpu_mesh_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let view = regia_asset::GpuMeshView::parse(b).map_err(e)?;
    for lod in 0..view.info.lods.len().max(1) {
        view.lod_indices(lod).map_err(e)?;
    }
    for sub in 0..view.submeshes.len() {
        view.submesh_indices(sub).map_err(e)?;
    }
    view.to_cpu_mesh().map_err(e)?;
    let indices: Vec<u32> = view
        .indices
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    regia_asset::encode_gpu_mesh_parts(
        &regia_asset::GpuMeshParts {
            info: &view.info,
            submeshes: &view.submeshes,
            vertices: view.vertices,
            indices: &indices,
            extras: view.extras.as_ref(),
        },
        0,
    )
    .map_err(e)
}

/// A cooked sound payload: a measured one-second tone and a few stand-in source bytes.
fn cooked_sound_fixture() -> Vec<u8> {
    let samples: Vec<f32> = (0..4800 * 2)
        .map(|i| ((i / 2) as f32 * 0.05).sin() * 0.25)
        .collect();
    regia_audio::SoundLoudness::measure_interleaved_stereo(&samples, 48_000)
        .ok()
        .and_then(|loudness| {
            regia_audio::cooked_sound::CookedSound {
                loudness,
                data: b"RIFF-fixture".to_vec(),
            }
            .to_bytes()
            .ok()
        })
        .unwrap_or_default()
}

fn data_table_cooked_fixture() -> Vec<u8> {
    regia_asset::data_table::CookedDataTable::from_text(
        include_str!("fixtures/data-table.example.table.ron"),
        regia_asset::data_table::TableFormat::Ron,
    )
    .ok()
    .and_then(|t| t.to_bytes().ok())
    .unwrap_or_default()
}

/// A cooked data table: decoded with its structure checked, turned into the untyped table a typed load
/// starts from, then encoded again.
fn data_table_cooked_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let table = regia_asset::data_table::CookedDataTable::from_bytes(b).map_err(e)?;
    let raw = table.to_raw(&regia_asset::data_table::CsvImport::default());
    if raw.rows.len() != table.len() {
        return Err(format!(
            "{}{} rows decoded as {}",
            super::BUG_PREFIX,
            table.len(),
            raw.rows.len()
        ));
    }
    table.to_bytes()
}

pub(super) const fn text(
    name: &'static str,
    format: Option<&'static str>,
    format_version: u32,
    round_trip: bool,
    fixture: fn() -> String,
    load: fn(&str) -> Result<String, String>,
) -> FuzzTarget {
    FuzzTarget {
        name,
        format,
        format_version,
        round_trip,
        input: FuzzInput::Text { fixture, load },
    }
}

/// Canonical RON for a format whose crate has no writer of its own.
pub(super) fn pretty<T: serde::Serialize>(value: &T) -> Result<String, String> {
    ron::ser::to_string_pretty(value, regia_core::ron_pretty()).map_err(e)
}

/// The RON a level stores for reflected component `value`.
pub(super) fn reflected_fixture<T: PartialReflect>(register: fn(&mut TypeRegistry), value: &T) -> String {
    let mut registry = TypeRegistry::default();
    register(&mut registry);
    ReflectCodec::new(&registry)
        .to_ron(value as &dyn PartialReflect)
        .unwrap_or_default()
}

/// The scene loader's path for a reflected component: decode under its type path, refuse a newer `format_version`
/// with the check `spawn_scene` runs, then `consume` it (what the owning crate does with it), then encode again.
pub(super) fn reflected_load<T: Reflect + TypePath>(
    s: &str,
    register: fn(&mut TypeRegistry),
    consume: fn(&mut T) -> Result<(), String>,
) -> Result<String, String> {
    let mut registry = TypeRegistry::default();
    register(&mut registry);
    let mut codec = ReflectCodec::new(&registry);
    let value = codec.from_ron(s).map_err(e)?.into_value().ok_or("no value")?;
    let mut value = value
        .downcast::<T>()
        .map_err(|_| format!("not a {}", T::short_type_path()))?;
    let registration = registry
        .get(std::any::TypeId::of::<T>())
        .ok_or_else(|| format!("{}{} is not registered", super::BUG_PREFIX, T::short_type_path()))?;
    regia_scene::spawn::check_format_version(registration, value.as_partial_reflect())?;
    consume(&mut value)?;
    codec.to_ron(&*value as &dyn PartialReflect).map_err(e)
}

/// The format targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        // regia_asset
        text(
            "meta",
            Some("meta"),
            regia_asset::META_FORMAT_VERSION,
            true,
            || include_str!("fixtures/meta.example.ron").into(),
            |s| {
                regia_asset::MetaSidecar::from_ron(s.as_bytes())
                    .and_then(|m| m.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "material",
            Some("material"),
            regia_asset::MATERIAL_FORMAT_VERSION,
            true,
            || include_str!("fixtures/material-v3.example.ron").into(),
            |s| {
                regia_asset::MaterialAssetRon::from_ron(s.as_bytes())
                    .and_then(|m| m.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "glasscrack",
            Some("glasscrack"),
            regia_asset::glasscrack::GLASSCRACK_FORMAT_VERSION,
            true,
            || include_str!("fixtures/glasscrack.example.ron").into(),
            |s| {
                regia_asset::glasscrack::GlassCrackSet::from_ron(s)
                    .and_then(|g| g.to_ron())
                    .map_err(e)
            },
        ),
        // The asset id manifest `.assetids.ron` (ADR-8460): read, rules checked, written again.
        text(
            "asset-ids",
            Some("asset-ids"),
            regia_asset::asset_ids::ASSET_IDS_FORMAT_VERSION,
            true,
            || regia_asset::asset_ids::fixture_manifest().to_ron(),
            |s| {
                regia_asset::asset_ids::AssetIdsManifest::from_ron(s)
                    .map(|m| m.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "data-table",
            Some("data-table"),
            regia_asset::data_table::DATA_TABLE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/data-table.example.table.ron").into(),
            |s| {
                regia_asset::data_table::read_raw_ron(s)
                    .map(|t| t.to_ron())
                    .map_err(|issue| format!("{issue:?}"))
            },
        ),
        // Their cooked (pak) form (ADR-2132): version word plus postcard, decoded and encoded again.
        FuzzTarget {
            name: "data-table.cooked",
            format: Some("data-table-cooked"),
            format_version: regia_asset::data_table::DATA_TABLE_COOKED_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: data_table_cooked_fixture,
                load: data_table_cooked_load,
            },
        },
        // The GPU-ready mesh a desktop pak holds (`gpu-mesh` v3, ADR-2788, ADR-10080): parsed, then written again
        // from what it holds. The fixture has two LODs, two submeshes and the skin and morph block.
        FuzzTarget {
            name: "gpu-mesh",
            format: Some("gpu-mesh"),
            format_version: regia_asset::GPU_MESH_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: gpu_mesh_fixture,
                load: gpu_mesh_load,
            },
        },
        // The cooked sound a pak holds (`cooked-sound` v1, ADR-2788): source bytes plus loudness record.
        FuzzTarget {
            name: "cooked-sound",
            format: Some("cooked-sound"),
            format_version: regia_audio::cooked_sound::COOKED_SOUND_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: cooked_sound_fixture,
                load: |b| {
                    regia_audio::cooked_sound::CookedSound::from_bytes(b)
                        .map_err(e)
                        .and_then(|sound| sound.to_bytes().map_err(e))
                },
            },
        },
        // regia_scene
        text(
            "scene",
            Some("scene"),
            regia_scene::format::SCENE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/level.example.ron").into(),
            |s| {
                regia_scene::format::SceneFile::from_ron_str(s)
                    .and_then(|f| f.to_ron_string())
                    .map_err(e)
            },
        ),
        // A baked level (scene format v11, ADR-2313): `look.lightmap` and saved receivers, through
        // the scene reader, the settings reader and the receiver's reflected decode.
        text(
            "scene.lightmap",
            None,
            regia_scene::format::SCENE_FORMAT_VERSION,
            true,
            || LIGHTMAP_LEVEL_SEED.into(),
            lightmap_level_load,
        ),
        // regia_render (reflected components)
        text(
            "primitive",
            Some("primitive"),
            regia_render::primitives::PRIMITIVE_FORMAT_VERSION,
            true,
            primitive_fixture,
            primitive_load,
        ),
        text(
            "post-volume",
            Some("post-volume"),
            regia_render::post_volumes::POST_VOLUME_FORMAT_VERSION,
            true,
            post_volume_fixture,
            post_volume_load,
        ),
        // regia_ui
        text(
            "ui",
            Some("ui"),
            regia_ui::UI_RON_VERSION,
            false,
            || include_str!("fixtures/ui.example.ui.ron").into(),
            |s| {
                regia_ui::parse_ui_ron(s, "fuzz.ui.ron", &regia_ui::UiMigrators::default())
                    .map(|_| String::new())
                    .map_err(e)
            },
        ),
        // The newest node fields: group opacity, clip shape and masks (ADR-2866).
        text(
            "ui-group",
            None,
            regia_ui::UI_RON_VERSION,
            false,
            || include_str!("fixtures/ui.v7.example.ui.ron").into(),
            |s| {
                regia_ui::parse_ui_ron(s, "fuzz.ui.ron", &regia_ui::UiMigrators::default())
                    .map(|_| String::new())
                    .map_err(e)
            },
        ),
        // Components (ADR-3147): the declaration checks and an expansion whose every `use` loads the
        // document itself, so cycles, depth, unknown parameters and slots all run on mutated input.
        text(
            "ui-component",
            None,
            regia_ui::UI_RON_VERSION,
            false,
            || include_str!("fixtures/ui.components.example.ui.ron").into(),
            |s| {
                let migrators = regia_ui::UiMigrators::default();
                let file = regia_ui::ui_component::parse_component(s, "ui/fuzz.ui.ron", &migrators)
                    .map_err(|f| format!("{f:?}"))?;
                let me = std::sync::Arc::new(file.clone());
                let mut load = |_: &str| -> Result<
                    std::sync::Arc<regia_ui::UiFile>,
                    regia_ui::ui_component::LoadFailure,
                > { Ok(me.clone()) };
                let expansion = regia_ui::ui_component::expand(&file, "ui/fuzz.ui.ron", &mut load);
                Ok(format!("{} diagnostics", expansion.diagnostics.len()))
            },
        ),
        // Pictures named by asset path or icon, nine-slice insets (ADR-3148): the loader, then the
        // reference walk the cook and the designer read.
        text(
            "ui-picture",
            None,
            regia_ui::UI_RON_VERSION,
            false,
            || include_str!("fixtures/ui.v9.example.ui.ron").into(),
            |s| {
                regia_ui::parse_ui_ron(s, "fuzz.ui.ron", &regia_ui::UiMigrators::default())
                    .map(|file| {
                        let refs = regia_ui::ui_image_references(&file);
                        let deps = regia_ui::ui_asset_dependencies(&file);
                        format!("{} refs, {} deps", refs.len(), deps.len())
                    })
                    .map_err(e)
            },
        ),
        // The v11 containers, markup and focus-neighbors fold (ADR-7827): the loader, a write and
        // re-read that must give the same document, the markup spans of every text, and a split's
        // runtime ratio that stays finite and inside its bounds.
        text(
            "ui-parity",
            None,
            regia_ui::UI_RON_VERSION,
            false,
            || include_str!("fixtures/ui.v11-parity.example.ui.ron").into(),
            |s| {
                let migrators = regia_ui::UiMigrators::default();
                let file = regia_ui::parse_ui_ron(s, "fuzz.ui.ron", &migrators).map_err(e)?;
                let mut stack = vec![&file.root];
                let mut spans = 0usize;
                while let Some(node) = stack.pop() {
                    if let Some(text) = &node.text {
                        spans += regia_ui::authoring::text_def_spans(text).len();
                    }
                    if let Some(regia_ui::authoring::WidgetDef::Split(d)) = &node.widget {
                        let split = d.to_split();
                        if !split.ratio.is_finite() || !(0.0..=1.0).contains(&split.ratio) {
                            return Err(format!(
                                "{}split ratio {} from {d:?}",
                                super::BUG_PREFIX,
                                split.ratio
                            ));
                        }
                    }
                    stack.extend(node.children.iter());
                }
                // The writer's output reads back (reported as a rejection, not a crash: the round
                // trip itself is pinned by regia_ui's tests on the fixture).
                if let Ok(written) = regia_ui::write_ui_ron(&file) {
                    regia_ui::parse_ui_ron(&written, "fuzz.ui.ron", &migrators)
                        .map_err(|err| format!("written document does not read back: {err}"))?;
                }
                Ok(format!("{spans} spans"))
            },
        ),
        text(
            "uisettings",
            Some("uisettings"),
            regia_ui::UI_SETTINGS_VERSION,
            false,
            || include_str!("fixtures/ui.example.uisettings.ron").into(),
            |s| {
                regia_ui::parse_ui_settings_ron(s, "fuzz.uisettings.ron")
                    .map(|_| String::new())
                    .map_err(e)
            },
        ),
        // The button-prompt glyph table (ADR-7070).
        text(
            "glyphs",
            Some("glyphs"),
            regia_ui::INPUT_GLYPHS_VERSION,
            false,
            || include_str!("fixtures/input.example.glyphs.ron").into(),
            |s| {
                regia_ui::InputGlyphTable::from_ron(s)
                    .map(|t| format!("{} actions", t.glyphs.len()))
                    .map_err(e)
            },
        ),
        // Item templates, converters and the comment-keeping merge of a designer save (ADR-2489).
        text(
            "ui-repeat",
            None,
            regia_ui::UI_RON_VERSION,
            false,
            || include_str!("fixtures/ui.v6.example.ui.ron").into(),
            |s| {
                regia_ui::ron_merge::fuzz_check(s).map_err(|failure| match failure {
                    regia_ui::ron_merge::FuzzFailure::Rejected(reason) => reason,
                    regia_ui::ron_merge::FuzzFailure::Bug(reason) => {
                        format!("{}{reason}", super::BUG_PREFIX)
                    }
                })
            },
        ),
        // The v11 fold (ADR-7751): theme-token and animation bindings, a repeat's row height from
        // the template, and image `fit: tile`. Every binding builds its runtime form and samples its
        // animation at hostile clocks; every image tiles a small and a huge rect.
        text(
            "ui-designer",
            None,
            regia_ui::UI_RON_VERSION,
            false,
            || include_str!("fixtures/ui.v11-designer.example.ui.ron").into(),
            |s| {
                let file =
                    regia_ui::parse_ui_ron(s, "fuzz.ui.ron", &regia_ui::UiMigrators::default()).map_err(e)?;
                let mut stack = vec![&file.root];
                let mut seen = 0usize;
                while let Some(node) = stack.pop() {
                    for bind in node.bind.iter().chain(&node.bindings) {
                        let runtime = bind.to_bind();
                        if let regia_ui::BindSource::Animation(animation) = runtime.source {
                            for t in [0.0, 0.25, 1e9, f32::MAX, f32::INFINITY, f32::NAN, -1.0] {
                                let v = animation.sample(t);
                                if !v.is_finite() {
                                    return Err(format!("{}animation sample {v} at {t}", super::BUG_PREFIX));
                                }
                                let _ = animation.running_at(t);
                            }
                        }
                    }
                    if node
                        .repeat
                        .as_ref()
                        .is_some_and(|r| r.row_height == regia_ui::authoring::RepeatRowHeight::Template)
                        && regia_ui::authoring::RepeatDef::template_row_height(&node.children)
                            .is_some_and(|h| !(h.is_finite() && h > 0.0))
                    {
                        return Err(format!("{}template row height not positive", super::BUG_PREFIX));
                    }
                    if let Some(image) = &node.image {
                        let fit = image.effective_fit();
                        for rect in [[0.0, 0.0, 64.0, 64.0], [0.0, 0.0, 1e7, 1e7]] {
                            let tiles =
                                regia_ui::extract::image_tiles(rect, [0.0, 0.0, 1.0, 1.0], Some([1, 1]), 1.0);
                            if tiles.len() > regia_ui::extract::MAX_IMAGE_TILES {
                                return Err(format!(
                                    "{}{} tiles for {fit:?}",
                                    super::BUG_PREFIX,
                                    tiles.len()
                                ));
                            }
                        }
                    }
                    seen += 1;
                    stack.extend(node.children.iter());
                }
                Ok(format!("{seen} nodes"))
            },
        ),
        text(
            "theme",
            Some("theme"),
            regia_ui::THEME_FORMAT_VERSION,
            false,
            || include_str!("fixtures/theme.example.theme.ron").into(),
            |s| {
                regia_ui::parse_theme_ron(s, "fuzz.theme.ron")
                    .map(|_| String::new())
                    .map_err(e)
            },
        ),
        // A v2 theme with a `focus_visible` layer and style rules (ADR-8105 (e)), loaded over the
        // built-ins so mutations also reach rule resolution (the `theme` fixture is v1).
        text(
            "theme-rules",
            Some("theme"),
            regia_ui::THEME_FORMAT_VERSION,
            false,
            || include_str!("fixtures/theme-rules.example.theme.ron").into(),
            |s| {
                let mut set = regia_ui::theme_resolve::builtin_theme_set();
                let name = set.load_ron(s, "fuzz.theme.ron").map_err(e)?;
                let resolved = set.resolve(&name, None).map_err(e)?;
                Ok(format!("{} rules", resolved.rules.len()))
            },
        ),
        // regia_vfx
        text(
            "vfx",
            Some("vfx"),
            regia_vfx::VFX_FORMAT_VERSION,
            true,
            || include_str!("fixtures/muzzle_flash.vfx.ron").into(),
            |s| {
                regia_vfx::VfxSystemDef::from_ron(s)
                    .and_then(|v| v.to_ron())
                    .map_err(e)
            },
        ),
        // A v8 effect with authored fixed bounds (`system.bounds`, the v8 fold of ADR-8105 row (h)):
        // the loader's box checks, then the culling volume and the measured bounds the editor reads
        // (P-225), which must be finite boxes whenever they exist.
        text(
            "vfx-bounds",
            Some("vfx"),
            regia_vfx::VFX_FORMAT_VERSION,
            true,
            || include_str!("fixtures/vfx-bounds.example.ron").into(),
            vfx_bounds_load,
        ),
        text(
            "decal",
            Some("decal"),
            regia_vfx::DECAL_FORMAT_VERSION,
            true,
            || include_str!("fixtures/decal.example.ron").into(),
            |s| {
                regia_vfx::DecalPrefab::from_ron(s)
                    .and_then(|d| d.to_ron())
                    .map_err(e)
            },
        ),
        // regia_animation
        text(
            "animgraph",
            Some("animgraph"),
            regia_animation::ANIMGRAPH_FORMAT_VERSION,
            true,
            || include_str!("fixtures/locomotion.animgraph.ron").into(),
            |s| {
                regia_animation::AnimGraph::from_ron_str(s)
                    .and_then(|g| g.to_ron_string())
                    .map_err(e)
            },
        ),
        // A v4 graph with both iterative-IK nodes (ADR-8475): per-joint cone and hinge limits, a
        // pole, shares and a uniform limit, so mutations reach the v4 node bodies (the
        // `animgraph` fixture has none).
        text(
            "animgraph-ik",
            Some("animgraph"),
            regia_animation::ANIMGRAPH_FORMAT_VERSION,
            true,
            || include_str!("fixtures/chain_ik.animgraph.ron").into(),
            |s| {
                regia_animation::AnimGraph::from_ron_str(s)
                    .and_then(|g| g.to_ron_string())
                    .map_err(e)
            },
        ),
        // A v4 graph with both locomotion warp nodes (ADR-8540): a `Pose` stride warp with legs,
        // pelvis and full settings under an orientation warp with spine shares.
        text(
            "animgraph-warp",
            Some("animgraph"),
            regia_animation::ANIMGRAPH_FORMAT_VERSION,
            true,
            || include_str!("fixtures/locomotion_warp.animgraph.ron").into(),
            |s| {
                regia_animation::AnimGraph::from_ron_str(s)
                    .and_then(|g| g.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "animedit",
            Some("animedit"),
            regia_animation::ANIMEDIT_FORMAT_VERSION,
            true,
            || include_str!("fixtures/animedit-ron.ron").into(),
            |s| {
                regia_animation::AnimEditDocument::from_ron_str(s)
                    .and_then(|d| d.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "spring",
            Some("spring"),
            regia_animation::SPRING_FORMAT_VERSION,
            true,
            || include_str!("fixtures/spring-ron.ron").into(),
            |s| {
                regia_animation::SpringBoneSetup::from_ron_str(s)
                    .and_then(|d| d.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "drivers",
            Some("drivers"),
            regia_animation::DRIVERS_FORMAT_VERSION,
            true,
            || include_str!("fixtures/arm.drivers.ron").into(),
            |s| {
                regia_animation::DriverSet::from_ron_str(s)
                    .and_then(|d| d.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "skelprofile",
            Some("skelprofile"),
            regia_animation::SKELPROFILE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/skelprofile-ron.ron").into(),
            |s| {
                regia_animation::SkeletonProfile::from_ron_str(s)
                    .and_then(|d| d.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "montage",
            Some("montage"),
            regia_animation::MONTAGE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/montage-ron.ron").into(),
            |s| {
                regia_animation::Montage::from_ron_str(s)
                    .and_then(|d| d.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "sync",
            Some("sync"),
            regia_animation::SYNC_FORMAT_VERSION,
            true,
            || include_str!("fixtures/sync-ron.ron").into(),
            |s| {
                regia_animation::SyncSetup::from_ron_str(s)
                    .and_then(|d| d.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "montage-variant",
            Some("montage-variant"),
            regia_animation::MONTAGE_VARIANT_FORMAT_VERSION,
            true,
            || include_str!("fixtures/montage-variant-ron.ron").into(),
            |s| {
                regia_animation::MontageVariant::from_ron_str(s)
                    .and_then(|d| d.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "slots",
            Some("slots"),
            regia_animation::SLOTS_FORMAT_VERSION,
            true,
            || include_str!("fixtures/slots-ron.ron").into(),
            |s| {
                regia_animation::SkeletonSlots::from_ron_str(s)
                    .and_then(|d| d.to_ron_string())
                    .map_err(e)
            },
        ),
        // regia_audio
        text(
            "cue",
            Some("cue"),
            regia_audio::CUE_VERSION,
            true,
            || include_str!("fixtures/impact_flesh.cue.ron").into(),
            // `migrate_ron`, not from_ron + to_ron: a loader's output is this build's canonical
            // form of the file, so it must be stamped at the current version (a v1 file that only
            // round-trips keeps `version: 1` and would pass as "current" mislabeled).
            |s| regia_audio::SoundCueDef::migrate_ron(s).map_err(e),
        ),
        // `.ambience.ron` v1: the example of docs/formats/ambience-ron.md (ADR-2721).
        text(
            "ambience",
            Some("ambience"),
            regia_audio::AMBIENCE_VERSION,
            true,
            || include_str!("fixtures/glade.ambience.ron").into(),
            |s| {
                regia_audio::AmbienceDef::from_ron(s)
                    .and_then(|d| d.to_ron())
                    .map_err(e)
            },
        ),
        // `.music.ron` v2: the example of docs/formats/music-ron.md.
        text(
            "music",
            Some("music"),
            regia_audio::MUSIC_VERSION,
            true,
            || include_str!("fixtures/combat.music.ron").into(),
            // `migrate_ron`, not from_ron + to_ron: the output must be stamped at the current
            // version (a v1 file that only round-trips keeps no version and would pass
            // mislabeled).
            |s| regia_audio::MusicDef::migrate_ron(s).map_err(e),
        ),
        text(
            "audiopatch",
            Some("audiopatch"),
            regia_audio::PATCH_VERSION,
            true,
            // A v1 example: the loader migrates it to v2 (ADR-7226) and writes the v2 text.
            || include_str!("fixtures/hover_engine.audiopatch.ron").into(),
            |s| {
                regia_audio::AudioPatchDef::from_ron(s)
                    .and_then(|def| def.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "loopgen",
            Some("loopgen"),
            regia_audio::LOOPGEN_VERSION,
            true,
            || include_str!("fixtures/loopgen.example.ron").into(),
            |s| {
                let def = regia_audio::LoopGenDef::parse(s).map_err(e)?;
                ron::ser::to_string_pretty(&def, regia_core::ron_pretty()).map_err(e)
            },
        ),
        text(
            "voice-chat",
            Some("voice-chat"),
            regia_audio::voice_chat::CONFIG_FORMAT_VERSION,
            true,
            || include_str!("fixtures/voice-chat-ron.ron").into(),
            |s| {
                regia_audio::voice_chat::VoiceChatSettings::from_ron(s)
                    .and_then(|v| v.to_ron())
                    .map_err(e)
            },
        ),
        // regia_terrain
        text(
            "terrain-layers",
            Some("terrain-layers"),
            regia_terrain::TERRAIN_LAYERS_FORMAT_VERSION,
            true,
            || include_str!("fixtures/ground.terrain.layers.ron").into(),
            |s| {
                regia_terrain::TerrainLayers::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "terrain-holes",
            Some("terrain-holes"),
            regia_terrain::HOLES_FORMAT_VERSION,
            true,
            holes_fixture,
            |s| {
                regia_terrain::HolesMask::from_holes_ron(s)
                    .and_then(|h| h.to_holes_ron())
                    .map_err(e)
            },
        ),
        // Indoor cells and portals (`<map>.viscells.ron`, ADR-7586).
        text(
            "viscells",
            Some("viscells"),
            regia_render::viscells::VISCELLS_FORMAT_VERSION,
            true,
            viscells_fixture,
            |s| {
                regia_render::viscells::VisCells::from_ron(s)
                    .and_then(|c| c.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "scatter",
            Some("scatter"),
            regia_terrain::SCATTER_FORMAT_VERSION,
            true,
            || include_str!("fixtures/rocks.scatter.ron").into(),
            |s| {
                regia_terrain::ScatterSet::from_ron(s, None)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "grass",
            Some("grass"),
            regia_terrain::GRASS_FORMAT_VERSION,
            true,
            || include_str!("fixtures/grass-ron.ron").into(),
            |s| {
                regia_terrain::GrassSet::from_ron(s, None)
                    .and_then(|g| g.to_ron())
                    .map_err(e)
            },
        ),
        // regia_matgraph
        text(
            "matgraph",
            Some("matgraph"),
            regia_matgraph::MATGRAPH_FORMAT_VERSION,
            true,
            || include_str!("fixtures/matgraph.example.ron").into(),
            matgraph_load,
        ),
        // A v3 foliage graph: the `Wind` node, object position and time driving a vertex offset,
        // so mutations reach the v3 node and the vertex stage (the `matgraph` fixture is v1).
        text(
            "matgraph-wind",
            Some("matgraph"),
            regia_matgraph::MATGRAPH_FORMAT_VERSION,
            true,
            || include_str!("fixtures/matgraph-wind.example.ron").into(),
            matgraph_load,
        ),
        // A v5 graph with parameter metadata (ADR-8105 (b)): categories, tooltips and ranges
        // (a stepped Float, a Color), so mutations reach the loader's range checks.
        text(
            "matgraph-params",
            Some("matgraph"),
            regia_matgraph::MATGRAPH_FORMAT_VERSION,
            true,
            || include_str!("fixtures/matgraph-params.example.ron").into(),
            matgraph_load,
        ),
        text(
            "matinst",
            Some("matinst"),
            regia_matgraph::MATINST_FORMAT_VERSION,
            true,
            || include_str!("fixtures/matinst.example.ron").into(),
            |s| {
                regia_matgraph::MaterialInstance::from_ron(s)
                    .and_then(|g| g.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "matfunc",
            Some("matfunc"),
            regia_matgraph::MATFUNC_FORMAT_VERSION,
            true,
            matfunc_fixture,
            |s| {
                regia_matgraph::MaterialFunction::from_ron(s)
                    .and_then(|g| g.to_ron())
                    .map_err(e)
            },
        ),
        // regia_2d
        text(
            "tilemap",
            Some("tilemap"),
            regia_2d::TILEMAP_FORMAT_VERSION,
            true,
            || include_str!("fixtures/tilemap.example.ron").into(),
            |s| {
                regia_2d::TileMapAsset::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "atlas",
            Some("atlas"),
            regia_2d::ATLAS_FORMAT_VERSION,
            true,
            || include_str!("fixtures/atlas.example.ron").into(),
            |s| {
                regia_2d::AtlasSource::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        // regia_framework
        text(
            "shots",
            Some("shots"),
            regia_framework::shots::SHOT_SEQUENCE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/shots.example.ron").into(),
            |s| {
                regia_framework::shots::ShotSequence::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "sequence",
            Some("sequence"),
            regia_framework::sequencer::SEQUENCE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/seq.example.ron").into(),
            |s| {
                regia_framework::sequencer::Sequence::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        // A v6 sequence with event tracks, a locked track and a muted event track (ADR-8105 row
        // (c)), so mutations reach the v6 shapes (`seq.example.ron` stays the player target's).
        text(
            "sequence-events",
            Some("sequence"),
            regia_framework::sequencer::SEQUENCE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/seq-events.example.ron").into(),
            |s| {
                regia_framework::sequencer::Sequence::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "tags",
            Some("tags"),
            regia_framework::tags::TAG_TABLE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/tags.example.ron").into(),
            |s| {
                regia_framework::tags::TagTable::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "camera",
            Some("camera"),
            regia_framework::camera::asset::CAMERA_FORMAT_VERSION,
            true,
            || include_str!("fixtures/camera-ron.ron").into(),
            |s| {
                regia_framework::camera::asset::CameraAsset::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        // regia_ai
        text(
            "logic",
            Some("logic"),
            regia_ai::logic::LOGIC_FORMAT_VERSION,
            true,
            || include_str!("fixtures/logic.example.ron").into(),
            logic_load,
        ),
        // `.logic.ron` v2 typed entity and world variables (ADR-8900), so mutations reach the
        // `entity_vars` / `world_vars` lists and the declared-type check (`logic.example.ron` stays
        // the copy of the docs example).
        text(
            "logic-vars",
            Some("logic"),
            regia_ai::logic::LOGIC_FORMAT_VERSION,
            true,
            || include_str!("fixtures/logic-vars.example.ron").into(),
            logic_load,
        ),
        text(
            "statetree",
            Some("statetree"),
            regia_ai::state_tree::STATE_TREE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/statetree.example.ron").into(),
            statetree_load,
        ),
        text(
            "smartobject",
            Some("smartobject"),
            regia_ai::smart_object_def::SMART_OBJECT_DEF_VERSION,
            true,
            || include_str!("fixtures/smartobject-ron.ron").into(),
            |s| {
                regia_ai::SmartObjectDef::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "utility",
            Some("utility"),
            regia_ai::utility::UTILITY_AI_FORMAT_VERSION,
            true,
            || include_str!("fixtures/utility.example.ron").into(),
            utility_load,
        ),
        // regia_physics
        text(
            "destructible",
            Some("destructible"),
            regia_physics::destruction::DESTRUCTIBLE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/destructible.ron").into(),
            |s| {
                regia_physics::destruction::DestructibleAsset::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        // `.destructible.ron` v2 (ADR-2734): structural load, falling sections and a bond limit,
        // so the new fields see mutated input too (the fixture above is v1 and goes through the
        // migrator).
        text(
            "destructible.load",
            Some("destructible"),
            regia_physics::destruction::DESTRUCTIBLE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/destructible-load.ron").into(),
            |s| {
                regia_physics::destruction::DestructibleAsset::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        // `.destructionfx.ron` v1 (ADR-2734).
        text(
            "destructionfx",
            Some("destructionfx"),
            regia_framework::destruction_fx::DESTRUCTION_FX_FORMAT_VERSION,
            true,
            || include_str!("fixtures/destructionfx.ron").into(),
            |s| {
                regia_framework::destruction_fx::DestructionFxTable::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        // regia_agent
        text(
            "render-job",
            Some("render-job"),
            crate::render_job::RENDER_JOB_FORMAT_VERSION,
            true,
            || include_str!("fixtures/render-job.example.ron").into(),
            |s| {
                crate::render_job::RenderJob::from_ron(s)
                    .and_then(|t| t.to_ron())
                    .map_err(e)
            },
        ),
        FuzzTarget {
            name: "lightmap",
            format: Some("lightmap"),
            format_version: regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: || include_bytes!("fixtures/lightmap-v1.flmap").to_vec(),
                load: |b| {
                    regia_asset::lightmap::Lightmap::from_bytes(b)
                        .and_then(|l| l.to_bytes())
                        .map_err(e)
                },
            },
        },
        // `.flmap` version 5 (ADR-2314): BC6H blocks and a denoise record, so the block decoder and
        // the new header field see mutated input too.
        FuzzTarget {
            name: "lightmap.blocks",
            format: Some("lightmap"),
            format_version: regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: lightmap_blocks_fixture,
                load: |b| {
                    regia_asset::lightmap::Lightmap::from_bytes(b)
                        .and_then(|l| l.to_bytes())
                        .map_err(e)
                },
            },
        },
        // `.flmap` version 7 (ADR-3300): two atlas pages with directions, so the page count, the
        // entry pages and the per-page sections see mutated input too.
        FuzzTarget {
            name: "lightmap.pages",
            format: Some("lightmap"),
            format_version: regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: lightmap_pages_fixture,
                load: |b| {
                    regia_asset::lightmap::Lightmap::from_bytes(b)
                        .and_then(|l| l.to_bytes())
                        .map_err(e)
                },
            },
        },
        // The `voice_chat` section of the user settings: a hand-written reader that keeps unknown
        // keys as raw RON (ADR-2608), so mutated input must never panic and must reach a fixed
        // point after one save.
        text(
            "voice_chat.user_settings",
            None,
            0,
            true,
            voice_chat_user_fixture,
            |s| {
                let user: regia_audio::voice_chat::VoiceChatUserSettings = ron::from_str(s).map_err(e)?;
                pretty(&user)
            },
        ),
        // Formation slot lists (`.formation.ron` v1, ADR-8358).
        text(
            "ai.formation",
            Some("ai-formation"),
            regia_ai::FORMATION_FORMAT_VERSION,
            true,
            formation_fixture,
            |s| {
                let file = regia_ai::FormationFile::from_ron(s).map_err(|err| err.to_string())?;
                Ok(file.to_ron())
            },
        ),
        // A deterministic utility catalogue (`.detutility.ron` v1, gap AI-7): parse, validate, write the
        // canonical text (a fixed point after one save).
        text(
            "ai.det_utility",
            Some(regia_ai::det::DET_UTILITY_FORMAT),
            regia_ai::det::DET_UTILITY_FORMAT_VERSION,
            true,
            det_utility_fixture,
            |s| {
                let catalogue =
                    regia_ai::det::UtilityCatalogue::from_ron_str(s).map_err(|err| err.to_string())?;
                Ok(catalogue.to_ron())
            },
        ),
        // Gait settings (`.gait.ron` v1, gap G7): untrusted text is parsed, version-checked and validated
        // with the stepper's own checks, then written back canonically (a fixed point after one save).
        text(
            "anim.gait",
            Some(regia_animation::gait_asset::GAIT_FORMAT),
            regia_animation::gait_asset::GAIT_FORMAT_VERSION,
            true,
            gait_fixture,
            |s| {
                let asset =
                    regia_animation::gait_asset::GaitAsset::from_ron(s).map_err(|err| err.to_string())?;
                asset.to_ron().map_err(|err| err.to_string())
            },
        ),
    ]
}

/// A six-leg tripod walker at format version 1 with every required block, the shared reach limit, a ground band,
/// a body joint and per-leg hip offsets (the format corpus file).
fn gait_fixture() -> String {
    include_str!("../../tests/fixtures/format-corpus/gait/v1.ron").to_string()
}

/// A two-option, three-input catalogue at format version 1 with every curve field and both consideration kinds.
fn det_utility_fixture() -> String {
    r#"(
    format_version: 1,
    name: "fuzz",
    inputs: [
        (name: "hull"),
        (name: "threat"),
        (name: "ammo"),
    ],
    options: [
        (
            name: "push",
            min_score: 0.0,
            considerations: [
                (input: "threat", curve: (kind: Logistic, m: 12.0, k: 0.5, b: 0.0, c: 0.0), weight: 1.0, multiplier: false),
                (input: "ammo", curve: (kind: Plateau, k: 0.2), multiplier: true),
            ],
        ),
        (
            name: "retreat",
            min_score: 0.05,
            considerations: [
                (input: "hull", curve: (kind: InverseLogistic, m: 8.0, k: 0.3), weight: 0.8),
                (input: "threat", curve: (kind: Band, b: 0.6, c: 0.2), weight: 0.2),
            ],
        ),
    ],
)
"#
    .to_string()
}

/// A two-slot formation at format version 1.
fn formation_fixture() -> String {
    "(\n    format_version: 1,\n    name: \"wedge-custom\",\n    spacing_scale: 1.0,\n    fill_order: [\n        \"left\",\n        \"right\",\n    ],\n    slots: [\n        (\n            name: \"left\",\n            offset: (x: -1.0, y: 0.0, z: 2.0),\n            role: Some(\"flanker\"),\n        ),\n        (\n            name: \"right\",\n            offset: (x: 1.0, y: 0.0, z: 2.0),\n            role: None,\n        ),\n    ],\n)\n"
        .to_string()
}

/// A `voice_chat` user settings section with every known key and unknown keys of several shapes.
fn voice_chat_user_fixture() -> String {
    r#"(
    consent: true,
    device: Some("USB microphone"),
    mode: Vad,
    input_gain_db: 3.0,
    output_gain_db: -2.0,
    muted_accounts: ([(account: "acct-1", note: "loud")]),
    blocked_accounts: ([]),
    deafen: false,
    future_level: Fancy(level: 3, tags: ["a", "b"]),
    future_list: [1, 2, 3], // a comment
)"#
    .into()
}

/// The `lightmap.blocks` fixture: the version 1 fixture with a denoise record and BC6H texels,
/// written at the current version (empty if the fixture ever stopped loading, which the fuzz test
/// then reports).
fn lightmap_blocks_fixture() -> Vec<u8> {
    use regia_asset::lightmap::{Lightmap, LightmapDenoiseInfo, LightmapTexelEncoding};
    Lightmap::from_bytes(include_bytes!("fixtures/lightmap-v1.flmap"))
        .ok()
        .and_then(|mut map| {
            map.denoise = Some(LightmapDenoiseInfo::default());
            map.encode_texels(LightmapTexelEncoding::Bc6h).ok()?;
            map.to_bytes().ok()
        })
        .unwrap_or_default()
}

/// The `lightmap.pages` fixture (ADR-3300): the version 1 fixture's entries on two pages (the
/// second page a copy of the first) with directions, written at the current version (empty if the
/// fixture ever stopped loading, which the fuzz test then reports).
fn lightmap_pages_fixture() -> Vec<u8> {
    use regia_asset::lightmap::Lightmap;
    Lightmap::from_bytes(include_bytes!("fixtures/lightmap-v1.flmap"))
        .ok()
        .and_then(|mut map| {
            let texels = map.texels.clone();
            let coverage = map.coverage.clone();
            let last = map.entries.len().checked_sub(1)?;
            map.entries[last].page = 1;
            map.texels.extend(texels);
            map.coverage.extend(coverage);
            let dirs = vec![[0.0, 1.0, 0.0, 0.5]; map.total_texels()];
            map.set_directions(&dirs).ok()?;
            map.to_bytes().ok()
        })
        .unwrap_or_default()
}

/// A logic graph goes past the parse (spec 102 § 13 P1.11): compiled with only the built-in `logic::*`
/// tasks (the game registers its own; the fuzz build has none, so other actions are dropped first), so
/// the limits, names, scopes, event arms, conditions and built-in parameters run on the mutated input.
fn logic_load(s: &str) -> Result<String, String> {
    let def = regia_ai::LogicGraphDef::from_ron(s).map_err(e)?;
    let mut compiled = def.clone();
    for rule in &mut compiled.rules {
        rule.actions
            .retain(|a| regia_ai::logic::BUILTIN_TASKS.contains(&a.task.as_str()));
    }
    regia_ai::LogicGraph::compile(&compiled, &regia_ai::BtTaskRegistry::default()).map_err(e)?;
    def.to_ron().map_err(e)
}

/// A state tree goes past the parse: compiled with includes resolved to nothing. The task paths are cleared
/// first (the game registers its task types; the fuzz build has none), so everything else the compiler checks
/// (names, parents, reachability, conditions, weights, transitions) runs on the mutated input. Include expansion
/// runs too, on its own (spec 38 § 8 test 10), in the bounded passes of [`statetree_include_passes`], so the
/// cycle check, the prefixing, the nesting limit and the state-count check see mutated input; its result is not
/// part of the output.
fn statetree_load(s: &str) -> Result<String, String> {
    let def = regia_ai::StateTreeDef::from_ron(s).map_err(e)?;
    if def.has_includes() {
        let _ = statetree_include_passes(&def, s);
    }
    let mut compiled = def.clone();
    compiled.includes.clear();
    compiled.ambient.clear();
    for state in &mut compiled.states {
        state.tasks.clear();
    }
    regia_ai::StateTree::compile_with(&compiled, &regia_ai::BtTaskRegistry::default(), &|_| None)
        .map_err(e)?;
    def.to_ron().map_err(e)
}

/// What include expansion did with one state tree input: how many include texts were loaded and,
/// per pass, the expanded state count or the error.
#[derive(Debug)]
struct IncludeProbe {
    loads: usize,
    outcomes: Vec<Result<usize, regia_ai::state_tree::StateTreeError>>,
}

/// Most include texts one expansion pass of the state tree fuzz target loads. A pass that needs
/// more sees its next include as missing, so an input with many includes per level (which would
/// fan out as `n + n^2 + ...` loads up to the nesting limit) stays bounded.
const STATETREE_INCLUDE_LOADS: usize = 64;
/// Expansion passes the state tree fuzz target runs on an input with includes: the input as its own
/// include, then depth-keyed passes that stop at depth 1, stop at the nesting limit, and never stop.
const STATETREE_INCLUDE_PASSES: usize = 4;
/// The include paths a depth-keyed pass writes: `fuzz-include/<depth>/<index>`.
const STATETREE_INCLUDE_TAG: &str = "fuzz-include/";

/// Runs include expansion on `def` (parsed from `s`) in [`STATETREE_INCLUDE_PASSES`] passes, each
/// within [`STATETREE_INCLUDE_LOADS`] loads:
/// 1. every include path loads `s` itself, so the cycle check sees the mutated paths (the nested
///    level's first include is already on the stack);
/// 2. depth-keyed passes: each level's includes are renamed `fuzz-include/<depth>/<index>` and load
///    the mutated definition again, so no false cycle stops them. With the includes of the level at
///    depth `leaf` removed the expansion can succeed (prefixing and the state-count check after
///    each merge run on mutated input, at depth 1 and at the nesting limit); with none removed it
///    reaches `IncludeDepth`.
fn statetree_include_passes(def: &regia_ai::StateTreeDef, s: &str) -> IncludeProbe {
    use std::cell::Cell;
    let mut probe = IncludeProbe {
        loads: 0,
        outcomes: Vec::with_capacity(STATETREE_INCLUDE_PASSES),
    };
    let budget = |loads: &Cell<usize>| {
        let granted = loads.get() < STATETREE_INCLUDE_LOADS;
        if granted {
            loads.set(loads.get() + 1);
        }
        granted
    };

    let loads = Cell::new(0usize);
    let outcome = def.expand_includes(&|_| budget(&loads).then(|| s.to_string()));
    probe.loads += loads.get();
    probe.outcomes.push(outcome.map(|expanded| expanded.states.len()));

    for leaf in [Some(1), Some(regia_ai::state_tree::MAX_INCLUDE_DEPTH), None] {
        let level = |depth: usize| {
            let mut level = def.clone();
            if leaf.is_some_and(|leaf| depth >= leaf) {
                level.includes.clear();
            } else {
                for (index, include) in level.includes.iter_mut().enumerate() {
                    include.path = format!("{STATETREE_INCLUDE_TAG}{}/{index}", depth + 1);
                }
            }
            level
        };
        let loads = Cell::new(0usize);
        let load = |path: &str| {
            let depth = path
                .strip_prefix(STATETREE_INCLUDE_TAG)?
                .split_once('/')?
                .0
                .parse::<usize>()
                .ok()?;
            if !budget(&loads) {
                return None;
            }
            level(depth).to_ron().ok()
        };
        let outcome = level(0).expand_includes(&load);
        probe.loads += loads.get();
        probe.outcomes.push(outcome.map(|expanded| expanded.states.len()));
    }
    debug_assert_eq!(probe.outcomes.len(), STATETREE_INCLUDE_PASSES);
    probe
}

/// A utility definition goes past the parse: the reasoner's validation runs on the mutated input, and an
/// explain over an empty blackboard scores whatever passed.
fn utility_load(s: &str) -> Result<String, String> {
    let def = regia_ai::UtilityAiDef::from_ron(s).map_err(e)?;
    let reasoner = regia_ai::Reasoner::new(&def).map_err(e)?;
    let _ = reasoner.explain(&regia_ai::Blackboard::new(), 0.0);
    def.to_ron().map_err(e)
}

/// The `vfx-bounds` target: load, then every bounds reading of the result (ADR-8355). A
/// non-finite box from a definition the loader accepted is a bug.
fn vfx_bounds_load(s: &str) -> Result<String, String> {
    let def = regia_vfx::VfxSystemDef::from_ron(s).map_err(|err| err.to_string())?;
    let axes = (
        regia_core::math::Vec3::X,
        regia_core::math::Vec3::Y,
        regia_core::math::Vec3::Z,
    );
    let volume = regia_vfx::bounds::culling_volume(&def, regia_core::math::Vec3::ZERO, axes, 1.0);
    if !(volume.center().is_finite() && volume.radius().is_finite()) {
        return Err(format!(
            "{}culling volume not finite: {volume:?}",
            super::BUG_PREFIX
        ));
    }
    let settings = regia_vfx::probe::ProbeSettings {
        event_capacity: 256,
        child_capacity: 256,
        ..regia_vfx::probe::ProbeSettings::default()
    };
    // A hostile budget or rate must not make one input simulate millions of particles.
    let mut measured_def = def.clone();
    measured_def.system.budget = measured_def.system.budget.min(512);
    let measured = regia_vfx::bounds::measured_bounds(
        &measured_def,
        regia_core::math::Vec3::ZERO,
        regia_core::math::Vec3::Z,
        settings,
        12,
    );
    for b in measured.emitters.iter().flatten() {
        if !(b.min.is_finite() && b.max.is_finite()) {
            return Err(format!("{}measured bounds not finite: {b:?}", super::BUG_PREFIX));
        }
    }
    def.to_ron().map_err(|err| err.to_string())
}

/// A material graph goes past the parse: compiled to WGSL and composed into the host shaders, which naga
/// validates. Generated code that fails validation is a compiler bug, reported with [`super::BUG_PREFIX`].
fn matgraph_load(s: &str) -> Result<String, String> {
    let graph = regia_matgraph::MaterialGraph::from_ron(s).map_err(e)?;
    if let Err(errors) = regia_matgraph::compile::compile(&graph) {
        let text = errors.to_string();
        if text.contains("compiler bug") {
            return Err(format!("{}{text}", super::BUG_PREFIX));
        }
    }
    graph.to_ron().map_err(e)
}

fn holes_fixture() -> String {
    regia_terrain::HolesMask::default()
        .to_holes_ron()
        .unwrap_or_default()
}

/// The `<map>.viscells.ron` seed: two rooms, a doorway between them and a window to outdoors, so
/// mutations start from every field and the portal checks.
fn viscells_fixture() -> String {
    regia_render::viscells::VisCells::from_ron(
        "(version: 1, cells: [
            (id: 0, name: \"outdoor\", outdoor: true, bounds: []),
            (id: 1, name: \"room_a\", bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0), rotation_radians: 0.25)]),
            (id: 2, name: \"room_b\", bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))]),
        ], portals: [
            (a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)]),
            (a: 1, b: 0, quad: [(-1.0, 0.5, 4.0), (1.0, 0.5, 4.0), (1.0, 2.0, 4.0), (-1.0, 2.0, 4.0)]),
        ])",
    )
    .and_then(|c| c.to_ron())
    .unwrap_or_default()
}

/// The `.matfunc.ron` seed: one input feeding a `Noise` node (the v2 addition), so mutations
/// start from the current shape.
fn matfunc_fixture() -> String {
    regia_matgraph::MaterialFunction {
        format_version: regia_matgraph::MATFUNC_FORMAT_VERSION,
        name: "fuzz".into(),
        inputs: vec![regia_matgraph::FunctionInput {
            name: "p".into(),
            ty: regia_matgraph::ValueType::Vec3,
        }],
        nodes: [
            ("in".to_string(), regia_matgraph::Node::Input("p".into())),
            (
                "n".to_string(),
                regia_matgraph::Node::Noise {
                    input: "in".into(),
                    octaves: 3,
                },
            ),
        ]
        .into_iter()
        .collect(),
        outputs: [("out".to_string(), "n".to_string())].into_iter().collect(),
    }
    .to_ron()
    .unwrap_or_default()
}

/// The seed of `scene.lightmap`: a level naming its lightmap, with a two-part mesh whose owner
/// saved a receiver (ADR-2313).
const LIGHTMAP_LEVEL_SEED: &str = r#"SceneFile(
    format_version: 11,
    entities: [
        (
            guid: "00000000-0000-0000-0000-0000000000a1",
            name: Some("Hall"),
            components: {
                "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)),
                "regia_render::runtime::MeshSource": (mesh: "assets/meshes/hall.gltf", material: ""),
                "regia_scene::components::GiMobility": Static,
                "regia_render::lightmap::LightmapReceiver": (entry: "entity_00000000-0000-0000-0000-0000000000a1_0"),
            },
        ),
    ],
    resources: {
        "regia_scene::level::LevelSettings": (look: (lightmap: Some((file: "maps/hall.flmap", enabled: None, intensity: Some(1.5))))),
    },
)
"#;

/// The saved receiver's type path.
const RECEIVER_PATH: &str = "regia_render::lightmap::LightmapReceiver";

/// A level's load: the scene reader (migrations included), its settings, and every saved
/// `LightmapReceiver` decoded through reflection; then the scene is written again.
fn lightmap_level_load(s: &str) -> Result<String, String> {
    let file = regia_scene::format::SceneFile::from_ron_str(s).map_err(e)?;
    if let Some(raw) = file.resources.get(regia_scene::level::LEVEL_SETTINGS_PATH) {
        regia_scene::level::LevelSettings::from_ron(raw.get_ron()).map_err(e)?;
    }
    let mut registry = TypeRegistry::default();
    registry.register::<regia_render::lightmap::LightmapReceiver>();
    let registration = registry
        .get(std::any::TypeId::of::<regia_render::lightmap::LightmapReceiver>())
        .ok_or_else(|| format!("{}LightmapReceiver is not registered", super::BUG_PREFIX))?;
    for entity in &file.entities {
        if let Some(raw) = entity.components.get(RECEIVER_PATH) {
            ReflectCodec::new(&registry)
                .from_ron_typed(raw.get_ron(), registration)
                .map_err(e)?;
        }
    }
    file.to_ron_string().map_err(e)
}

fn primitive_registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    registry.register::<regia_render::primitives::Primitive>();
    registry.register::<regia_render::primitives::PrimitiveShape>();
    registry
}

fn primitive_fixture() -> String {
    let registry = primitive_registry();
    ReflectCodec::new(&registry)
        .to_ron(&regia_render::primitives::Primitive::default() as &dyn PartialReflect)
        .unwrap_or_default()
}

/// The scene loader's path for a reflected component: decode under its type path, then migrate.
fn primitive_load(s: &str) -> Result<String, String> {
    let registry = primitive_registry();
    let value = ReflectCodec::new(&registry)
        .from_ron(s)
        .map_err(e)?
        .into_value()
        .ok_or_else(|| "no value".to_string())?;
    let primitive = value
        .downcast::<regia_render::primitives::Primitive>()
        .map_err(|_| "not a Primitive".to_string())?
        .migrate()
        .map_err(e)?;
    ReflectCodec::new(&registry)
        .to_ron(&primitive as &dyn PartialReflect)
        .map_err(e)
}

fn post_volume_registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    regia_render::post_volumes::register_post_volume_types(&mut registry);
    registry
}

fn post_volume_fixture() -> String {
    let registry = post_volume_registry();
    ReflectCodec::new(&registry)
        .to_ron(&post_volume_v3() as &dyn PartialReflect)
        .unwrap_or_default()
}

/// A format-v3 volume: a slanted convex hull with a LUT (ADR-2323), so mutations reach the hull
/// planes and the LUT path as well as the v2 fields.
fn post_volume_v3() -> regia_render::PostVolume {
    let mut planes = regia_render::post_volume_shapes::box_hull_planes([4.0, 3.0, 12.0]);
    let h = std::f32::consts::FRAC_1_SQRT_2;
    planes.push([h, h, 0.0, 4.5]);
    regia_render::PostVolume {
        shape: regia_render::PostVolumeShape::ConvexHull { planes },
        priority: 2,
        blend_distance: 1.5,
        lut: Some("looks/hall.cube".into()),
        ..regia_render::PostVolume::default()
    }
}

fn post_volume_load(s: &str) -> Result<String, String> {
    let registry = post_volume_registry();
    let value = ReflectCodec::new(&registry)
        .from_ron(s)
        .map_err(e)?
        .into_value()
        .ok_or_else(|| "no value".to_string())?;
    let volume = value
        .downcast::<regia_render::PostVolume>()
        .map_err(|_| "not a PostVolume".to_string())?
        .migrate()
        .map_err(e)?;
    ReflectCodec::new(&registry)
        .to_ron(&volume as &dyn PartialReflect)
        .map_err(e)
}

#[cfg(test)]
mod statetree_include_tests {
    use super::*;
    use regia_ai::state_tree::StateTreeError;

    /// The fuzz fixture with `includes` entries added (`count` of them, prefixes `inc0`, `inc1`...).
    fn with_includes(count: usize) -> String {
        let fixture = include_str!("fixtures/statetree.example.ron");
        let includes: String = (0..count)
            .map(|i| format!("(prefix: \"inc{i}\", path: \"ai/part{i}.statetree.ron\"), "))
            .collect();
        let text = fixture.replacen(
            "    states: [",
            &format!("    includes: [{includes}],\n    states: ["),
            1,
        );
        assert_ne!(text, fixture, "the fixture's `states:` line moved");
        text
    }

    /// Review finding (fixrev4): every include path loaded the input itself, so the nested
    /// level's first include was already on the stack and expansion always stopped with
    /// `IncludeCycle` at depth 1. Prefixing, the nesting limit and the state-count check after
    /// merging never ran on fuzz input. Now each runs.
    #[test]
    fn include_expansion_reaches_prefixing_and_the_nesting_limit() {
        let s = with_includes(1);
        let def = regia_ai::StateTreeDef::from_ron(&s).expect("the fixture with an include parses");
        let base = def.states.len();
        let probe = statetree_include_passes(&def, &s);
        assert!(
            probe
                .outcomes
                .iter()
                .any(|o| matches!(o, Err(StateTreeError::IncludeCycle(_)))),
            "the cycle check runs: {:?}",
            probe.outcomes
        );
        assert!(
            probe.outcomes.iter().any(|o| matches!(o, Ok(n) if *n > base)),
            "an expansion succeeds with prefixed states merged in: {:?}",
            probe.outcomes
        );
        assert!(
            probe
                .outcomes
                .iter()
                .any(|o| matches!(o, Err(StateTreeError::IncludeDepth))),
            "the nesting limit is reached: {:?}",
            probe.outcomes
        );
        assert!(statetree_load(&s).is_ok(), "the input still round-trips");
    }

    /// Regression input: 24 includes at every level. Following them all down to the nesting limit
    /// loads about 1,200 texts before the state-count check stops the pass (and more with more
    /// includes); the fuzz target stops each pass at its load budget instead. The bound below is
    /// fixed (4 passes of 64 loads), not derived from the budget constants, so raising the budget
    /// fails this test.
    #[test]
    fn include_fan_out_stays_bounded() {
        const MOST_LOADS: usize = 4 * 64;
        let s = with_includes(24);
        let def = regia_ai::StateTreeDef::from_ron(&s).expect("the regression input parses");
        let probe = statetree_include_passes(&def, &s);
        assert!(
            probe.loads <= MOST_LOADS,
            "{} include loads (at most {MOST_LOADS}): {:?}",
            probe.loads,
            probe.outcomes
        );
        assert!(
            probe.outcomes.iter().any(|o| matches!(
                o,
                Err(StateTreeError::IncludeMissing(path)) if path.starts_with(STATETREE_INCLUDE_TAG)
            )),
            "the budget, not the input, stops the pass that goes to the nesting limit: {:?}",
            probe.outcomes
        );
        assert!(statetree_load(&s).is_ok(), "the input still round-trips");
    }

    /// A large input with a few includes reaches the state-count check after merging, within the
    /// load budget.
    #[test]
    fn include_expansion_reaches_the_state_count_check() {
        let mut def = regia_ai::StateTreeDef::from_ron(&with_includes(8)).expect("parses");
        let pad = def.states.last().cloned().expect("the fixture has states");
        for i in 0..700 {
            let mut state = pad.clone();
            state.name = format!("Pad{i}");
            def.states.push(state);
        }
        let s = def.to_ron().expect("writes");
        let def = regia_ai::StateTreeDef::from_ron(&s).expect("reads back");
        let probe = statetree_include_passes(&def, &s);
        assert!(
            probe
                .outcomes
                .iter()
                .any(|o| matches!(o, Err(StateTreeError::TooManyStates(_)))),
            "the state-count check after merging runs: {:?}",
            probe.outcomes
        );
        assert!(probe.loads <= STATETREE_INCLUDE_PASSES * STATETREE_INCLUDE_LOADS);
    }
}
