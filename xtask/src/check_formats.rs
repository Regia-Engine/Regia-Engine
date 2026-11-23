// SPDX-License-Identifier: MPL-2.0
//! `check-formats`: fail when a frozen content format's serde shape changed without a version bump
//! (`docs/specs/12-assets.md` § 4a, M18).
//!
//! Each format crate exposes a `schema_fingerprint()` — the ordered serde field list of its root
//! struct, every nested struct, and the variant names (canonical + alias spellings) of every
//! serde enum it reaches — and a current version constant. `check-formats` hashes every
//! fingerprint and compares it to the committed table in `docs/formats/format-hashes.ron`. If a
//! hash changed but the committed version did not, a struct was edited without bumping the format
//! version and adding a migrator, and the check fails. Variant names are part of the fingerprint
//! so a `rename`/`rename_all`/`alias` edit (a wire-format change with no field moved) trips the
//! guard too — the 2026-10-06 `MaterialShaderRef` miss.
//!
//! After a deliberate format change (bump the version, register a migrator in the loader and in
//! `regia upgrade-project`), run `cargo xtask check-formats --update` to re-record the hashes.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// The committed table: format name → (current version, schema hash).
#[derive(Serialize, Deserialize)]
struct FormatHashes {
    formats: BTreeMap<String, FormatEntry>,
}

/// One format's committed record.
#[derive(Serialize, Deserialize)]
struct FormatEntry {
    /// The current format version.
    version: u32,
    /// BLAKE3 hex of the schema fingerprint.
    hash: String,
}

/// One format's live data from the compiled crates.
pub(crate) struct LiveFormat {
    /// Registry name (as in `regia upgrade-project`).
    pub(crate) name: &'static str,
    /// Current version.
    pub(crate) version: u32,
    /// Serde field-list fingerprint.
    pub(crate) fingerprint: String,
    /// The `from` versions of the format's registered migration steps, so a version bump can be
    /// checked against a real migrator.
    pub(crate) migration_sources: Vec<u32>,
}

/// The registry of every frozen format: name, current version and the serde field-list fingerprint
/// from the format's crate. The name matches the `regia upgrade-project` registry.
pub(crate) fn live_formats() -> Vec<LiveFormat> {
    let mut formats = vec![
        LiveFormat {
            name: "meta",
            version: regia_asset::META_FORMAT_VERSION,
            fingerprint: regia_asset::meta_schema_fingerprint(),
            migration_sources: regia_asset::meta_migration_sources(),
        },
        LiveFormat {
            name: "material",
            version: regia_asset::MATERIAL_FORMAT_VERSION,
            fingerprint: regia_asset::material_schema_fingerprint(),
            migration_sources: regia_asset::material_migration_sources(),
        },
        LiveFormat {
            name: "glasscrack",
            version: regia_asset::glasscrack::GLASSCRACK_FORMAT_VERSION,
            fingerprint: regia_asset::glasscrack::glasscrack_schema_fingerprint(),
            migration_sources: regia_asset::glasscrack::glasscrack_migration_sources(),
        },
        LiveFormat {
            name: "world-index",
            version: regia_world::format::WORLD_INDEX_FORMAT_VERSION,
            fingerprint: regia_world::format::world_index_schema_fingerprint(),
            migration_sources: regia_world::format::world_index_migration_sources(),
        },
        LiveFormat {
            name: "cell-blob",
            version: regia_world::format::CELL_BLOB_FORMAT_VERSION,
            fingerprint: regia_world::format::cell_blob_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        LiveFormat {
            name: "scene",
            version: regia_scene::format::SCENE_FORMAT_VERSION,
            fingerprint: regia_scene::format::scene_schema_fingerprint(),
            migration_sources: regia_scene::format::scene_migration_sources(),
        },
        LiveFormat {
            name: "primitive",
            version: regia_render::primitives::PRIMITIVE_FORMAT_VERSION,
            fingerprint: regia_render::primitives::primitive_schema_fingerprint(),
            migration_sources: regia_render::primitives::primitive_migration_sources(),
        },
        LiveFormat {
            name: "ui",
            version: regia_ui::UI_RON_VERSION,
            fingerprint: regia_ui::ui_schema_fingerprint(),
            migration_sources: regia_ui::ui_migration_sources(),
        },
        LiveFormat {
            name: "uisettings",
            version: regia_ui::UI_SETTINGS_VERSION,
            fingerprint: regia_ui::ui_settings_schema_fingerprint(),
            migration_sources: regia_ui::ui_settings_migration_sources(),
        },
        LiveFormat {
            name: "theme",
            version: regia_ui::THEME_FORMAT_VERSION,
            fingerprint: regia_ui::theme_schema_fingerprint(),
            migration_sources: regia_ui::theme_migration_sources(),
        },
        // The localization glossary (`.locglossary.ron`, ADR-3359): approved term translations.
        LiveFormat {
            name: "glossary",
            version: regia_ui::loc_glossary::GLOSSARY_FORMAT_VERSION,
            fingerprint: regia_ui::loc_glossary::glossary_schema_fingerprint(),
            migration_sources: regia_ui::loc_glossary::glossary_migration_sources(),
        },
        LiveFormat {
            name: "vfx",
            version: regia_vfx::VFX_FORMAT_VERSION,
            fingerprint: regia_vfx::vfx_schema_fingerprint(),
            migration_sources: regia_vfx::vfx_migration_sources(),
        },
        LiveFormat {
            name: "animgraph",
            version: regia_animation::ANIMGRAPH_FORMAT_VERSION,
            fingerprint: regia_animation::animgraph_schema_fingerprint(),
            migration_sources: regia_animation::animgraph_migration_sources(),
        },
        LiveFormat {
            name: "animedit",
            version: regia_animation::ANIMEDIT_FORMAT_VERSION,
            fingerprint: regia_animation::animedit_schema_fingerprint(),
            migration_sources: regia_animation::animedit_migration_sources(),
        },
        LiveFormat {
            name: "spring",
            version: regia_animation::SPRING_FORMAT_VERSION,
            fingerprint: regia_animation::spring_schema_fingerprint(),
            migration_sources: regia_animation::spring_migration_sources(),
        },
        LiveFormat {
            name: "drivers",
            version: regia_animation::DRIVERS_FORMAT_VERSION,
            fingerprint: regia_animation::drivers_schema_fingerprint(),
            migration_sources: regia_animation::drivers_migration_sources(),
        },
        LiveFormat {
            name: "skelprofile",
            version: regia_animation::SKELPROFILE_FORMAT_VERSION,
            fingerprint: regia_animation::skelprofile_schema_fingerprint(),
            migration_sources: regia_animation::skelprofile_migration_sources(),
        },
        LiveFormat {
            name: "cue",
            version: regia_audio::CUE_VERSION,
            fingerprint: regia_audio::cue_schema_fingerprint(),
            migration_sources: regia_audio::cue_migration_sources(),
        },
        LiveFormat {
            name: "music",
            version: regia_audio::MUSIC_VERSION,
            fingerprint: regia_audio::music_schema_fingerprint(),
            migration_sources: regia_audio::music_migration_sources(),
        },
        LiveFormat {
            name: "ambience",
            version: regia_audio::AMBIENCE_VERSION,
            fingerprint: regia_audio::ambience_schema_fingerprint(),
            migration_sources: regia_audio::ambience_migration_sources(),
        },
        LiveFormat {
            name: "audiopatch",
            version: regia_audio::PATCH_VERSION,
            fingerprint: regia_audio::patch_schema_fingerprint(),
            migration_sources: regia_audio::patch_migration_sources(),
        },
        LiveFormat {
            name: "loopgen",
            version: regia_audio::LOOPGEN_VERSION,
            fingerprint: regia_audio::loopgen_schema_fingerprint(),
            migration_sources: regia_audio::loopgen_migration_sources(),
        },
        // Video sources and playlists (specs/50 § 4, ADR-1670).
        LiveFormat {
            name: "video",
            version: regia_media::source::VIDEO_SOURCE_VERSION,
            fingerprint: regia_media::source::video_source_schema_fingerprint(),
            migration_sources: regia_media::source::video_migration_sources(),
        },
        LiveFormat {
            name: "videolist",
            version: regia_media::source::VIDEO_LIST_VERSION,
            fingerprint: regia_media::source::video_list_schema_fingerprint(),
            migration_sources: regia_media::source::video_migration_sources(),
        },
        LiveFormat {
            name: "terrain-layers",
            version: regia_terrain::TERRAIN_LAYERS_FORMAT_VERSION,
            fingerprint: regia_terrain::terrain_layers_schema_fingerprint(),
            migration_sources: regia_terrain::terrain_layers_migration_sources(),
        },
        LiveFormat {
            name: "terrain-holes",
            version: regia_terrain::HOLES_FORMAT_VERSION,
            fingerprint: regia_terrain::holes_schema_fingerprint(),
            migration_sources: regia_terrain::holes_migration_sources(),
        },
        LiveFormat {
            name: "scatter",
            version: regia_terrain::SCATTER_FORMAT_VERSION,
            fingerprint: regia_terrain::scatter_schema_fingerprint(),
            migration_sources: regia_terrain::scatter_migration_sources(),
        },
        LiveFormat {
            name: "scatter-paint",
            version: regia_terrain::SCATTER_PAINT_FORMAT_VERSION,
            fingerprint: regia_terrain::scatter_paint_schema_fingerprint(),
            migration_sources: regia_terrain::scatter_paint_migration_sources(),
        },
        LiveFormat {
            name: "grass",
            version: regia_terrain::GRASS_FORMAT_VERSION,
            fingerprint: regia_terrain::grass_schema_fingerprint(),
            migration_sources: regia_terrain::grass_migration_sources(),
        },
        LiveFormat {
            name: "matgraph",
            version: regia_matgraph::MATGRAPH_FORMAT_VERSION,
            fingerprint: regia_matgraph::matgraph_schema_fingerprint(),
            migration_sources: regia_matgraph::matgraph_migration_sources(),
        },
        LiveFormat {
            name: "matinst",
            version: regia_matgraph::MATINST_FORMAT_VERSION,
            fingerprint: regia_matgraph::matinst_schema_fingerprint(),
            migration_sources: regia_matgraph::matinst_migration_sources(),
        },
        LiveFormat {
            name: "matfunc",
            version: regia_matgraph::MATFUNC_FORMAT_VERSION,
            fingerprint: regia_matgraph::matfunc_schema_fingerprint(),
            migration_sources: regia_matgraph::matfunc_migration_sources(),
        },
        LiveFormat {
            name: "tilemap",
            version: regia_2d::TILEMAP_FORMAT_VERSION,
            fingerprint: regia_2d::tilemap_schema_fingerprint(),
            migration_sources: regia_2d::tilemap_migration_sources(),
        },
        LiveFormat {
            name: "atlas",
            version: regia_2d::ATLAS_FORMAT_VERSION,
            fingerprint: regia_2d::atlas_schema_fingerprint(),
            migration_sources: regia_2d::atlas_migration_sources(),
        },
        LiveFormat {
            name: "autotile",
            version: regia_2d::AUTOTILE_FORMAT_VERSION,
            fingerprint: regia_2d::autotile_schema_fingerprint(),
            migration_sources: regia_2d::autotile_migration_sources(),
        },
        LiveFormat {
            name: "tilepatterns",
            version: regia_2d::tilepatterns::TILEPATTERNS_FORMAT_VERSION,
            fingerprint: regia_2d::tilepatterns::tilepatterns_schema_fingerprint(),
            migration_sources: regia_2d::tilepatterns::tilepatterns_migration_sources(),
        },
        LiveFormat {
            name: "save",
            version: regia_framework::save_game::SAVE_FORMAT_VERSION,
            fingerprint: regia_framework::save_game::save_schema_fingerprint(),
            migration_sources: regia_framework::save_game::save_migration_sources(),
        },
        LiveFormat {
            name: "demo",
            version: regia_framework::demo::DEMO_FORMAT_VERSION,
            fingerprint: regia_framework::demo::demo_schema_fingerprint(),
            migration_sources: regia_framework::demo::demo_migration_sources(),
        },
        LiveFormat {
            name: "frec",
            version: regia_framework::input_recording::FREC_FORMAT_VERSION,
            fingerprint: regia_framework::input_recording::frec_schema_fingerprint(),
            migration_sources: regia_framework::input_recording::frec_migration_sources(),
        },
        LiveFormat {
            name: "shots",
            version: regia_framework::shots::SHOT_SEQUENCE_FORMAT_VERSION,
            fingerprint: regia_framework::shots::shot_sequence_schema_fingerprint(),
            migration_sources: regia_framework::shots::shot_sequence_migration_sources(),
        },
        LiveFormat {
            name: "sequence",
            version: regia_framework::sequencer::SEQUENCE_FORMAT_VERSION,
            fingerprint: regia_framework::sequencer::sequence_schema_fingerprint(),
            migration_sources: regia_framework::sequencer::sequence_migration_sources(),
        },
        LiveFormat {
            name: "statetree",
            version: regia_ai::state_tree::STATE_TREE_FORMAT_VERSION,
            fingerprint: regia_ai::state_tree::state_tree_schema_fingerprint(),
            migration_sources: regia_ai::state_tree::state_tree_migration_sources(),
        },
        LiveFormat {
            name: "logic",
            version: regia_ai::logic::LOGIC_FORMAT_VERSION,
            fingerprint: regia_ai::logic::logic_schema_fingerprint(),
            migration_sources: regia_ai::logic::logic_migration_sources(),
        },
        LiveFormat {
            name: "smartobject",
            version: regia_ai::smart_object_def::SMART_OBJECT_DEF_VERSION,
            fingerprint: regia_ai::smart_object_def::smart_object_def_schema_fingerprint(),
            migration_sources: regia_ai::smart_object_def::smart_object_def_migration_sources(),
        },
        LiveFormat {
            name: "utility",
            version: regia_ai::utility::UTILITY_AI_FORMAT_VERSION,
            fingerprint: regia_ai::utility::utility_ai_schema_fingerprint(),
            migration_sources: regia_ai::utility::utility_ai_migration_sources(),
        },
        LiveFormat {
            name: "tags",
            version: regia_framework::tags::TAG_TABLE_FORMAT_VERSION,
            fingerprint: regia_framework::tags::tag_table_schema_fingerprint(),
            migration_sources: regia_framework::tags::tag_table_migration_sources(),
        },
        // Conversations and captions (spec 56, ADR-1697): four v1 source formats.
        LiveFormat {
            name: "conversation",
            version: regia_framework::conversation::CONVERSATION_FORMAT_VERSION,
            fingerprint: regia_framework::conversation::conversation_schema_fingerprint(),
            migration_sources: regia_framework::conversation::conversation_migration_sources(),
        },
        LiveFormat {
            name: "conversation-speakers",
            version: regia_framework::conversation::CONVERSATION_FORMAT_VERSION,
            fingerprint: regia_framework::conversation::speakers_schema_fingerprint(),
            migration_sources: regia_framework::conversation::conversation_migration_sources(),
        },
        LiveFormat {
            name: "conversation-settings",
            version: regia_framework::conversation::CONVERSATION_FORMAT_VERSION,
            fingerprint: regia_framework::conversation::settings_schema_fingerprint(),
            migration_sources: regia_framework::conversation::conversation_migration_sources(),
        },
        LiveFormat {
            name: "conversation-cue-events",
            version: regia_framework::conversation::CONVERSATION_FORMAT_VERSION,
            fingerprint: regia_framework::conversation::cue_events_schema_fingerprint(),
            migration_sources: regia_framework::conversation::conversation_migration_sources(),
        },
        // Voices, dialogue lines and subtitle cue sets (spec 36, ADR-1882): three v1 source formats.
        LiveFormat {
            name: "voice",
            version: regia_asset::dialogue::VOICE_VERSION,
            fingerprint: regia_asset::dialogue::voice_schema_fingerprint(),
            migration_sources: regia_asset::dialogue::dialogue_migration_sources(),
        },
        LiveFormat {
            name: "dialogue",
            version: regia_asset::dialogue::DIALOGUE_VERSION,
            fingerprint: regia_asset::dialogue::dialogue_schema_fingerprint(),
            migration_sources: regia_asset::dialogue::dialogue_migration_sources(),
        },
        LiveFormat {
            name: "subtitles",
            version: regia_asset::dialogue::SUBTITLES_VERSION,
            fingerprint: regia_asset::dialogue::subtitles_schema_fingerprint(),
            migration_sources: regia_asset::dialogue::dialogue_migration_sources(),
        },
        LiveFormat {
            name: "cooked-mesh",
            version: regia_asset::COOKED_MESH_FORMAT_VERSION,
            fingerprint: regia_asset::cooked_mesh_schema_fingerprint(),
            migration_sources: regia_asset::cooked_mesh_migration_sources(),
        },
        LiveFormat {
            name: "pak",
            version: regia_asset::FPAK_VERSION,
            fingerprint: regia_asset::pak_schema_fingerprint(),
            migration_sources: regia_asset::pak_migration_sources(),
        },
        LiveFormat {
            name: "bake",
            version: regia_pt::bake_desc::BAKE_DESC_FORMAT_VERSION,
            fingerprint: regia_pt::bake_desc::bake_desc_schema_fingerprint(),
            migration_sources: regia_pt::bake_desc::bake_desc_migration_sources(),
        },
        LiveFormat {
            name: "cooked-clip",
            version: regia_animation::clip::CLIP_FORMAT_VERSION,
            fingerprint: regia_animation::clip::clip_schema_fingerprint(),
            migration_sources: regia_animation::clip::clip_migration_sources(),
        },
        LiveFormat {
            name: "node-tracks",
            version: regia_animation::node_tracks::NODE_TRACKS_VERSION,
            fingerprint: regia_animation::node_tracks::node_tracks_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // Baked lightmaps (`.flmap`, ADR-860): the postcard header's shape.
        LiveFormat {
            name: "lightmap",
            version: regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION,
            fingerprint: regia_asset::lightmap::lightmap_schema_fingerprint(),
            migration_sources: regia_asset::lightmap::lightmap_migration_sources(),
        },
        // Post volumes inside scene files (the reflected `PostVolume` component, ADR-1464).
        LiveFormat {
            name: "post-volume",
            version: regia_render::post_volumes::POST_VOLUME_FORMAT_VERSION,
            fingerprint: regia_render::post_volumes::post_volume_schema_fingerprint(),
            migration_sources: regia_render::post_volumes::post_volume_migration_sources(),
        },
        // Montages and sync groups (ADR-1465).
        LiveFormat {
            name: "montage",
            version: regia_animation::MONTAGE_FORMAT_VERSION,
            fingerprint: regia_animation::montage_schema_fingerprint(),
            migration_sources: regia_animation::montage_migration_sources(),
        },
        LiveFormat {
            name: "sync",
            version: regia_animation::SYNC_FORMAT_VERSION,
            fingerprint: regia_animation::sync_schema_fingerprint(),
            migration_sources: regia_animation::sync_migration_sources(),
        },
        // Variant montages (spec 34 M4, ADR-1644).
        LiveFormat {
            name: "montage-variant",
            version: regia_animation::MONTAGE_VARIANT_FORMAT_VERSION,
            fingerprint: regia_animation::montage_variant_schema_fingerprint(),
            migration_sources: regia_animation::montage_variant_migration_sources(),
        },
        // The skeleton slot table (spec 34 M1, ADR-1644).
        LiveFormat {
            name: "slots",
            version: regia_animation::SLOTS_FORMAT_VERSION,
            fingerprint: regia_animation::slots_schema_fingerprint(),
            migration_sources: regia_animation::slots_migration_sources(),
        },
        // The render-job file of sequence-driven render-out (ADR-1616).
        LiveFormat {
            name: "render-job",
            version: regia_agent::render_job::RENDER_JOB_FORMAT_VERSION,
            fingerprint: regia_agent::render_job::render_job_schema_fingerprint(),
            migration_sources: regia_agent::render_job::render_job_migration_sources(),
        },
        // The look-review shot list of `seeing.capture_plate` (spec 27 § 5, ADR-2463).
        LiveFormat {
            name: "shot",
            version: regia_agent::shot_list::SHOT_LIST_FORMAT_VERSION,
            fingerprint: regia_agent::shot_list::shot_list_schema_fingerprint(),
            migration_sources: regia_agent::shot_list::shot_list_migration_sources(),
        },
        // Cloth and soft-body assets, sources and cooked payloads (spec 55 § 4, ADR-2083).
        LiveFormat {
            name: "cloth",
            version: regia_physics::cloth::asset::CLOTH_FORMAT_VERSION,
            fingerprint: regia_physics::cloth::asset::cloth_schema_fingerprint(),
            migration_sources: regia_physics::cloth::asset::cloth_migration_sources(),
        },
        LiveFormat {
            name: "soft",
            version: regia_physics::cloth::asset::SOFT_FORMAT_VERSION,
            fingerprint: regia_physics::cloth::asset::soft_schema_fingerprint(),
            migration_sources: regia_physics::cloth::asset::soft_migration_sources(),
        },
        LiveFormat {
            name: "cooked-cloth",
            version: regia_physics::cloth::asset::COOKED_CLOTH_FORMAT_VERSION,
            fingerprint: regia_physics::cloth::asset::cooked_cloth_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        LiveFormat {
            name: "cooked-soft",
            version: regia_physics::cloth::asset::COOKED_SOFT_FORMAT_VERSION,
            fingerprint: regia_physics::cloth::asset::cooked_soft_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // Destruction level 2 chunk assets (`.destructible.ron`, ADR-1597).
        LiveFormat {
            name: "destructible",
            version: regia_physics::destruction::DESTRUCTIBLE_FORMAT_VERSION,
            fingerprint: regia_physics::destruction::destructible_schema_fingerprint(),
            migration_sources: regia_physics::destruction::destructible_migration_sources(),
        },
        // Destruction effects tables (`.destructionfx.ron`, ADR-2734).
        LiveFormat {
            name: "destructionfx",
            version: regia_framework::destruction_fx::DESTRUCTION_FX_FORMAT_VERSION,
            fingerprint: regia_framework::destruction_fx::destruction_fx_schema_fingerprint(),
            migration_sources: regia_framework::destruction_fx::destruction_fx_migration_sources(),
        },
        // Pre-fractured chunk sets (`.fracture.ron`, ADR-1567; registered at v2 by ADR-1967).
        LiveFormat {
            name: "fracture",
            version: regia_asset_pipeline::fracture::FRACTURE_FORMAT_VERSION,
            fingerprint: regia_asset_pipeline::fracture::fracture_schema_fingerprint(),
            migration_sources: regia_asset_pipeline::fracture::fracture_migration_sources(),
        },
        // Vehicle configurations (`.vehicle.ron`, ADR-2301; v2 and its v1 migrator, ADR-2598; v3 adds
        // the wheels' `tire_model`, migrating v1 and v2, ADR-2719; v4 the slip tire's transients,
        // migrating v1 to v3, ADR-3276).
        LiveFormat {
            name: "vehicle",
            version: regia_physics::vehicle_asset::VEHICLE_FORMAT_VERSION,
            fingerprint: regia_physics::vehicle_asset::vehicle_schema_fingerprint(),
            migration_sources: regia_physics::vehicle_asset::vehicle_migration_sources(),
        },
        // A saved vehicle (`.vehicle-snapshot.ron`, ADR-3080).
        LiveFormat {
            name: "vehicle-snapshot",
            version: regia_physics::vehicle_snapshot::VEHICLE_SNAPSHOT_FORMAT_VERSION,
            fingerprint: regia_physics::vehicle_snapshot::vehicle_snapshot_schema_fingerprint(),
            migration_sources: regia_physics::vehicle_snapshot::vehicle_snapshot_migration_sources(),
        },
        // Physics materials (`.physmat.ron`, ADR-2424).
        LiveFormat {
            name: "physmat",
            version: regia_physics::material::PHYSMAT_FORMAT_VERSION,
            fingerprint: regia_physics::material::physmat_schema_fingerprint(),
            migration_sources: regia_physics::material::physmat_migration_sources(),
        },
        // Cooked convex hull sets (`FCHS`, the importer's `collision: ConvexDecomposition`, ADR-2301):
        // a binary layout, fingerprinted by its description.
        LiveFormat {
            name: "hull-set",
            version: regia_physics::hull_set::HULL_SET_FORMAT_VERSION,
            fingerprint: regia_physics::hull_set::hull_set_schema_fingerprint(),
            migration_sources: regia_physics::hull_set::hull_set_migration_sources(),
        },
        // Data-driven gameplay cameras (`.camera.ron`, spec 35, ADR-1637; registered by ADR-1654).
        LiveFormat {
            name: "camera",
            version: regia_framework::camera::asset::CAMERA_FORMAT_VERSION,
            fingerprint: regia_framework::camera::asset::camera_schema_fingerprint(),
            migration_sources: regia_framework::camera::asset::camera_migration_sources(),
        },
        // Voice chat project settings (`config/voice_chat.ron`, spec 59, ADR-1647; registered by
        // ADR-1654).
        LiveFormat {
            name: "voice-chat",
            version: regia_audio::voice_chat::CONFIG_FORMAT_VERSION,
            fingerprint: regia_audio::voice_chat::voice_chat_config_schema_fingerprint(),
            migration_sources: regia_audio::voice_chat::voice_chat_config_migration_sources(),
        },
        // The project's weather config (`config/weather.ron`, spec 99, ADR-1847).
        LiveFormat {
            name: "weather",
            version: regia_vfx::weather::WEATHER_CONFIG_FORMAT_VERSION,
            fingerprint: regia_vfx::weather::weather_config_schema_fingerprint(),
            migration_sources: regia_vfx::weather::weather_config_migration_sources(),
        },
        // Procedural content graphs (specs/62, ADR-1640).
        LiveFormat {
            name: "pgraph",
            version: regia_procgen::graph::PGRAPH_FORMAT_VERSION,
            fingerprint: regia_procgen::graph::pgraph_schema_fingerprint(),
            migration_sources: regia_procgen::graph::pgraph_migration_sources(),
        },
        LiveFormat {
            name: "pgvar",
            version: regia_procgen::graph::PGVAR_FORMAT_VERSION,
            fingerprint: regia_procgen::graph::pgvar_schema_fingerprint(),
            migration_sources: regia_procgen::graph::pgvar_migration_sources(),
        },
        LiveFormat {
            name: "pgout",
            version: regia_procgen::pgout::PGOUT_VERSION,
            fingerprint: regia_procgen::pgout::pgout_schema_fingerprint(),
            migration_sources: regia_procgen::pgout::pgout_migration_sources(),
        },
        LiveFormat {
            name: "procgen-generator",
            version: regia_procgen::world::PROCGEN_GENERATOR_FORMAT_VERSION,
            fingerprint: regia_procgen::world::procgen_generator_schema_fingerprint(),
            migration_sources: regia_procgen::world::procgen_generator_migration_sources(),
        },
        LiveFormat {
            name: "procgen-instanced-set",
            version: regia_procgen::world::INSTANCED_SET_FORMAT_VERSION,
            fingerprint: regia_procgen::world::instanced_set_schema_fingerprint(),
            migration_sources: regia_procgen::world::instanced_set_migration_sources(),
        },
        // Anchors of anchored runtime generation (spec 66, ADR-1666); registered by ADR-1875.
        LiveFormat {
            name: "procgen-anchor",
            version: regia_procgen::tiles::PROCGEN_ANCHOR_FORMAT_VERSION,
            fingerprint: regia_procgen::tiles::procgen_anchor_schema_fingerprint(),
            migration_sources: regia_procgen::tiles::procgen_anchor_migration_sources(),
        },
        LiveFormat {
            name: "procgen-painted-points",
            version: regia_procgen::painted::PAINTED_POINTS_FORMAT_VERSION,
            fingerprint: regia_procgen::painted::painted_points_schema_fingerprint(),
            migration_sources: regia_procgen::painted::painted_points_migration_sources(),
        },
        // The cooked extension record (spec 57, ADR-1885).
        LiveFormat {
            name: "extensions-ron",
            version: regia_framework::extension::cooked::COOKED_FORMAT_VERSION,
            fingerprint: regia_framework::extension::cooked::cooked_schema_fingerprint(),
            migration_sources: regia_framework::extension::cooked::cooked_migration_sources(),
        },
        // Content chunks (spec 40, ADR-1667): the manifest, and the chunk map with the install record.
        LiveFormat {
            name: "content-manifest",
            version: regia_asset::content::CONTENT_MANIFEST_FORMAT_VERSION,
            fingerprint: regia_asset::content::content_manifest_schema_fingerprint(),
            migration_sources: regia_asset::content::content_manifest_migration_sources(),
        },
        LiveFormat {
            name: "chunk-map",
            version: regia_asset::content::CHUNK_MAP_FORMAT_VERSION,
            fingerprint: regia_asset::content::chunk_map_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // The per-device install record, its own row (ADR-7236; ADR-1667 asked for it).
        LiveFormat {
            name: "content-state",
            version: regia_asset::content::CONTENT_STATE_FORMAT_VERSION,
            fingerprint: regia_asset::content::content_state_schema_fingerprint(),
            migration_sources: regia_asset::content::content_state_migration_sources(),
        },
        // Per-entity level directories and layer definitions (spec 37, ADR-1667).
        LiveFormat {
            name: "level-dir",
            version: regia_scene::level_dir::LEVEL_DIR_FORMAT_VERSION,
            fingerprint: regia_scene::level_dir::level_dir_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        LiveFormat {
            name: "layers",
            version: regia_scene::layers::LAYERS_FORMAT_VERSION,
            fingerprint: regia_scene::layers::layers_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // A layer stored in its own file next to the level (ADR-7792).
        LiveFormat {
            name: "layer-file",
            version: regia_scene::layer_file::LAYER_FILE_FORMAT_VERSION,
            fingerprint: regia_scene::layer_file::layer_file_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // In-level functional tests (ADR-1673, spec 41 § 4).
        LiveFormat {
            name: "functional-test",
            version: regia_framework::functional::formats::FUNCTIONAL_TEST_VERSION,
            fingerprint: regia_framework::functional::formats::functional_test_fingerprint(),
            migration_sources: vec![],
        },
        LiveFormat {
            name: "functest-script",
            version: regia_framework::functional::formats::FUNCTEST_SCRIPT_VERSION,
            fingerprint: regia_framework::functional::formats::functest_script_fingerprint(),
            migration_sources: vec![],
        },
        LiveFormat {
            name: "test-report",
            version: regia_framework::functional::formats::TEST_REPORT_VERSION,
            fingerprint: regia_framework::functional::formats::test_report_fingerprint(),
            // v2 adds the optional per-test `notes` (ADR-7980); `TestReport::from_json` reads v1.
            migration_sources: regia_framework::functional::formats::test_report_migration_sources(),
        },
        LiveFormat {
            name: "expected-values",
            version: regia_framework::functional::formats::EXPECTED_VALUES_VERSION,
            fingerprint: regia_framework::functional::formats::expected_values_fingerprint(),
            migration_sources: vec![],
        },
        // Curve tables and choosers (spec 51), attributes, effects and abilities (spec 53); v1 each
        // (registered by ADR-1695).
        LiveFormat {
            name: "curves",
            version: regia_framework::curve_table::CURVE_TABLE_FORMAT_VERSION,
            fingerprint: regia_framework::curve_table::curve_table_schema_fingerprint(),
            migration_sources: regia_framework::curve_table::curve_table_migration_sources(),
        },
        LiveFormat {
            name: "chooser",
            version: regia_framework::chooser::CHOOSER_FORMAT_VERSION,
            fingerprint: regia_framework::chooser::chooser_schema_fingerprint(),
            migration_sources: regia_framework::chooser::chooser_migration_sources(),
        },
        // Their cooked (pak) forms (ADR-2012). A cache, not a source format: a shape change bumps
        // the version, and a pak of another version is cooked again, so there is no migration.
        LiveFormat {
            name: "curves-cooked",
            version: regia_framework::curve_table::CURVE_TABLE_COOKED_VERSION,
            fingerprint: regia_framework::curve_table::curve_table_cooked_schema_fingerprint(),
            migration_sources: vec![],
        },
        LiveFormat {
            name: "chooser-cooked",
            version: regia_framework::chooser::CHOOSER_COOKED_VERSION,
            fingerprint: regia_framework::chooser::chooser_cooked_schema_fingerprint(),
            migration_sources: vec![],
        },
        // The cooked (pak) form of typed data tables (ADR-2132): a cache like the two above.
        LiveFormat {
            name: "data-table-cooked",
            version: regia_asset::data_table::DATA_TABLE_COOKED_VERSION,
            fingerprint: regia_asset::data_table::data_table_cooked_schema_fingerprint(),
            migration_sources: vec![],
        },
        LiveFormat {
            name: "attrs",
            version: regia_framework::abilities::ATTRIBUTES_FORMAT_VERSION,
            fingerprint: regia_framework::abilities::attributes_schema_fingerprint(),
            migration_sources: regia_framework::abilities::attributes_migration_sources(),
        },
        LiveFormat {
            name: "effect",
            version: regia_framework::abilities::EFFECT_FORMAT_VERSION,
            fingerprint: regia_framework::abilities::effect_schema_fingerprint(),
            migration_sources: regia_framework::abilities::effect_migration_sources(),
        },
        LiveFormat {
            name: "ability",
            version: regia_framework::abilities::ABILITY_FORMAT_VERSION,
            fingerprint: regia_framework::abilities::ability_schema_fingerprint(),
            migration_sources: regia_framework::abilities::ability_migration_sources(),
        },
        // Their cooked (pak) form (spec 53 § 4.1, ADR-7216): a cache like `curves-cooked`; a shape
        // change bumps the version and the pak is cooked again, so there is no migration.
        LiveFormat {
            name: "abilities-cooked",
            version: regia_framework::abilities::ABILITIES_COOKED_VERSION,
            fingerprint: regia_framework::abilities::abilities_cooked_schema_fingerprint(),
            migration_sources: vec![],
        },
        // Spline and region-volume components (specs/64 § 2, ADR-1664).
        LiveFormat {
            name: "spline",
            version: regia_scene::spline::SPLINE_FORMAT_VERSION,
            fingerprint: regia_scene::spline::spline_schema_fingerprint(),
            migration_sources: regia_scene::spline::spline_migration_sources(),
        },
        LiveFormat {
            name: "shape",
            version: regia_scene::shape::SHAPE_FORMAT_VERSION,
            fingerprint: regia_scene::shape::shape_schema_fingerprint(),
            migration_sources: regia_scene::shape::shape_migration_sources(),
        },
        LiveFormat {
            name: "road",
            version: regia_procgen::road::ROAD_FORMAT_VERSION,
            fingerprint: regia_procgen::road::road_schema_fingerprint(),
            migration_sources: regia_procgen::road::road_migration_sources(),
        },
        LiveFormat {
            name: "road-of",
            version: regia_procgen::road::ROAD_OF_FORMAT_VERSION,
            fingerprint: regia_procgen::road::road_of_schema_fingerprint(),
            migration_sources: regia_procgen::road::road_of_migration_sources(),
        },
        // Biomes (specs/65, ADR-1665).
        LiveFormat {
            name: "biome",
            version: regia_procgen::biome::BIOME_FORMAT_VERSION,
            fingerprint: regia_procgen::biome::def::biome_schema_fingerprint(),
            migration_sources: regia_procgen::biome::def::biome_migration_sources(),
        },
        // Ecology profiles beside biomes (ADR-2456).
        LiveFormat {
            name: "ecology",
            version: regia_procgen::biome::ecology::ECOLOGY_FORMAT_VERSION,
            fingerprint: regia_procgen::biome::ecology::ecology_schema_fingerprint(),
            migration_sources: regia_procgen::biome::ecology::ecology_migration_sources(),
        },
        LiveFormat {
            name: "biome-area",
            version: regia_procgen::biome::ecs::BIOME_AREA_FORMAT_VERSION,
            fingerprint: regia_procgen::biome::ecs::biome_area_schema_fingerprint(),
            migration_sources: regia_procgen::biome::ecs::biome_area_migration_sources(),
        },
        // Plant variant index written by `regia procgen plant` (specs/68 § 8).
        LiveFormat {
            name: "plants-index",
            version: regia_procgen::plant::PLANTS_INDEX_FORMAT_VERSION,
            fingerprint: regia_procgen::plant::plants_index_schema_fingerprint(),
            migration_sources: regia_procgen::plant::plants_index_migration_sources(),
        },
        // Spatial queries for AI (specs/33, ADR-1674); v2 since ADR-2709, registered by ADR-1875.
        LiveFormat {
            name: "spatialquery",
            version: regia_ai::spatial_query::SPATIAL_QUERY_FORMAT_VERSION,
            fingerprint: regia_ai::spatial_query::spatial_query_schema_fingerprint(),
            migration_sources: regia_ai::spatial_query::spatial_query_migration_sources(),
        },
        // Ragdoll profiles (`.ragdoll.ron`, spec 93, ADR-1693; v3 active block, ADR-2498; v4 sole and recovery, ADR-2963).
        LiveFormat {
            name: "ragdoll",
            version: regia_framework::ragdoll::file::RAGDOLL_FORMAT_VERSION,
            fingerprint: regia_framework::ragdoll::file::ragdoll_schema_fingerprint(),
            migration_sources: regia_framework::ragdoll::file::ragdoll_migration_sources(),
        },
        // Motion-matching database sources (`.motiondb.ron`, spec 52 § 4, ADR-1831).
        LiveFormat {
            name: "motiondb",
            version: regia_animation::motion_matching::MOTIONDB_FORMAT_VERSION,
            fingerprint: regia_animation::motion_matching::motiondb_schema_fingerprint(),
            migration_sources: regia_animation::motion_matching::motiondb_migration_sources(),
        },
        // Retarget setups (spec 85, ADR-1857).
        LiveFormat {
            name: "retarget",
            version: regia_animation::RETARGET_FORMAT_VERSION,
            fingerprint: regia_animation::retarget_schema_fingerprint(),
            migration_sources: regia_animation::retarget_migration_sources(),
        },
        // Chunk reasons for the runtime `content.why` (spec 40, ADR-1876).
        LiveFormat {
            name: "chunk-why",
            version: regia_asset::content::CHUNK_WHY_FORMAT_VERSION,
            fingerprint: regia_asset::content::chunk_why_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // Where a packaged game fetches `external` chunks (spec 40 § 2.6, ADR-1876).
        LiveFormat {
            name: "content-host",
            version: regia_asset::content::CONTENT_HOST_FORMAT_VERSION,
            fingerprint: regia_asset::content::content_host_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // The keys that may sign a fetched manifest, and the detached signature (ADR-2710).
        LiveFormat {
            name: "content-trust",
            version: regia_asset::content::CONTENT_TRUST_FORMAT_VERSION,
            fingerprint: regia_asset::content::content_trust_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        LiveFormat {
            name: "content-signature",
            version: regia_asset::content::CONTENT_SIGNATURE_FORMAT_VERSION,
            fingerprint: regia_asset::content::content_signature_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // A Lua mod's manifest (ADR-1751).
        LiveFormat {
            name: "luamod",
            version: regia_script_lua::LUAMOD_FORMAT_VERSION,
            fingerprint: regia_script_lua::luamod_schema_fingerprint(),
            migration_sources: regia_script_lua::luamod_migration_sources(),
        },
        // Lua mod state in a save file (ADR-1943).
        LiveFormat {
            name: "luastate",
            version: regia_script_lua::LUASTATE_FORMAT_VERSION,
            fingerprint: regia_script_lua::luastate_schema_fingerprint(),
            migration_sources: regia_script_lua::luastate_migration_sources(),
        },
        // Authored off-mesh links (`regia_nav::link_file`, `.navlinks.ron` v1, spec 92, ADR-1821).
        LiveFormat {
            name: "navlinks",
            version: regia_nav::NAV_LINKS_FORMAT_VERSION,
            fingerprint: regia_nav::nav_links_schema_fingerprint(),
            migration_sources: regia_nav::nav_links_migration_sources(),
        },
        // Authored crowd lanes (`regia_nav::lanes`, `.navlanes.ron` v1, ADR-1934).
        LiveFormat {
            name: "navlanes",
            version: regia_nav::NAV_LANES_FORMAT_VERSION,
            fingerprint: regia_nav::nav_lanes_schema_fingerprint(),
            migration_sources: regia_nav::nav_lanes_migration_sources(),
        },
        // Off-mesh links authored as scene components (`regia_nav::NavLinkAnchor`, v1, ADR-2420).
        LiveFormat {
            name: "nav-link-anchor",
            version: regia_nav::NAV_LINK_ANCHOR_FORMAT_VERSION,
            fingerprint: regia_nav::nav_link_anchor_schema_fingerprint(),
            migration_sources: regia_nav::nav_link_anchor_migration_sources(),
        },
        // Traversal components saved in scenes (`regia_framework::traversal`, each v1, ADR-2971).
        LiveFormat {
            name: "traversal-settings",
            version: regia_framework::traversal::TRAVERSAL_SETTINGS_FORMAT_VERSION,
            fingerprint: regia_framework::traversal::traversal_settings_schema_fingerprint(),
            migration_sources: regia_framework::traversal::traversal_settings_migration_sources(),
        },
        LiveFormat {
            name: "traversal-ladder",
            version: regia_framework::traversal::TRAVERSAL_LADDER_FORMAT_VERSION,
            fingerprint: regia_framework::traversal::traversal_ladder_schema_fingerprint(),
            migration_sources: regia_framework::traversal::traversal_ladder_migration_sources(),
        },
        LiveFormat {
            name: "traversal-jump-point",
            version: regia_framework::traversal::TRAVERSAL_JUMP_POINT_FORMAT_VERSION,
            fingerprint: regia_framework::traversal::traversal_jump_point_schema_fingerprint(),
            migration_sources: regia_framework::traversal::traversal_jump_point_migration_sources(),
        },
        LiveFormat {
            name: "loudness-table",
            version: regia_audio::LOUDNESS_TABLE_VERSION,
            fingerprint: regia_audio::loudness_table_schema_fingerprint(),
            migration_sources: regia_audio::loudness_table_migration_sources(),
        },
        // Authored collision proxies of a glTF (`collision` sub-asset, v1, ADR-2341).
        LiveFormat {
            name: "collision-set",
            version: regia_asset::COLLISION_SET_FORMAT_VERSION,
            fingerprint: regia_asset::collision_set_schema_fingerprint(),
            migration_sources: regia_asset::collision_set_migration_sources(),
        },
        // The instanced nodes of a glTF (`prefab/instances` sub-asset, v1, ADR-3016).
        LiveFormat {
            name: "prefab-instances",
            version: regia_asset::PREFAB_INSTANCES_FORMAT_VERSION,
            fingerprint: regia_asset::prefab_instances_schema_fingerprint(),
            migration_sources: regia_asset::prefab_instances_migration_sources(),
        },
        // The format-registry sweep (ADR-2316): versioned files the gate did not guard yet. Net
        // (spec 76, 77, 89): `config/net.ron`, `.netprofile.ron`, `server.toml`, `bans.ron`,
        // `net-manifest.ron`, `server.key`, `*.siminput.ron` and the engine's lobby keys.
        LiveFormat {
            name: "net-settings",
            version: regia_net::settings::NET_SETTINGS_FORMAT_VERSION,
            fingerprint: regia_net::formats::net_settings_schema_fingerprint(),
            migration_sources: regia_net::formats::net_settings_migration_sources(),
        },
        LiveFormat {
            name: "netprofile",
            version: regia_net::profile::NET_PROFILE_FORMAT_VERSION,
            fingerprint: regia_net::formats::net_profile_schema_fingerprint(),
            migration_sources: regia_net::formats::net_profile_migration_sources(),
        },
        LiveFormat {
            name: "netdemo",
            version: regia_net::recorder::format::NETDEMO_FORMAT_VERSION,
            fingerprint: regia_net::recorder::format::netdemo_schema_fingerprint(),
            migration_sources: regia_net::recorder::format::netdemo_migration_sources(),
        },
        // A client capture with timing (spec 120 § 15.5, ADR-8221).
        LiveFormat {
            name: "netcapture",
            version: regia_net::capture::CAPTURE_FORMAT_VERSION,
            fingerprint: regia_net::capture::netcapture_schema_fingerprint(),
            migration_sources: regia_net::capture::netcapture_migration_sources(),
        },
        LiveFormat {
            name: "server-toml",
            version: regia_net::server_host::config::SERVER_TOML_FORMAT_VERSION,
            fingerprint: regia_net::formats::server_toml_schema_fingerprint(),
            migration_sources: regia_net::formats::server_toml_migration_sources(),
        },
        LiveFormat {
            name: "playlist-ron",
            version: regia_net::formats::PLAYLIST_RON_FORMAT_VERSION,
            fingerprint: regia_net::formats::playlist_ron_schema_fingerprint(),
            migration_sources: regia_net::formats::playlist_ron_migration_sources(),
        },
        // The dedicated server's kept player records, a section inside a world save (ADR-3354).
        LiveFormat {
            name: "server-players",
            version: regia_framework::server_persistence::PLAYER_SECTION_FORMAT_VERSION,
            fingerprint: regia_framework::server_persistence::server_players_schema_fingerprint(),
            migration_sources: regia_framework::server_persistence::server_players_migration_sources(),
        },
        // A finished match's record (audit gaps GR-10.1 to GR-10.4): JSON a host writes and reads back.
        LiveFormat {
            name: "match-result",
            version: regia_framework::match_results::MATCH_RESULT_FORMAT_VERSION,
            fingerprint: regia_framework::match_results::match_result_schema_fingerprint(),
            migration_sources: regia_framework::match_results::match_result_migration_sources(),
        },
        LiveFormat {
            name: "bans",
            version: regia_net::session::bans::BAN_LIST_FORMAT_VERSION,
            fingerprint: regia_net::formats::ban_list_schema_fingerprint(),
            migration_sources: regia_net::formats::ban_list_migration_sources(),
        },
        LiveFormat {
            name: "net-manifest",
            version: regia_net::session::manifest::NET_MANIFEST_FORMAT_VERSION,
            fingerprint: regia_net::formats::net_manifest_schema_fingerprint(),
            migration_sources: regia_net::formats::net_manifest_migration_sources(),
        },
        LiveFormat {
            name: "server-key",
            version: regia_net::crypto::SERVER_KEY_FORMAT_VERSION,
            fingerprint: regia_net::formats::server_key_schema_fingerprint(),
            migration_sources: regia_net::formats::server_key_migration_sources(),
        },
        LiveFormat {
            name: "siminput",
            version: regia_net::session::hub::SIM_RECORDING_FORMAT_VERSION,
            fingerprint: regia_net::formats::sim_recording_schema_fingerprint(),
            migration_sources: regia_net::formats::sim_recording_migration_sources(),
        },
        // The version probe and version stamp datagrams (spec 120 § 12.2, ADR-7985): a wire layout
        // frozen forever, fingerprinted by its description.
        LiveFormat {
            name: "version-probe",
            version: u32::from(regia_net::version_probe::PROBE_VERSION),
            fingerprint: regia_net::formats::version_probe_schema_fingerprint(),
            migration_sources: regia_net::formats::version_probe_migration_sources(),
        },
        // The lockstep desync bundle (spec 120 § 7.6, ADR-8455).
        LiveFormat {
            name: "desync",
            version: regia_net::desync::DESYNC_BUNDLE_FORMAT_VERSION,
            fingerprint: regia_net::formats::desync_bundle_schema_fingerprint(),
            migration_sources: regia_net::formats::desync_bundle_migration_sources(),
        },
        // The lockstep match replay (ADR-8513).
        LiveFormat {
            name: "lsreplay",
            version: regia_net::lockstep::replay::LSREPLAY_VERSION,
            fingerprint: regia_net::lockstep::replay::lsreplay_schema_fingerprint(),
            migration_sources: regia_net::lockstep::replay::lsreplay_migration_sources(),
        },
        LiveFormat {
            name: "lobby-keys",
            version: regia_net::lobby::LOBBY_KEYS_VERSION,
            fingerprint: regia_net::formats::lobby_keys_schema_fingerprint(),
            migration_sources: regia_net::formats::lobby_keys_migration_sources(),
        },
        // XR (spec 58): `config/xr.ron`, the action manifest and simulator scripts.
        LiveFormat {
            name: "xr-config",
            version: regia_xr::XR_CONFIG_FORMAT_VERSION,
            fingerprint: regia_xr::formats::xr_config_schema_fingerprint(),
            migration_sources: regia_xr::formats::xr_config_migration_sources(),
        },
        LiveFormat {
            name: "xr-actions",
            version: regia_xr::input::MANIFEST_FORMAT_VERSION,
            fingerprint: regia_xr::formats::xr_actions_schema_fingerprint(),
            migration_sources: regia_xr::formats::xr_actions_migration_sources(),
        },
        LiveFormat {
            name: "xrsim",
            version: regia_xr::sim::SIM_FORMAT_VERSION,
            fingerprint: regia_xr::formats::xrsim_schema_fingerprint(),
            migration_sources: regia_xr::formats::xrsim_migration_sources(),
        },
        // Tools: the packaged engine's release manifest (spec 94 § 4.1) and the session harness file
        // (spec 41 § 2.8, v2 since ADR-2137); both types live in the `regia_cli` library.
        LiveFormat {
            name: "engine-release",
            version: regia_cli::distribution::RELEASE_FORMAT_VERSION,
            fingerprint: regia_cli::distribution::release::release_schema_fingerprint(),
            migration_sources: regia_cli::distribution::release::release_migration_sources(),
        },
        LiveFormat {
            name: "session",
            version: regia_cli::session::SESSION_FILE_VERSION,
            fingerprint: regia_cli::session::session_schema_fingerprint(),
            migration_sources: regia_cli::session::session_migration_sources(),
        },
        // Content and project config: validation rules (spec 32), UI animations, data-table sources
        // (the cooked form is `data-table-cooked`), asset tags and the asset registry, input
        // overrides and profiles, the permutation list and cooked conversations.
        LiveFormat {
            name: "validation",
            version: regia_asset_pipeline::validation_rules::VALIDATION_FILE_FORMAT_VERSION,
            fingerprint: regia_asset_pipeline::validation_rules::validation_file_schema_fingerprint(),
            migration_sources: regia_asset_pipeline::validation_rules::validation_file_migration_sources(),
        },
        // The asset package manifest (`.regia_assetpkg.ron`, spec addendum § 8.9, ADR-7788).
        LiveFormat {
            name: "assetpkg",
            version: regia_asset_pipeline::asset_package::ASSET_PACKAGE_FORMAT_VERSION,
            fingerprint: regia_asset_pipeline::asset_package::asset_package_schema_fingerprint(),
            migration_sources: regia_asset_pipeline::asset_package::asset_package_migration_sources(),
        },
        // Project-wide import settings rules (spec 12 § 10, ADR-3321).
        LiveFormat {
            name: "import-rules",
            version: regia_asset_pipeline::import_rules::IMPORT_RULES_FORMAT_VERSION,
            fingerprint: regia_asset_pipeline::import_rules::import_rules_schema_fingerprint(),
            migration_sources: regia_asset_pipeline::import_rules::import_rules_migration_sources(),
        },
        LiveFormat {
            name: "uianim",
            version: regia_ui::anim_timeline::UIANIM_FORMAT_VERSION,
            fingerprint: regia_ui::anim_timeline::uianim_schema_fingerprint(),
            migration_sources: regia_ui::anim_timeline::uianim_migration_sources(),
        },
        // The asset id manifest (`.assetids.ron`, ADR-8460).
        LiveFormat {
            name: "asset-ids",
            version: regia_asset::asset_ids::ASSET_IDS_FORMAT_VERSION,
            fingerprint: regia_asset::asset_ids::asset_ids_schema_fingerprint(),
            migration_sources: regia_asset::asset_ids::asset_ids_migration_sources(),
        },
        LiveFormat {
            name: "data-table",
            version: regia_asset::data_table::DATA_TABLE_FORMAT_VERSION,
            fingerprint: regia_asset::data_table::data_table_schema_fingerprint(),
            migration_sources: regia_asset::data_table::data_table_migration_sources(),
        },
        // Custom data assets (`#[derive(Asset)]`, spec 10 § 2.4, ADR-7232).
        LiveFormat {
            name: "data-asset",
            version: regia_asset::data_asset::DATA_ASSET_FORMAT_VERSION,
            fingerprint: regia_asset::data_asset::data_asset_schema_fingerprint(),
            migration_sources: regia_asset::data_asset::data_asset_migration_sources(),
        },
        LiveFormat {
            name: "asset-tags",
            version: regia_asset::registry::TAG_RULES_FORMAT_VERSION,
            fingerprint: regia_asset::registry::tag_rules_schema_fingerprint(),
            migration_sources: regia_asset::registry::tag_rules_migration_sources(),
        },
        // Content Browser collections and favorites (ADR-7759).
        LiveFormat {
            name: "asset-collections",
            version: regia_asset::asset_collections::ASSET_COLLECTIONS_FORMAT_VERSION,
            fingerprint: regia_asset::asset_collections::asset_collections_schema_fingerprint(),
            migration_sources: regia_asset::asset_collections::asset_collections_migration_sources(),
        },
        // Named import presets per importer (ADR-7904).
        LiveFormat {
            name: "import-presets",
            version: regia_asset::import_presets::IMPORT_PRESETS_FORMAT_VERSION,
            fingerprint: regia_asset::import_presets::import_presets_schema_fingerprint(),
            migration_sources: regia_asset::import_presets::import_presets_migration_sources(),
        },
        LiveFormat {
            name: "asset-registry",
            version: regia_asset::registry::REGISTRY_FORMAT_VERSION,
            fingerprint: regia_asset::registry::registry_schema_fingerprint(),
            migration_sources: regia_asset::registry::registry_migration_sources(),
        },
        // The button-prompt glyph table (ADR-7070, v1 so far).
        LiveFormat {
            name: "glyphs",
            version: regia_ui::INPUT_GLYPHS_VERSION,
            fingerprint: regia_ui::input_glyphs_schema_fingerprint(),
            migration_sources: regia_ui::input_glyphs_migration_sources(),
        },
        LiveFormat {
            name: "input-overrides",
            version: regia_input::OVERRIDES_VERSION,
            fingerprint: regia_input::input_overrides_schema_fingerprint(),
            migration_sources: regia_input::input_overrides_migration_sources(),
        },
        LiveFormat {
            name: "permutations",
            version: regia_render::permutation_policy::PERMUTATION_MANIFEST_FORMAT_VERSION,
            fingerprint: regia_render::permutation_policy::permutation_manifest_schema_fingerprint(),
            migration_sources: regia_render::permutation_policy::permutation_manifest_migration_sources(),
        },
        LiveFormat {
            name: "convc",
            version: regia_framework::conversation::cooked::CONVC_VERSION,
            fingerprint: regia_framework::conversation::cooked::convc_schema_fingerprint(),
            migration_sources: regia_framework::conversation::cooked::convc_migration_sources(),
        },
        // AI and navigation: behavior trees (v2 adds value conditions, observer aborts and the
        // state-tree bridge; a file with no `format_version` is v1, ADR-2316, ADR-4543) and the cooked
        // nav cell (a hand-written layout, hashed from `NAV_CELL_LAYOUT`; v2 adds per-tile off-mesh links,
        // and the loader reads v1, ADR-2589).
        LiveFormat {
            name: "bt",
            version: regia_ai::tree::BT_FORMAT_VERSION,
            fingerprint: regia_ai::tree::bt_schema_fingerprint(),
            migration_sources: regia_ai::tree::bt_migration_sources(),
        },
        LiveFormat {
            name: "nav-cell",
            version: regia_nav::NAV_CELL_FORMAT_VERSION,
            fingerprint: regia_nav::tile_cache::nav_cell_schema_fingerprint(),
            migration_sources: regia_nav::tile_cache::nav_cell_migration_sources(),
        },
        // Painted terrain layer weights (`<map>.weights.bin`).
        LiveFormat {
            name: "terrain-weights",
            version: regia_terrain::LAYER_WEIGHTS_FORMAT_VERSION,
            fingerprint: regia_terrain::layer_paint::layer_weights_schema_fingerprint(),
            migration_sources: regia_terrain::layer_paint::layer_weights_migration_sources(),
        },
        // Painted grass density (`<map>.grassdensity.bin`, ADR-2411).
        LiveFormat {
            name: "grass-density",
            version: regia_terrain::GRASS_DENSITY_FORMAT_VERSION,
            fingerprint: regia_terrain::grass_density::grass_density_schema_fingerprint(),
            migration_sources: regia_terrain::grass_density::grass_density_migration_sources(),
        },
        // Non-destructive terrain edit layers (`<map>.terrain.editlayers`, ADR-7825).
        LiveFormat {
            name: "terrain-editlayers",
            version: regia_terrain::EDIT_LAYERS_FORMAT_VERSION,
            fingerprint: regia_terrain::edit_layers::edit_layers_schema_fingerprint(),
            migration_sources: regia_terrain::edit_layers::edit_layers_migration_sources(),
        },
        // The deterministic navmesh (`NVM2`, DO27's cooked mesh copied whole; enters at v2, ADR-8174).
        LiveFormat {
            name: regia_nav::det::navmesh::NAVMESH_FORMAT,
            version: regia_nav::det::navmesh::NAVMESH_VERSION,
            fingerprint: regia_nav::det::navmesh::navmesh_schema_fingerprint(),
            migration_sources: regia_nav::det::navmesh::navmesh_migration_sources(),
        },
        // Mesh layers and world geometry source (`<map>.meshlayers.ron` with `<map>.passages.ron`, spec 121
        // § 9.1, ADR-8112): v1, no migrator.
        LiveFormat {
            name: "terrain-mesh-layers",
            version: regia_world_geom::meshlayers::MESH_LAYERS_FORMAT_VERSION,
            fingerprint: mesh_layers_fingerprint(),
            migration_sources: Vec::new(),
        },
        // The cooked `<map>.meshlayers` (spec 121 § 9.2, ADR-8112): a cache, v1 in v0.6.0, v2 in v0.7. The
        // canonical section's own version moves with the file version. v2: the section stores a hashed head
        // and fixed-width counts (ADR-8816) and the chunk section has `collider` (ADR-8872); v1 (v0.6.0) loads
        // through `cooked::migrate_file` and the terrain crate's chunk migration.
        LiveFormat {
            name: "terrain-mesh-layers-cooked",
            version: regia_world_geom::cooked::COOKED_FORMAT_VERSION,
            fingerprint: mesh_layers_cooked_fingerprint(),
            migration_sources: regia_world_geom::cooked::cooked_migration_sources(),
        },
        // The cooked terrain index (`terrain/index`, v2 adds exact heights, holes and a photoreal
        // material, v3 the source grid's Z spacing and valid-data mask, ADR-8871) and its exact
        // final heights (`terrain/heights`), ADR-8024.
        LiveFormat {
            name: "terrain-index",
            version: regia_terrain::stream::TERRAIN_INDEX_FORMAT_VERSION,
            fingerprint: regia_terrain::terrain_index_schema_fingerprint(),
            migration_sources: regia_terrain::terrain_index_migration_sources().to_vec(),
        },
        LiveFormat {
            name: "terrain-final-heights",
            version: regia_terrain::TERRAIN_FINAL_HEIGHTS_FORMAT_VERSION,
            fingerprint: regia_terrain::final_heights_schema_fingerprint(),
            migration_sources: regia_terrain::final_heights_migration_sources().to_vec(),
        },
        // Prefab variants and the physical camera in scene files (ADR-2323).
        LiveFormat {
            name: "prefab-variant",
            version: regia_scene::prefab_variant::PREFAB_VARIANT_FORMAT_VERSION,
            fingerprint: regia_scene::prefab_variant::prefab_variant_schema_fingerprint(),
            migration_sources: regia_scene::prefab_variant::prefab_variant_migration_sources(),
        },
        // Level variants: a base level plus its differences (ADR-7823).
        LiveFormat {
            name: "level-variant",
            version: regia_scene::level_variant::LEVEL_VARIANT_FORMAT_VERSION,
            fingerprint: regia_scene::level_variant::level_variant_schema_fingerprint(),
            migration_sources: regia_scene::level_variant::level_variant_migration_sources(),
        },
        LiveFormat {
            name: "physical-camera",
            version: regia_render::PHYSICAL_CAMERA_FORMAT_VERSION,
            fingerprint: regia_render::physical_camera_schema_fingerprint(),
            migration_sources: regia_render::physical_camera_migration_sources(),
        },
        // Lens effects in scene files (ADR-3196).
        LiveFormat {
            name: "lens-effects",
            version: regia_render::LENS_EFFECTS_FORMAT_VERSION,
            fingerprint: regia_render::lens_effects_schema_fingerprint(),
            migration_sources: regia_render::lens_effects_migration_sources(),
        },
        // Profile captures (`.rprof.json`, ADR-2323).
        // Spec 54 § 4.4 (ADR-9070): diagnostic outputs, never rewritten by upgrade-project; a
        // layout change bumps the version and the reader keeps the old decoder.
        LiveFormat {
            name: "gpu-breadcrumbs",
            version: regia_render::breadcrumbs::REPORT_FORMAT_VERSION,
            fingerprint: regia_render::breadcrumbs::gpu_breadcrumbs_schema_fingerprint(),
            migration_sources: regia_render::breadcrumbs::gpu_breadcrumbs_migration_sources(),
        },
        LiveFormat {
            name: "regiatrace",
            version: regia_telemetry::trace::FORMAT_VERSION,
            fingerprint: regia_telemetry::trace::regiatrace_schema_fingerprint(),
            migration_sources: regia_telemetry::trace::regiatrace_migration_sources(),
        },
        LiveFormat {
            name: "profile-capture",
            version: regia_telemetry::profile_capture::PROFILE_CAPTURE_FORMAT_VERSION,
            fingerprint: regia_telemetry::profile_capture::profile_capture_schema_fingerprint(),
            migration_sources: regia_telemetry::profile_capture::profile_capture_migration_sources(),
        },
        // Cooked IES photometric profiles (`FIES`, ADR-2592).
        LiveFormat {
            name: "ies-profile",
            version: regia_asset::ies::IES_PROFILE_FORMAT_VERSION,
            fingerprint: regia_asset::ies::ies_profile_schema_fingerprint(),
            migration_sources: regia_asset::ies::ies_profile_migration_sources(),
        },
        // Baked particle-liquid runs (`.fluidcache`, binary, ADR-2441): the fingerprint is the layout text.
        LiveFormat {
            name: "fluidcache",
            version: regia_fluid::cache::FLUIDCACHE_FORMAT_VERSION,
            fingerprint: regia_fluid::cache::fluidcache_schema_fingerprint(),
            migration_sources: regia_fluid::cache::fluidcache_migration_sources(),
        },
        // The client's trust-on-first-use store (`known_hosts.ron`, ADR-2416).
        LiveFormat {
            name: "known-hosts",
            version: regia_net::known_hosts::KNOWN_HOSTS_FORMAT_VERSION,
            fingerprint: regia_net::formats::known_hosts_schema_fingerprint(),
            migration_sources: regia_net::formats::known_hosts_migration_sources(),
        },
        // Cooked video payloads (`regia_media::CookedVideo`, ADR-2410): the clip report's shape.
        LiveFormat {
            name: "cooked-video",
            version: regia_media::validate::COOKED_VIDEO_VERSION,
            fingerprint: regia_media::validate::cooked_video_schema_fingerprint(),
            migration_sources: regia_media::validate::cooked_video_migration_sources(),
        },
        // The project's provenance policy (`config/provenance.ron`, ADR-2428).
        LiveFormat {
            name: "provenance-policy",
            version: regia_asset_pipeline::provenance_policy::PROVENANCE_POLICY_FORMAT_VERSION,
            fingerprint: regia_asset_pipeline::provenance_policy::provenance_policy_schema_fingerprint(),
            migration_sources: regia_asset_pipeline::provenance_policy::provenance_policy_migration_sources(),
        },
        // Generator declarations (`generators/<name>.toml`, ADR-2412).
        LiveFormat {
            name: "generator",
            version: regia_asset_pipeline::generators::GENERATOR_FORMAT_VERSION,
            fingerprint: regia_asset_pipeline::generators::generator_schema_fingerprint(),
            migration_sources: regia_asset_pipeline::generators::generator_migration_sources(),
        },
        // Channel-pack recipes (`.texpack.ron`, ADR-3090): v1, no migrator yet.
        LiveFormat {
            name: "texpack",
            version: regia_asset_pipeline::texture_pack::TEXPACK_FORMAT_VERSION,
            fingerprint: regia_asset_pipeline::texture_pack::texpack_schema_fingerprint(),
            migration_sources: regia_asset_pipeline::texture_pack::texpack_migration_sources(),
        },
        // Entity kind declarations (`.kind.ron`, spec 101 § 2.3, ADR-2488): v1, no migrator yet.
        LiveFormat {
            name: "kind",
            version: regia_scene::kind::file::KIND_FORMAT_VERSION,
            fingerprint: regia_scene::kind::file::kind_schema_fingerprint(),
            migration_sources: regia_scene::kind::file::kind_migration_sources(),
        },
        // The GPU-ready mesh (`FGPM` pak entry, v2: submesh table, ADR-2788) and the cooked sound (`FSND`
        // pak entry: source bytes plus loudness record): caches like `curves-cooked`; a shape change
        // bumps the version and the pak is cooked again. v1 GPU-ready meshes still read.
        LiveFormat {
            name: "gpu-mesh",
            version: regia_asset::GPU_MESH_VERSION,
            fingerprint: regia_asset::gpu_mesh_schema_fingerprint(),
            migration_sources: regia_asset::gpu_mesh_migration_sources(),
        },
        LiveFormat {
            name: "cooked-sound",
            version: regia_audio::cooked_sound::COOKED_SOUND_VERSION,
            fingerprint: regia_audio::cooked_sound::cooked_sound_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // The cooked motion database payload (`motiondb-cooked`, spec 52 § 4, ADR-2409): a cache like
        // `curves-cooked`; a shape change bumps the version and cooked files are cooked again. The
        // loader still reads v1 payloads (ADR-2858).
        LiveFormat {
            name: "motiondb-cooked",
            version: regia_animation::motion_matching::COOKED_MOTIONDB_VERSION,
            fingerprint: regia_animation::motion_matching::cooked_motiondb_schema_fingerprint(),
            migration_sources: regia_animation::motion_matching::cooked_motiondb_migration_sources(),
        },
        // Product viewer contracts (ADR-2480, ADR-2535): JSON boundary formats whose shape is the JSON Schema
        // in docs/formats/jewelry-*.schema.json, so the fingerprint is the schema's RFC 8785 text. Version
        // is the `jewelry.<name>/<major>` major; a new major ships a migrator in `regia upgrade-project`.
        LiveFormat {
            name: regia_configurator::Interface::PartKit.format_name(),
            version: regia_configurator::CONTRACT_MAJOR,
            fingerprint: regia_configurator::Interface::PartKit.schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        LiveFormat {
            name: regia_configurator::Interface::MaterialLibrary.format_name(),
            version: regia_configurator::CONTRACT_MAJOR,
            fingerprint: regia_configurator::Interface::MaterialLibrary.schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        LiveFormat {
            name: regia_configurator::Interface::Build.format_name(),
            version: regia_configurator::CONTRACT_MAJOR,
            fingerprint: regia_configurator::Interface::Build.schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        LiveFormat {
            name: regia_configurator::Interface::ProductPackage.format_name(),
            version: regia_configurator::CONTRACT_MAJOR,
            fingerprint: regia_configurator::Interface::ProductPackage.schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // The product viewer's baked environment (`docs/formats/viewer-env.md`, ADR-2539): a baked output,
        // baked again from its source image on a layout change.
        LiveFormat {
            name: "viewer-env",
            version: regia_viewer::ENV_FORMAT_VERSION,
            fingerprint: regia_viewer::env::env_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // Product viewer packages: manifest.json plus mesh.rvm (cooked; re-cooked on a version change, ADR-2540).
        LiveFormat {
            name: "viewer-package",
            version: regia_viewer_pack::FORMAT_VERSION,
            fingerprint: regia_viewer_pack::schema_fingerprint(),
            migration_sources: regia_viewer_pack::migration_sources(),
        },
        // Cooked voxel far proxies (`FCKD`-wrapped `FVOX`, the importer's `voxel_far`, ADR-2445):
        // a binary layout, fingerprinted by its description (v4 adds the per-level coverage
        // estimate, ADR-3227; v1 to v3 migrate).
        LiveFormat {
            name: "voxel-far",
            version: regia_render::visbuffer::VOXEL_FAR_FORMAT_VERSION,
            fingerprint: regia_render::visbuffer::voxel_far_schema_fingerprint(),
            migration_sources: regia_render::visbuffer::voxel_far_migration_sources(),
        },
        // Agent benchmark tasks (`bench/agent_tasks/<id>.toml`, ADR-2621; owned by this crate).
        LiveFormat {
            name: "agent-task",
            version: crate::agent_bench::TASK_FORMAT_VERSION,
            fingerprint: crate::agent_bench::task_schema_fingerprint(),
            migration_sources: crate::agent_bench::task_migration_sources(),
        },
        // The CPU budget registry (`docs/perf/budgets.ron`, ADR-2704): a repository file `cargo xtask
        // perf-budgets` reads, never a project's. At v1 there is nothing to migrate.
        LiveFormat {
            name: "perf-budgets",
            version: crate::perf_budgets::PERF_BUDGETS_FORMAT_VERSION,
            fingerprint: crate::perf_budgets::perf_budgets_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // Baked GI volumes (`.fgiv`, ADR-206; v4 split section, ADR-2618): the postcard header's shape.
        LiveFormat {
            name: "baked-gi",
            version: regia_asset::baked_gi::BAKED_GI_VERSION,
            fingerprint: regia_asset::baked_gi_split::baked_gi_schema_fingerprint(),
            migration_sources: regia_asset::baked_gi_split::baked_gi_migration_sources(),
        },
        // Trailer configurations (`.trailer.ron`, ADR-3281); the chassis section is a vehicle config.
        LiveFormat {
            name: "trailer",
            version: regia_physics::vehicle_trailer_asset::TRAILER_FORMAT_VERSION,
            fingerprint: regia_physics::vehicle_trailer_asset::trailer_schema_fingerprint(),
            migration_sources: regia_physics::vehicle_trailer_asset::trailer_migration_sources(),
        },
        // Fixed-wing aircraft (`.aircraft.ron`, ADR-3374). At v1 there is nothing to migrate.
        LiveFormat {
            name: "aircraft",
            version: regia_physics::aero::asset::AIRCRAFT_FORMAT_VERSION,
            fingerprint: regia_physics::aero::asset::aircraft_schema_fingerprint(),
            migration_sources: regia_physics::aero::asset::aircraft_migration_sources(),
        },
        // Local reflection capture bakes (`.fcap`, ADR-2860).
        LiveFormat {
            name: "fcap",
            version: regia_asset::local_capture_bake::FCAP_FORMAT_VERSION,
            fingerprint: regia_asset::local_capture_bake::fcap_schema_fingerprint(),
            migration_sources: regia_asset::local_capture_bake::fcap_migration_sources(),
        },
        // The project bus tree `config/audio.ron` (`audio-buses`, ADR-8105 (g), ADR-8354): v0 is the
        // versionless shape v0.5.0 shipped; v1 adds the version, snapshots and effect bypass.
        LiveFormat {
            name: "audio-buses",
            version: regia_audio::AUDIO_BUSES_FORMAT_VERSION,
            fingerprint: regia_audio::bus_layout_schema_fingerprint(),
            migration_sources: regia_audio::audio_buses_migration_sources(),
        },
        // Vehicle sound definitions (`.vehicleaudio.ron`, ADR-3271).
        LiveFormat {
            name: "vehicle-audio",
            version: regia_audio::VEHICLE_AUDIO_VERSION,
            fingerprint: regia_audio::vehicle_audio_schema_fingerprint(),
            migration_sources: regia_audio::vehicle_audio_migration_sources(),
        },
        // Bake checkpoints (`.bakeck`, ADR-3095): binary and transient; a checkpoint of another
        // version is refused and the bake starts over, so there is nothing to migrate.
        LiveFormat {
            name: "bake-checkpoint",
            version: regia_asset::bake_checkpoint::BAKE_CHECKPOINT_FORMAT_VERSION,
            fingerprint: regia_asset::bake_checkpoint::bake_checkpoint_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // HLOD voxel tables of streamed levels with scatter (`.wvoxels` in `.fckd`, ADR-3253).
        LiveFormat {
            name: "hlod-voxels",
            version: regia_world::hlod_voxels::HLOD_VOXELS_FORMAT_VERSION,
            fingerprint: regia_world::hlod_voxels::hlod_voxels_schema_fingerprint(),
            migration_sources: regia_world::hlod_voxels::hlod_voxels_migration_sources(),
        },
        // Contact-sound banks (`.impactbank.ron`, ADR-3270).
        LiveFormat {
            name: "impactbank",
            version: regia_audio::impacts::IMPACT_BANK_FORMAT_VERSION,
            fingerprint: regia_audio::impacts::impactbank_schema_fingerprint(),
            migration_sources: regia_audio::impacts::impactbank_migration_sources(),
        },
        // Projected-decal prefabs (`.decal.ron`, v2 adds forward-decal fields, ADR-3255).
        LiveFormat {
            name: "decal",
            version: regia_vfx::decal_format::DECAL_FORMAT_VERSION,
            fingerprint: regia_vfx::decal_format::decal_schema_fingerprint(),
            migration_sources: regia_vfx::decal_format::decal_migration_sources(),
        },
        // The cell index of `regia bake-lightmaps --cells` (`<level>.cells/cells.json`, ADR-3318): a bake
        // cache; an index of another version is refused and every cell bakes again, so nothing migrates.
        LiveFormat {
            name: "lightmap-cells",
            version: regia_pt::lightmap::cells::CELL_BAKE_INDEX_VERSION,
            fingerprint: regia_pt::lightmap::cells::cell_bake_index_schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // Indoor cells and portals (`<map>.viscells.ron`, spec 25 § 7.7, ADR-7586): v1, nothing to migrate.
        LiveFormat {
            name: "viscells",
            version: regia_render::viscells::VISCELLS_FORMAT_VERSION,
            fingerprint: regia_render::viscells::viscells_schema_fingerprint(),
            migration_sources: regia_render::viscells::viscells_migration_sources(),
        },
        LiveFormat {
            name: "launch-profiles",
            version: regia_asset_pipeline::launch_profiles::FORMAT_VERSION,
            fingerprint: regia_asset_pipeline::launch_profiles::schema_fingerprint(),
            migration_sources: Vec::new(),
        },
        // Named formation slots (`.formation.ron` v1, ADR-8358). Version 1 is the first published shape.
        LiveFormat {
            name: "ai-formation",
            version: regia_ai::FORMATION_FORMAT_VERSION,
            fingerprint: regia_ai::formation_schema_fingerprint(),
            migration_sources: regia_ai::formation_migration_sources(),
        },
        // Key tracks (`.track.ron`, ADR-8403): v1, nothing to migrate.
        LiveFormat {
            name: "key-track",
            version: regia_cli::key_track::KEY_TRACK_FORMAT_VERSION,
            fingerprint: regia_cli::key_track::key_track_schema_fingerprint(),
            migration_sources: regia_cli::key_track::key_track_migration_sources(),
        },
        // Per-class delivery contract (`.contract.ron`, v1, ADR-8401). Nothing to migrate.
        LiveFormat {
            name: "delivery-contract",
            version: regia_asset::delivery_contract::DELIVERY_CONTRACT_FORMAT_VERSION,
            fingerprint: regia_asset::delivery_contract::delivery_contract_schema_fingerprint(),
            migration_sources: regia_asset::delivery_contract::delivery_contract_migration_sources(),
        },
        // Project source-lint policy (`config/source_lint.ron`, v1, ADR-8248). The evidence
        // report is versioned in the same fingerprint. No older policy version exists.
        LiveFormat {
            name: "source-lint",
            version: regia_asset_pipeline::source_lint::SOURCE_LINT_POLICY_VERSION,
            fingerprint: regia_asset_pipeline::source_lint::source_lint_schema_fingerprint(),
            migration_sources: regia_asset_pipeline::source_lint::source_lint_migration_sources(),
        },
        // Release chunks (`release-chunks.json`, v1, ADR-8404). Must merge before the v0.6 freeze.
        LiveFormat {
            name: "release-chunks",
            version: regia_cli::distribution::RELEASE_CHUNKS_FORMAT_VERSION,
            fingerprint: regia_cli::distribution::release_chunks_schema_fingerprint().to_owned(),
            migration_sources: Vec::new(),
        },
        // Named hover hulls (`.hoverhull.ron`, ADR-8512). v1 only; the host parses the text.
        LiveFormat {
            name: "hoverhull",
            version: regia_framework::det_hover::HOVERHULL_FORMAT_VERSION,
            fingerprint: regia_framework::det_hover::hoverhull_schema_fingerprint(),
            migration_sources: regia_framework::det_hover::hoverhull_migration_sources(),
        },
        // Logic authored on prefab and level entities (`regia_ai::logic::LogicGraphRef`, v1, P-165, ADR-8566).
        LiveFormat {
            name: "logic-graph-ref",
            version: regia_ai::logic::LOGIC_GRAPH_REF_FORMAT_VERSION,
            fingerprint: regia_ai::logic::logic_graph_ref_schema_fingerprint(),
            migration_sources: regia_ai::logic::logic_graph_ref_migration_sources(),
        },
        // Two-wheel bikes (`.bike.ron`, ADR-8630). v1 only; every config field is optional.
        LiveFormat {
            name: "bike",
            version: regia_physics::vehicle_bike::asset::BIKE_FORMAT_VERSION,
            fingerprint: regia_physics::vehicle_bike::asset::bike_schema_fingerprint(),
            migration_sources: regia_physics::vehicle_bike::asset::bike_migration_sources(),
        },
        // Deterministic utility catalogues (`.detutility.ron` v1, gap AI-7): v1, nothing to migrate.
        LiveFormat {
            name: regia_ai::det::DET_UTILITY_FORMAT,
            version: regia_ai::det::DET_UTILITY_FORMAT_VERSION,
            fingerprint: det_utility_schema_fingerprint(),
            migration_sources: regia_ai::det::det_utility_migration_sources(),
        },
        // Terrain source descriptors (ADR-8639): `heightfield.raster.json` (count-raster layout, ADR-8542) and
        // `heightfield.tiles.json` (a heightfield in several files, ADR-8595). Authored content the cook hashes;
        // v1, nothing to migrate. The overview descriptor (`<name>.overview.json`) is a cache and stays out.
        LiveFormat {
            name: "terrain-raster-descriptor",
            version: regia_terrain::source::RASTER_DESCRIPTOR_VERSION,
            fingerprint: regia_terrain::source::raster_descriptor_schema_fingerprint(),
            migration_sources: regia_terrain::source::raster_descriptor_migration_sources(),
        },
        LiveFormat {
            name: "terrain-source-tiles",
            version: regia_terrain::source::TILE_SET_VERSION,
            fingerprint: regia_terrain::source::tile_set_schema_fingerprint(),
            migration_sources: regia_terrain::source::tile_set_migration_sources(),
        },
        // Rule sets (`.rules.ron`, a match mode as data, ADR-8694): v1, the first version (v0.7 window).
        LiveFormat {
            name: "rules-ron",
            version: regia_framework::rules::ruleset::RULES_RON_FORMAT_VERSION,
            fingerprint: regia_framework::ruleset_file::rules_ron_schema_fingerprint(),
            migration_sources: regia_framework::ruleset_file::rules_ron_migration_sources(),
        },
        // Lobby hub config (`hub.toml`, v1, ADR-8648).
        LiveFormat {
            name: "hub-toml",
            version: regia_net::lobby::hub::HUB_TOML_FORMAT_VERSION,
            fingerprint: regia_net::lobby::hub::hub_toml_schema_fingerprint(),
            migration_sources: regia_net::lobby::hub::hub_toml_migration_sources(),
        },
        // Release manifest signature sidecar (`manifest-<platform>.json.sig`, v1, ADR-8687,
        // registered by ADR-8846). v1 is the first version.
        LiveFormat {
            name: "release-signature",
            version: regia_cli::distribution::RELEASE_SIGNATURE_FORMAT,
            fingerprint: regia_cli::distribution::release_signature_schema_fingerprint().to_owned(),
            migration_sources: Vec::new(),
        },
        // Gait settings (`.gait.ron` v1, gap G7). Version 1 is the first published shape.
        LiveFormat {
            name: regia_animation::gait_asset::GAIT_FORMAT,
            version: regia_animation::gait_asset::GAIT_FORMAT_VERSION,
            fingerprint: regia_animation::gait_asset::gait_schema_fingerprint(),
            migration_sources: regia_animation::gait_asset::gait_migration_sources(),
        },
    ];
    // The editor's persisted files (ADR-7902): their types live in the desktop-only `regia_editor`,
    // so the descriptors come from `regia_core::editor_formats` (kept equal to the types by
    // `regia_editor`'s `user_formats` tests).
    formats.extend(
        regia_core::editor_formats::EDITOR_FORMATS
            .iter()
            .map(|f| LiveFormat {
                name: f.name,
                version: f.version,
                fingerprint: f.fingerprint.to_owned(),
                migration_sources: f.migration_sources.to_vec(),
            }),
    );
    // Existing committed determinism fixture encoding; registration only (ADR-8231).
    formats.push(LiveFormat {
        name: "regia-det-golden-1",
        version: 1,
        fingerprint: "key=value;format=regia-det-golden-1;scenario=lowercase_name;seed=u64_hex_optional_0x;ticks=u32_decimal;rate_hz=u32_decimal;hash.<target>=u64_hex;trace_target=optional_target;t=repeated_u64_hex;hash=fnv1a64_le;f32=nan_canonical_zero_sign_preserved".into(),
        migration_sources: Vec::new(),
    });
    formats
}

/// `terrain-mesh-layers`: the document, every struct and enum it reaches, and the passages file it names
/// (whose Stage A layout types are part of the shape).
fn mesh_layers_fingerprint() -> String {
    use regia_core::schema::{serde_field_names as f, serde_variant_names as v, serde_variant_shapes as s};
    use regia_world_geom::compat::layout;
    use regia_world_geom::meshlayers as m;
    regia_core::schema::join_field_lists(&[
        f::<m::MeshLayersDocument>(),
        f::<regia_world_geom::GeomSettings>(),
        v::<m::GroundMode>(),
        s::<m::GroundMode>(),
        f::<m::SectionRule>(),
        f::<m::MeshLayerSource>(),
        v::<m::LayerSource>(),
        s::<m::LayerSource>(),
        f::<m::TunnelDetail>(),
        f::<m::BlockerSource>(),
        f::<m::LayerLook>(),
        f::<regia_world_geom::passages::PassagesDocument>(),
        f::<layout::TunnelLayout>(),
        f::<layout::Passage>(),
        v::<layout::PassageKind>(),
        f::<layout::Opening>(),
        f::<layout::Junction>(),
        f::<layout::CrawlwayVolume>(),
        f::<layout::SampledLayout>(),
        f::<layout::SampledPassage>(),
        f::<layout::Sample>(),
        f::<layout::MouthHole>(),
    ])
}

/// `terrain-mesh-layers-cooked`: the header, the canonical section and every struct and enum it reaches.
/// Postcard is positional, so the field lists stand for the field order.
fn mesh_layers_cooked_fingerprint() -> String {
    use regia_core::schema::{serde_field_names as f, serde_variant_names as v, serde_variant_shapes as s};
    use regia_world_geom::cooked as c;
    regia_core::schema::join_field_lists(&[
        vec![
            format!("magic={:?}", c::COOKED_MAGIC),
            format!("canonical_version={}", c::CANONICAL_VERSION),
        ],
        f::<c::MeshLayersHeader>(),
        f::<regia_world_geom::GeomSettings>(),
        v::<regia_world_geom::GroundMode>(),
        s::<regia_world_geom::GroundMode>(),
        f::<c::WorldGeomCooked>(),
        // The version 2 section (ADR-8816): the stored head, then the counts as little-endian `u16`,
        // hashed in blocks of this many bytes.
        f::<c::CanonicalHead>(),
        vec![
            "counts=u16le_after_head".to_string(),
            format!("count_block_bytes={}", c::COUNT_BLOCK_BYTES),
        ],
        f::<c::LatticeConstants>(),
        f::<c::VertexHeightMm>(),
        f::<c::CookedTile>(),
        f::<c::CookedTriangle>(),
        v::<c::TriKind>(),
        f::<c::CutBitmap>(),
        f::<c::TriRange>(),
        f::<c::CookedSection>(),
        f::<c::CookedPortal>(),
        f::<c::CookedZone>(),
        v::<c::CookedZoneShape>(),
        s::<c::CookedZoneShape>(),
        f::<c::CookedPlug>(),
        // The presentation section (plan row W-07, ADR-8175): the render meshes and mouth quads.
        vec![format!(
            "presentation_version={}",
            regia_world_geom::render::PRESENTATION_VERSION
        )],
        f::<regia_world_geom::render::PresentationCooked>(),
        f::<regia_world_geom::render::LayerRender>(),
        f::<regia_world_geom::render::RenderTriangle>(),
        v::<regia_world_geom::render::RenderPart>(),
        f::<regia_world_geom::render::RenderSource>(),
        f::<regia_world_geom::render::PassageRender>(),
        f::<regia_world_geom::render::MouthQuad>(),
        // The chunk section (ADR-8576): a fourth part, stored only when not empty, holding the
        // terrain's mesh-layer chunks (ADR-8398, ADR-8451).
        vec![
            "chunk_section=optional_after_presentation".to_string(),
            format!(
                "chunk_version={}",
                regia_terrain::mesh_layer::PRESENTATION_VERSION
            ),
        ],
        f::<regia_terrain::TerrainMeshLayersCooked>(),
        f::<regia_terrain::LayerChunkGrid>(),
        f::<regia_terrain::CookedLayer>(),
        f::<regia_terrain::CookedLayerChunk>(),
        f::<regia_terrain::CookedChunkLod>(),
    ])
}

/// BLAKE3 hex of a schema fingerprint.
fn hash(fingerprint: &str) -> String {
    blake3::hash(fingerprint.as_bytes()).to_hex().to_string()
}

/// Path of the committed hash table.
fn hashes_path(root: &Path) -> std::path::PathBuf {
    root.join("docs/formats/format-hashes.ron")
}

/// Loads the committed table; an error when the file is absent or does not parse.
fn load_committed(root: &Path) -> anyhow::Result<FormatHashes> {
    let path = hashes_path(root);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let text = std::str::from_utf8(&bytes).context("format-hashes.ron is not UTF-8")?;
    let hashes: FormatHashes = ron::from_str(text).context("parsing format-hashes.ron")?;
    Ok(hashes)
}

/// Computes the live table from the compiled crates.
fn compute_live() -> FormatHashes {
    let mut formats = BTreeMap::new();
    for f in live_formats() {
        formats.insert(
            f.name.to_string(),
            FormatEntry {
                version: f.version,
                hash: hash(&f.fingerprint),
            },
        );
    }
    FormatHashes { formats }
}

/// Compares one live format against its committed record and returns the problems found.
///
/// Detects: a missing record, a schema change without a version bump, a version bump (which needs
/// `--update` to re-record the hash), a version rollback (which `--update` records as a warned
/// action: the table holds no history, so a reverted bump cannot be told from an accidental
/// downgrade here — the corpus ratchet still demands the fixtures of the versions kept), and a
/// version bump that has no registered migrator from a version in the gap
/// (`docs/specs/12-assets.md` § 4a — a frozen format only changes together with a version bump
/// *and* a migrator).
fn check_format(name: &str, committed: Option<&FormatEntry>, live: &LiveFormat) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(entry) = committed else {
        errors.push(format!(
            "{name}: missing from format-hashes.ron (run --update to record it)"
        ));
        return errors;
    };
    let live_hash = hash(&live.fingerprint);
    // A bump with an unchanged shape must be recorded too: a table left at the old version would
    // let a later unbumped shape change pass as "version bumped".
    if entry.hash != live_hash || entry.version != live.version {
        if entry.version == live.version {
            errors.push(format!(
                "{name}: schema changed but version {} was not bumped; bump the format version \
                 and register a migrator (docs/specs/12-assets.md § 4a)",
                entry.version
            ));
        } else if live.version < entry.version {
            errors.push(format!(
                "{name}: version rolled back ({} -> {}); restore v{} or record the rollback \
                 knowingly with `cargo xtask check-formats --update`",
                entry.version, live.version, entry.version
            ));
        } else {
            errors.push(format!(
                "{name}: version bumped ({} -> {}); run `cargo xtask check-formats --update` to \
                 record the new schema hash after registering the migrator",
                entry.version, live.version
            ));
        }
    }
    // A version bump must ship with a migrator from every version in the gap.
    if live.version > entry.version {
        for v in entry.version..live.version {
            if !live.migration_sources.contains(&v) {
                errors.push(format!(
                    "{name}: version bumped from {} to {} but no migrator is registered from v{v}; \
                     register a step in the loader and in `regia upgrade-project`",
                    entry.version, live.version
                ));
            }
        }
    }
    errors
}

/// The problems `--update` still records, naming each as a warning so they cannot slip in
/// silently: a shape change without a bump, a bump without a migrator, and a version rollback
/// (recording is never refused — a new or provisional format needs it).
fn recording_warning(problem: &str) -> bool {
    problem.contains("not bumped") || problem.contains("no migrator") || problem.contains("rolled back")
}

/// The format table of the last tagged minor release, the baseline of the one-step-per-release rule
/// (ADR-7724). Moves to the new release's copy when that release is tagged
/// (`book/changing-a-content-format.md`, "One step per release"): v0.7.0 since its release commit
/// (ADR-10373), so the v0.8 line moves each format at most one step past what v0.7.0 ships. The v0.7 line
/// was held to one step past v0.6.0 (`docs/formats/format-hashes-v0.6.0.ron`, ADR-10183).
const RELEASE_BASELINE: &str = "docs/formats/format-hashes-v0.7.0.ron";

/// The v0.6.0 table, the v0.7 line's baseline (ADR-10183); kept in the tree with its own test.
#[cfg(test)]
const V060_BASELINE: &str = "docs/formats/format-hashes-v0.6.0.ron";

/// Compares the table about to ship with the table of the last release (`label` names it in the
/// messages) and returns the problems found (ADR-7724). A release moves each frozen format at most
/// one version step: a format at its released version must keep its released shape, a format one
/// step ahead may change shape again before the tag (the later change folds into the unreleased
/// version), and a format two or more steps ahead, one whose version went back, or one that is gone
/// from the registry fails. A format the release did not have is new and passes at any version.
fn check_since_release(baseline: &FormatHashes, live: &FormatHashes, label: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, shipped) in &baseline.formats {
        let Some(now) = live.formats.get(name) else {
            errors.push(format!(
                "{name}: v{} shipped in {label} but the format is gone from the registry; a released format \
                 keeps loading (restore its registry row and loader)",
                shipped.version
            ));
            continue;
        };
        if now.version < shipped.version {
            errors.push(format!(
                "{name}: version went back from v{} ({label}) to v{}",
                shipped.version, now.version
            ));
        } else if now.version == shipped.version && now.hash != shipped.hash {
            errors.push(format!(
                "{name}: shape changed since {label} without a version bump; bump v{} -> v{} (the one step this \
                 release may take) and register a migrator (docs/specs/12-assets.md § 4a)",
                shipped.version,
                shipped.version + 1
            ));
        } else if now.version > shipped.version + 1 {
            errors.push(format!(
                "{name}: v{} in {label}, v{} now: a release moves a format at most one version step; fold the later \
                 change into the unreleased v{} with an additive shape (ADR-7724, book/changing-a-content-format.md)",
                shipped.version,
                now.version,
                shipped.version + 1
            ));
        }
    }
    errors
}

/// Loads a release baseline: `since` is a file (absolute or relative to `root`) holding a format
/// table, or a git revision (a tag such as `v0.5.0`) whose `docs/formats/format-hashes.ron` is read.
/// Returns the table and the label the messages use.
fn load_baseline(root: &Path, since: &str) -> anyhow::Result<(FormatHashes, String)> {
    let path = root.join(since);
    let text = if path.is_file() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("show")
            .arg(format!("{since}:docs/formats/format-hashes.ron"))
            .output();
        match out {
            Ok(out) if out.status.success() => {
                String::from_utf8(out.stdout).context("the baseline table is not UTF-8")?
            }
            Ok(out) => anyhow::bail!(
                "--since-tag {since}: not a file and not a git revision with docs/formats/format-hashes.ron: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(err) => anyhow::bail!("--since-tag {since}: not a file, and git could not run: {err}"),
        }
    };
    let table: FormatHashes =
        ron::from_str(&text).with_context(|| format!("parsing the baseline table {since}"))?;
    Ok((table, since.to_string()))
}

/// Where the old-bytes corpus lives (`docs/specs/12-assets.md` § 4a, ADR-2743).
const CORPUS_DIR: &str = "crates/regia_agent/tests/fixtures/format-corpus";

/// The checked-in list of corpus versions that cannot have a fixture, each with its reason.
const CORPUS_EXCEPTIONS: &str = "docs/formats/format-corpus-exceptions.ron";

/// One `<format>/index.ron` of the corpus; only the fields the ratchet reads.
#[derive(Deserialize)]
struct CorpusIndex {
    format: String,
    entries: Vec<CorpusEntry>,
}

/// One fixture of a corpus index.
#[derive(Deserialize)]
struct CorpusEntry {
    version: u32,
    file: String,
}

/// A version with no corpus fixture, and why.
#[derive(Deserialize)]
struct CorpusException {
    format: String,
    /// The version without a fixture; `0` means every version of the format.
    version: u32,
    reason: String,
}

/// The exceptions file.
#[derive(Deserialize)]
struct CorpusExceptions {
    exceptions: Vec<CorpusException>,
}

/// The corpus as the ratchet reads it: format → (version, fixture file exists) per entry.
type CorpusVersions = BTreeMap<String, Vec<(u32, bool)>>;

/// Reads the corpus, plus the problems found reading it.
fn load_corpus(root: &Path) -> (CorpusVersions, Vec<String>) {
    let mut corpus = BTreeMap::new();
    let mut problems = Vec::new();
    let dir = root.join(CORPUS_DIR);
    let Ok(read) = std::fs::read_dir(&dir) else {
        problems.push(format!(
            "{}: the format corpus directory is missing",
            dir.display()
        ));
        return (corpus, problems);
    };
    for entry in read.flatten() {
        let folder = entry.path();
        let Ok(text) = std::fs::read_to_string(folder.join("index.ron")) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        match ron::from_str::<CorpusIndex>(&text) {
            Ok(index) => {
                if index.format != name {
                    problems.push(format!("{name}: index.ron names format {}", index.format));
                }
                let entries = index
                    .entries
                    .iter()
                    .map(|e| (e.version, folder.join(&e.file).is_file()))
                    .collect();
                corpus.insert(name, entries);
            }
            Err(err) => problems.push(format!("{name}: index.ron does not parse: {err}")),
        }
    }
    (corpus, problems)
}

/// The ratchet (ADR-2743): every registered format has a corpus fixture for its current version, and for
/// every older version from v1, unless `exceptions` lists that version with a reason. A bump therefore needs
/// a new fixture while the old current one stays. Also reports an entry whose file is missing, an entry above
/// the current version, and an exception that is no longer needed or has no reason.
fn check_corpus(
    formats: &[(&str, u32)],
    corpus: &CorpusVersions,
    exceptions: &[CorpusException],
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut used = vec![false; exceptions.len()];
    for &(name, current) in formats {
        let entries = corpus.get(name).map(Vec::as_slice).unwrap_or_default();
        for (version, exists) in entries {
            if !exists {
                errors.push(format!(
                    "{name}: the corpus lists v{version} but its file is missing"
                ));
            }
            if *version > current {
                errors.push(format!(
                    "{name}: the corpus has a v{version} fixture above the current v{current}"
                ));
            }
        }
        for v in 1..=current {
            if entries.iter().any(|(e, exists)| *e == v && *exists) {
                continue;
            }
            let excused = exceptions
                .iter()
                .position(|x| x.format == name && (x.version == v || x.version == 0));
            match excused {
                Some(i) => used[i] = true,
                None => errors.push(format!(
                    "{name}: no corpus fixture for v{v}; add `{CORPUS_DIR}/{name}/v{v}.<ext>` with an index.ron \
                     entry (v{current} is the current one; keep the older fixtures when you bump), or list it in \
                     {CORPUS_EXCEPTIONS} with a reason"
                )),
            }
        }
    }
    for (i, x) in exceptions.iter().enumerate() {
        if x.reason.trim().is_empty() {
            errors.push(format!(
                "{CORPUS_EXCEPTIONS}: the exception for {} has no reason",
                x.format
            ));
        }
        if !formats.iter().any(|(n, _)| *n == x.format) {
            errors.push(format!(
                "{CORPUS_EXCEPTIONS}: {} is not a registered format",
                x.format
            ));
        } else if !used[i] {
            errors.push(format!(
                "{CORPUS_EXCEPTIONS}: the exception for {} v{} is not needed (the fixture exists or the version is out of range); remove it",
                x.format, x.version
            ));
        }
    }
    errors
}

/// The corpus problems of the checked-out tree.
fn corpus_problems(root: &Path) -> Vec<String> {
    let (corpus, mut errors) = load_corpus(root);
    let exceptions = match std::fs::read_to_string(root.join(CORPUS_EXCEPTIONS)) {
        Ok(text) => match ron::from_str::<CorpusExceptions>(&text) {
            Ok(x) => x.exceptions,
            Err(err) => {
                errors.push(format!("{CORPUS_EXCEPTIONS} does not parse: {err}"));
                Vec::new()
            }
        },
        Err(err) => {
            errors.push(format!("{CORPUS_EXCEPTIONS}: {err}"));
            Vec::new()
        }
    };
    let live = live_formats();
    let formats: Vec<(&str, u32)> = live.iter().map(|f| (f.name, f.version)).collect();
    errors.extend(check_corpus(&formats, &corpus, &exceptions));
    errors
}

/// Writes the committed table through a sibling temp file and a rename, like the other
/// generated-table writers (`release_notes`, `adr_index`): an interrupted `--update` leaves the
/// previous table in place instead of a truncated one, and a stale `.tmp` from an earlier
/// interrupted run is consumed rather than left behind.
fn write_format_hashes(root: &Path, live: &FormatHashes) -> anyhow::Result<()> {
    let pretty =
        ron::ser::to_string_pretty(live, regia_core::ron_pretty()).context("serializing format hashes")?;
    let path = hashes_path(root);
    let tmp = path.with_extension("ron.tmp");
    std::fs::write(&tmp, format!("{pretty}\n")).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Run `check-formats`. `update` re-records the committed hashes; otherwise the check fails when a
/// shape changed without a version bump, a version bumped without a migrator, or a format moved more
/// than one version step since the last release (ADR-7724). `since` names that release's table (a
/// file or a git tag, `--since-tag`); `None` is the committed copy of the last release's table.
pub fn run(root: &Path, update: bool, since: Option<&str>) -> anyhow::Result<std::process::ExitCode> {
    let since = since.unwrap_or(RELEASE_BASELINE);
    if update {
        // Recording is never refused (a new or provisional format needs it), but a change the
        // plain check would reject — a shape change without a bump, a bump without a migrator, a
        // version rollback — is named, so it does not slip in through `--update`.
        if let Ok(committed) = load_committed(root) {
            for f in live_formats() {
                for problem in check_format(f.name, committed.formats.get(f.name), &f) {
                    if recording_warning(&problem) {
                        eprintln!("warning: recording anyway: {problem}");
                    }
                }
            }
        }
        let live = compute_live();
        write_format_hashes(root, &live)?;
        if let Ok((baseline, label)) = load_baseline(root, since) {
            for problem in check_since_release(&baseline, &live, &label) {
                eprintln!("warning: recording anyway: {problem}");
            }
        }
        println!("check-formats: wrote {} format hashes", live.formats.len());
        return Ok(std::process::ExitCode::SUCCESS);
    }

    let committed = match load_committed(root) {
        Ok(committed) => committed,
        Err(err) => {
            eprintln!("error: {err:#}");
            eprintln!("run `cargo xtask check-formats --update` to record the initial hashes");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let live = compute_live();

    let mut errors = Vec::new();
    for f in live_formats() {
        errors.extend(check_format(f.name, committed.formats.get(f.name), &f));
    }
    // Formats that disappeared entirely (removed from the registry) are also a shape change.
    for name in committed.formats.keys() {
        if !live.formats.contains_key(name) {
            errors.push(format!("{name}: format removed from the registry; run --update"));
        }
    }

    match load_baseline(root, since) {
        Ok((baseline, label)) => errors.extend(check_since_release(&baseline, &live, &label)),
        Err(err) => errors.push(format!("{err:#}")),
    }

    errors.extend(corpus_problems(root));
    // Designed formats with no crate type yet need their spec 12 § 4a row too (ADR-8019).
    let built: std::collections::BTreeSet<String> = live.formats.keys().cloned().collect();
    errors.extend(crate::proposed_formats::check(root, &built));

    if errors.is_empty() {
        println!("check-formats: {} formats up to date", live.formats.len());
        Ok(std::process::ExitCode::SUCCESS)
    } else {
        for err in &errors {
            eprintln!("error: {err}");
        }
        eprintln!("check-formats: {} problem(s)", errors.len());
        Ok(std::process::ExitCode::FAILURE)
    }
}

/// Serde field and variant lists of `.detutility.ron` (gap AI-7): the file struct and every struct and enum
/// reachable from it. The types live in `regia_detsim`, which has no `regia_core`, so the fingerprint is
/// built here.
fn det_utility_schema_fingerprint() -> String {
    use regia_ai::det::{ConsiderationFile, CurveFile, CurveKind, DetUtilityFile, InputDef, OptionFile};
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<DetUtilityFile>(),
        serde_field_names::<InputDef>(),
        serde_field_names::<OptionFile>(),
        serde_field_names::<ConsiderationFile>(),
        serde_field_names::<CurveFile>(),
        serde_variant_names::<CurveKind>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(version: u32, fingerprint: &str, sources: &[u32]) -> LiveFormat {
        LiveFormat {
            name: "test",
            version,
            fingerprint: fingerprint.to_string(),
            migration_sources: sources.to_vec(),
        }
    }

    fn committed(version: u32, fingerprint: &str) -> FormatEntry {
        FormatEntry {
            version,
            hash: hash(fingerprint),
        }
    }

    #[test]
    fn unchanged_format_passes() {
        let c = committed(1, "same");
        let l = live(1, "same", &[]);
        assert!(check_format("test", Some(&c), &l).is_empty());
    }

    #[test]
    fn schema_change_without_a_bump_fails() {
        let c = committed(1, "old-shape");
        let l = live(1, "new-shape", &[]);
        let errors = check_format("test", Some(&c), &l);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("not bumped"), "{}", errors[0]);
    }

    #[test]
    fn bump_without_a_migrator_fails() {
        // Version bumped 1 -> 2, but no migration source from v1 is registered.
        let c = committed(1, "shape");
        let l = live(2, "shape", &[]);
        let errors = check_format("test", Some(&c), &l);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("no migrator is registered from v1")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_bump_with_an_unchanged_shape_must_be_recorded() {
        // Before: no error, the table stayed at v1, and a later unbumped shape change compared v1
        // with v2 and was reported as an ordinary bump.
        let c = committed(1, "shape");
        let l = live(2, "shape", &[1]);
        let errors = check_format("test", Some(&c), &l);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("version bumped (1 -> 2)"), "{}", errors[0]);
        assert!(errors[0].contains("--update"), "{}", errors[0]);
    }

    /// The bump check above only looks at the gap between the committed and the live version. This
    /// one checks every live format's whole chain (ADR-1378): a format past v1 registers migrators,
    /// they run from the oldest registered version to the current one without a gap, and none starts
    /// at or above the current version.
    /// Formats that entered the engine past version 1, with the version they entered at: a game's
    /// shipped format copied whole, whose older versions were never written by Regia, so no migrator
    /// chain starts below it. Their history starts at that version (ADR-8174).
    const ENTERED_AT: &[(&str, u32)] = &[
        // DO27's cooked navmesh (`NVM2`), `regia_nav::det::navmesh` (ADR-8174).
        ("nav-det-navmesh", 2),
    ];

    #[test]
    fn every_live_format_has_a_contiguous_migrator_chain() {
        let mut problems = Vec::new();
        for format in live_formats() {
            let mut sources = format.migration_sources.clone();
            sources.sort_unstable();
            sources.dedup();
            let entered = ENTERED_AT
                .iter()
                .find(|(name, _)| *name == format.name)
                .map_or(1, |(_, v)| *v);
            let Some(&oldest) = sources.first() else {
                if format.version > entered {
                    problems.push(format!(
                        "{}: v{} but no migrator is registered",
                        format.name, format.version
                    ));
                }
                continue;
            };
            for v in oldest..format.version {
                if !sources.contains(&v) {
                    problems.push(format!("{}: no migrator from v{v}", format.name));
                }
            }
            if let Some(v) = sources.iter().find(|&&v| v >= format.version) {
                problems.push(format!(
                    "{}: a migrator from v{v}, at or above the current v{}",
                    format.name, format.version
                ));
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn bump_with_a_migrator_is_only_a_rehash_reminder() {
        let c = committed(1, "shape");
        let l = live(2, "new-shape", &[1]);
        let errors = check_format("test", Some(&c), &l);
        assert!(
            errors.iter().all(|e| !e.contains("no migrator")),
            "the migrator exists: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("--update")),
            "still needs rehash: {errors:?}"
        );
    }

    /// A version decrease is a rollback, not a bump (sprint-deep-zai-cli-tools finding 7): the
    /// message names it, so a reverted bump is never told to "record the new schema hash" as if
    /// the version had gone up.
    #[test]
    fn a_version_rollback_is_reported_as_a_rollback() {
        let c = committed(2, "shape");
        let l = live(1, "shape", &[]);
        let errors = check_format("test", Some(&c), &l);
        assert_eq!(errors.len(), 1, "{errors:?}");
        let text = &errors[0];
        assert!(text.contains("rolled back (2 -> 1)"), "{text}");
        assert!(!text.contains("bumped"), "a rollback is not a bump: {text}");
        // Recording it is still named as the escape hatch, and the warning arm says so.
        assert!(text.contains("--update"), "{text}");
        assert!(recording_warning(text), "recording a rollback is a warned action");
        assert!(recording_warning(
            "test: schema changed but version 1 was not bumped; bump the format version"
        ));
        assert!(recording_warning(
            "test: version bumped from 1 to 2 but no migrator is registered from v1"
        ));
        assert!(!recording_warning("test: missing from format-hashes.ron"));
    }

    /// `--update` replaces the committed table through a temp file and a rename, like the other
    /// generated writers (sprint-deep-zai-cli-tools finding 4): an interrupted run leaves the old
    /// table in place instead of a truncated one, and a stale `.tmp` from an earlier interrupted
    /// run is consumed rather than left behind.
    #[test]
    fn update_replaces_the_hash_table_through_a_tmp_rename() {
        let dir = regia_test_utils::unique_temp_dir("check-formats-atomic");
        std::fs::create_dir_all(dir.join("docs/formats")).expect("dirs");
        let table = hashes_path(&dir);
        std::fs::write(&table, "committed: old\n").expect("table");
        // A stale temp file from an interrupted earlier update.
        std::fs::write(table.with_extension("ron.tmp"), "stale half-write\n").expect("stale tmp");

        let mut live = FormatHashes {
            formats: BTreeMap::new(),
        };
        live.formats.insert(
            "example".to_string(),
            FormatEntry {
                version: 3,
                hash: hash("shape"),
            },
        );
        write_format_hashes(&dir, &live).expect("write");

        let text = std::fs::read_to_string(&table).expect("read");
        assert!(text.contains("example"), "{text}");
        assert!(!text.contains("old"), "{text}");
        assert!(
            !table.with_extension("ron.tmp").exists(),
            "the tmp file is renamed away, and a stale one does not survive either"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn corpus(entries: &[(&str, &[(u32, bool)])]) -> CorpusVersions {
        entries
            .iter()
            .map(|(n, v)| ((*n).to_string(), v.to_vec()))
            .collect()
    }

    fn exception(format: &str, version: u32, reason: &str) -> CorpusException {
        CorpusException {
            format: format.into(),
            version,
            reason: reason.into(),
        }
    }

    #[test]
    fn corpus_needs_every_version_up_to_the_current_one() {
        let c = corpus(&[("a", &[(1, true), (2, true)]), ("b", &[(1, true)])]);
        assert!(check_corpus(&[("a", 2), ("b", 1)], &c, &[]).is_empty());
    }

    #[test]
    fn a_bump_without_a_new_fixture_fails_and_the_old_one_must_stay() {
        let c = corpus(&[("a", &[(1, true)])]);
        let errors = check_corpus(&[("a", 2)], &c, &[]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("no corpus fixture for v2"), "{}", errors[0]);
        // The old current fixture was deleted on the bump: v1 is missing too.
        let c = corpus(&[("a", &[(2, true)])]);
        let errors = check_corpus(&[("a", 2)], &c, &[]);
        assert!(errors.iter().any(|e| e.contains("v1")), "{errors:?}");
    }

    #[test]
    fn a_format_with_no_fixtures_at_all_fails() {
        let errors = check_corpus(&[("a", 1)], &corpus(&[]), &[]);
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn an_exception_with_a_reason_excuses_one_version_or_all() {
        let c = corpus(&[("a", &[(2, true)])]);
        let one = [exception("a", 1, "no sample was ever checked in")];
        assert!(check_corpus(&[("a", 2)], &c, &one).is_empty());
        let all = [exception("b", 0, "binary pack needs a toolchain")];
        assert!(check_corpus(&[("b", 3)], &corpus(&[]), &all).is_empty());
    }

    #[test]
    fn exceptions_need_a_reason_and_must_still_be_needed() {
        let c = corpus(&[("a", &[(1, true)])]);
        let errors = check_corpus(&[("a", 1)], &c, &[exception("a", 1, "stale")]);
        assert!(errors.iter().any(|e| e.contains("not needed")), "{errors:?}");
        let errors = check_corpus(&[("a", 1)], &corpus(&[]), &[exception("a", 1, " ")]);
        assert!(errors.iter().any(|e| e.contains("no reason")), "{errors:?}");
        let errors = check_corpus(&[("a", 1)], &c, &[exception("zzz", 1, "typo")]);
        assert!(
            errors.iter().any(|e| e.contains("not a registered format")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_missing_file_or_a_future_entry_fails() {
        let c = corpus(&[("a", &[(1, false), (3, true)])]);
        let errors = check_corpus(&[("a", 2)], &c, &[]);
        assert!(errors.iter().any(|e| e.contains("file is missing")), "{errors:?}");
        assert!(
            errors.iter().any(|e| e.contains("above the current v2")),
            "{errors:?}"
        );
    }

    /// A table of `(name, version, fingerprint)` rows.
    fn table(rows: &[(&str, u32, &str)]) -> FormatHashes {
        FormatHashes {
            formats: rows
                .iter()
                .map(|&(name, version, fingerprint)| (name.to_string(), committed(version, fingerprint)))
                .collect(),
        }
    }

    #[test]
    fn a_release_that_moves_nothing_passes() {
        let shipped = table(&[("a", 3, "shape"), ("b", 1, "shape")]);
        assert!(check_since_release(&shipped, &shipped, "v0.5.0").is_empty());
    }

    #[test]
    fn a_shape_change_without_a_bump_since_the_release_fails() {
        // The plain check compares with the committed table, which `--update` re-records: a shape
        // change recorded that way passed it. The release baseline does not move with `--update`.
        let shipped = table(&[("a", 3, "shape")]);
        let now = table(&[("a", 3, "new-shape")]);
        let errors = check_since_release(&shipped, &now, "v0.5.0");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("shape changed since v0.5.0"), "{}", errors[0]);
        assert!(errors[0].contains("v3 -> v4"), "{}", errors[0]);
    }

    #[test]
    fn two_steps_in_one_release_fail() {
        let shipped = table(&[("a", 3, "shape")]);
        let now = table(&[("a", 5, "later-shape")]);
        let errors = check_since_release(&shipped, &now, "v0.5.0");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("v3 in v0.5.0, v5 now"), "{}", errors[0]);
        assert!(errors[0].contains("unreleased v4"), "{}", errors[0]);
    }

    #[test]
    fn one_step_passes_and_may_change_shape_again_before_the_tag() {
        let shipped = table(&[("a", 3, "shape")]);
        // A bump with an unchanged shape (a number-only bump), a bump with a new shape, and a
        // second change folded into the same unreleased version all look alike here.
        for later in ["shape", "v4-shape", "v4-shape-with-a-later-optional-field"] {
            let now = table(&[("a", 4, later)]);
            assert!(
                check_since_release(&shipped, &now, "v0.5.0").is_empty(),
                "{later}"
            );
        }
    }

    #[test]
    fn a_new_format_passes_and_a_removed_or_lowered_one_fails() {
        let shipped = table(&[
            ("kept", 2, "shape"),
            ("gone", 1, "shape"),
            ("lowered", 3, "shape"),
        ]);
        let now = table(&[
            ("kept", 2, "shape"),
            ("lowered", 2, "shape"),
            ("fresh", 1, "shape"),
        ]);
        let errors = check_since_release(&shipped, &now, "v0.5.0");
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors.iter().any(|e| e.starts_with("gone: v1 shipped in v0.5.0")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("lowered: version went back from v3")),
            "{errors:?}"
        );
    }

    fn repo_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    /// The gate runs `cargo xtask check-formats`, which compares the live table with the baseline;
    /// this checks the committed table (which the plain check keeps equal to the live one) without
    /// compiling every format crate's fingerprint, so `cargo test -p xtask` catches it too.
    #[test]
    fn the_committed_table_moves_each_format_at_most_one_step_since_the_release() {
        let root = repo_root();
        let committed = load_committed(&root).expect("docs/formats/format-hashes.ron");
        let (baseline, label) = load_baseline(&root, RELEASE_BASELINE).expect("the release baseline");
        assert!(
            baseline.formats.len() > 100,
            "{label}: {} rows; the baseline is a whole release table",
            baseline.formats.len()
        );
        let errors = check_since_release(&baseline, &committed, &label);
        assert!(errors.is_empty(), "{errors:#?}");
    }

    /// The baseline is the last tagged minor release (v0.7.0, ADR-10373), not an older one: the formats v0.7
    /// added (`gait`, `match-result`, ...) are "new" against v0.6.0's table, and the formats v0.7 moved one step
    /// (`logic` 2, `scene` 14, `nav-cell` 3, ...) could move a second step on the v0.8 line unseen against it.
    #[test]
    fn the_release_baseline_is_the_v070_table() {
        let (baseline, label) = load_baseline(&repo_root(), RELEASE_BASELINE).expect("the release baseline");
        assert!(label.ends_with("format-hashes-v0.7.0.ron"), "{label}");
        assert_eq!(
            baseline.formats.len(),
            247,
            "{label}: v0.7.0 ships 247 format rows"
        );
        for (name, shipped) in [
            ("logic", 2),
            ("playlist-ron", 2),
            ("terrain-index", 3),
            ("terrain-mesh-layers-cooked", 2),
            ("decal", 3),
            ("nav-cell", 3),
            ("scene", 14),
            ("gait", 1),
            ("match-result", 1),
        ] {
            let row = baseline.formats.get(name).map(|row| row.version);
            assert_eq!(row, Some(shipped), "{label}: `{name}` as v0.7.0 ships it");
        }
        // A format v0.7.0 moved one step, moved again on the next line, fails against this baseline.
        let mut live = FormatHashes {
            formats: baseline
                .formats
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        FormatEntry {
                            version: v.version,
                            hash: v.hash.clone(),
                        },
                    )
                })
                .collect(),
        };
        if let Some(row) = live.formats.get_mut("scene") {
            row.version = 16;
        }
        let errors = check_since_release(&baseline, &live, &label);
        assert!(
            errors.iter().any(|e| e.starts_with("scene: v14 in")),
            "{errors:#?}"
        );
    }

    /// The v0.6.0 table (the v0.7 line's baseline, ADR-10183) stays in the tree as v0.6.0 shipped it: the
    /// formats v0.6 added (`logic`, `playlist-ron`, `terrain-index`, `terrain-mesh-layers-cooked`) are "new"
    /// against v0.5.0's table, so a two-step move of one of them would pass unseen against the older one.
    #[test]
    fn the_v060_table_is_the_v07_line_baseline() {
        let (baseline, label) = load_baseline(&repo_root(), V060_BASELINE).expect("the v0.6.0 table");
        assert!(label.ends_with("format-hashes-v0.6.0.ron"), "{label}");
        assert_eq!(
            baseline.formats.len(),
            240,
            "{label}: v0.6.0 shipped 240 format rows"
        );
        for (name, shipped) in [
            ("logic", 1),
            ("playlist-ron", 1),
            ("terrain-index", 2),
            ("terrain-mesh-layers-cooked", 1),
            ("decal", 2),
            ("nav-cell", 2),
            ("scene", 13),
        ] {
            let row = baseline.formats.get(name).map(|row| row.version);
            assert_eq!(row, Some(shipped), "{label}: `{name}` as v0.6.0 shipped it");
        }
        // A format v0.6.0 added, moved two steps, fails against this baseline.
        let mut live = FormatHashes {
            formats: baseline
                .formats
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        FormatEntry {
                            version: v.version,
                            hash: v.hash.clone(),
                        },
                    )
                })
                .collect(),
        };
        if let Some(row) = live.formats.get_mut("logic") {
            row.version = 3;
        }
        let errors = check_since_release(&baseline, &live, &label);
        assert!(
            errors.iter().any(|e| e.starts_with("logic: v1 in")),
            "{errors:#?}"
        );
    }

    #[test]
    fn a_baseline_file_outside_the_tree_and_a_bad_name_are_read_or_refused() {
        let dir = regia_test_utils::unique_temp_dir("xtask-since-tag");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("baseline.ron");
        std::fs::write(
            &file,
            "// a header comment\n(formats: {\"a\": (version: 2, hash: \"00\")})\n",
        )
        .expect("write");
        let (table, label) =
            load_baseline(&repo_root(), &file.to_string_lossy()).expect("an absolute file path");
        assert_eq!(table.formats["a"].version, 2);
        assert_eq!(label, file.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        let Err(err) = load_baseline(&repo_root(), "no-such-file-or-tag-7724") else {
            panic!("a name that is neither a file nor a git revision must be refused");
        };
        assert!(
            format!("{err:#}").contains("--since-tag no-such-file-or-tag-7724"),
            "{err:#}"
        );
    }
}
