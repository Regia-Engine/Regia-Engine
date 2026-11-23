// SPDX-License-Identifier: MPL-2.0
//! The fuzz-target registry in the gate (ADR-1917, `docs/specs/97-fuzz-hardening.md`): every frozen format has a
//! target, every target's fixture loads and round trips, every target survives seeded mutation through the
//! time-boxed harness, and every mutation that loads holds the round-trip property.

use std::collections::BTreeMap;

use regia_agent::fuzz_targets::{self, FuzzInput, check_round_trip_bytes, check_round_trip_text};
use regia_test_utils::fuzz::{
    fuzz_headerless_loader, fuzz_loader, fuzz_text_loader, fuzz_text_values_loader, fuzz_tolerant_loader,
};

/// Frozen formats fuzzed by a test outside the registry: `(format, test file, test fn)`. `bake` lives in
/// `regia_pt`, an optional dependency of `regia_agent` (the offline path tracer). The binary save, pak, cooked
/// mesh and cooked clip need multi-part fixtures (a CRC frame, a TOC with entries, a cooked header) that the
/// per-crate tests already build and read past the parse (ADR-538). `luamod` and `luastate` live in
/// `regia_script_lua`, which depends on `regia_agent`, so the registry cannot reach them; `fracture` lives in the
/// editor-side `regia_asset_pipeline`, which the runtime registry must not depend on.
const COVERED_ELSEWHERE: &[(&str, &str, &str)] = &[
    // A datagram layout (spec 120 § 12.2, ADR-7985): the pinning test is the frozen-format check;
    // the `net.version_probe` target fuzzes its decoders as wire data.
    (
        "version-probe",
        "crates/regia_net/src/version_probe.rs",
        "probe_bytes_are_pinned",
    ),
    (
        "bake",
        "crates/regia_pt/tests/fuzz.rs",
        "bake_desc_ron_never_panics",
    ),
    (
        "save",
        "crates/regia_framework/tests/fuzz.rs",
        "save_payload_with_valid_crc_never_panics",
    ),
    (
        "pak",
        "crates/regia_asset/tests/fuzz.rs",
        "pak_with_entries_reads_never_panic",
    ),
    (
        "cooked-mesh",
        "crates/regia_asset/tests/fuzz.rs",
        "load_skinned_morphed_mesh_bytes_never_panics",
    ),
    (
        "cooked-clip",
        "crates/regia_animation/tests/fuzz.rs",
        "cooked_clip_never_panics",
    ),
    (
        "luamod",
        "crates/regia_script_lua/tests/fuzz.rs",
        "luamod_manifest_never_panics",
    ),
    // The Lua mod state in a save (ADR-1943) lives in `regia_script_lua` too.
    (
        "luastate",
        "crates/regia_script_lua/tests/fuzz.rs",
        "luastate_never_panics",
    ),
    // `.fracture.ron` lives in `regia_asset_pipeline`, only a dev-dependency here (the editor's crate).
    (
        "fracture",
        "crates/regia_asset_pipeline/tests/fuzz.rs",
        "fracture_ron_never_panics",
    ),
    // Frozen by the format-registry sweep (ADR-2316). `config/validation.ron` lives in `regia_asset_pipeline`
    // (a dev-dependency here); the XR files in `regia_xr` (optional here); `.session.ron` and the engine
    // release manifest in `regia_cli` (above this crate).
    (
        "validation",
        "crates/regia_asset_pipeline/tests/fuzz.rs",
        "validation_rules_never_panic",
    ),
    // `config/import_rules.ron` (ADR-3321) lives in `regia_asset_pipeline` (a dev-dependency here).
    (
        "import-rules",
        "crates/regia_asset_pipeline/tests/fuzz.rs",
        "import_rules_never_panic",
    ),
    (
        "xr-config",
        "crates/regia_xr/tests/fuzz.rs",
        "xr_config_never_panics",
    ),
    (
        "xr-actions",
        "crates/regia_xr/tests/fuzz.rs",
        "xr_actions_never_panic",
    ),
    ("xrsim", "crates/regia_xr/tests/fuzz.rs", "xrsim_never_panics"),
    (
        "session",
        "crates/regia_cli/tests/fuzz.rs",
        "session_ron_never_panics",
    ),
    (
        "engine-release",
        "crates/regia_cli/tests/fuzz.rs",
        "engine_release_ron_never_panics",
    ),
    // `config/provenance.ron` (ADR-2428): the policy type lives in the desktop-only pipeline.
    (
        "provenance-policy",
        "crates/regia_asset_pipeline/tests/fuzz.rs",
        "provenance_policy_never_panics",
    ),
    // `generators/<name>.toml` (ADR-2412) lives in `regia_asset_pipeline`, beside `.fracture.ron`.
    (
        "generator",
        "crates/regia_asset_pipeline/tests/fuzz.rs",
        "generator_toml_never_panics",
    ),
    // `.texpack.ron` channel-pack recipes (ADR-3090) live in the desktop-only pipeline.
    (
        "texpack",
        "crates/regia_asset_pipeline/tests/fuzz.rs",
        "texpack_ron_never_panics",
    ),
    (
        "viewer-package",
        "crates/regia_viewer_pack/src/mesh.rs",
        "every_truncation_and_bit_flip_is_refused_or_valid",
    ),
    (
        "baked-gi",
        "crates/regia_asset/tests/fuzz.rs",
        "fgiv_v4_split_part_never_panics",
    ),
    // Agent benchmark task files (`bench/agent_tasks/<id>.toml`, ADR-2621) live in xtask.
    (
        "agent-task",
        "xtask/src/agent_bench.rs",
        "agent_task_toml_never_panics",
    ),
    // The CPU budget registry (`docs/perf/budgets.ron`, ADR-2704) lives in xtask.
    (
        "perf-budgets",
        "xtask/src/perf_budgets.rs",
        "perf_budgets_ron_never_panics",
    ),
    // The cell index of `regia bake-lightmaps --cells` (ADR-3318).
    (
        "lightmap-cells",
        "crates/regia_pt/tests/fuzz.rs",
        "lightmap_cells_index_never_panics",
    ),
    // The editor's own files (ADR-7902): the editor crate is desktop-only, so its `baseline_fuzz` targets
    // run there.
    (
        "editor-keymap",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "editor-layout",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "editor-panel",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "editor-prefs",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "editor-profile",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "editor-theme",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "editor-view-state",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "editor-window-state",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "inspector-pins",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "viewport-snap",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    (
        "saved-searches",
        "crates/regia_editor/src/baseline_fuzz.rs",
        "the_baseline_targets_run_clean",
    ),
    // The asset package manifest (`.regia_assetpkg.ron`, ADR-7788).
    (
        "assetpkg",
        "crates/regia_asset_pipeline/src/asset_package.rs",
        "manifest_survives_mutation",
    ),
    (
        "launch-profiles",
        "crates/regia_asset_pipeline/tests/fuzz.rs",
        "launch_profiles_ron_never_panics",
    ),
    (
        "node-tracks",
        "crates/regia_animation/tests/node_tracks.rs",
        "node_tracks_never_panic_under_seeded_mutation",
    ),
    (
        "regia-det-golden-1",
        "crates/regia_detsim/tests/det_golden.rs",
        "golden_text_survives_mutation",
    ),
    (
        "key-track",
        "crates/regia_cli/tests/fuzz.rs",
        "key_track_ron_never_panics",
    ),
    (
        "source-lint",
        "crates/regia_asset_pipeline/tests/fuzz.rs",
        "source_lint_policy_never_panic",
    ),
    (
        "release-chunks",
        "crates/regia_cli/tests/fuzz.rs",
        "release_chunks_json_never_panics",
    ),
    (
        "netcapture",
        "crates/regia_net/src/capture.rs",
        "every_truncation_and_bit_flip_is_a_typed_error_or_a_clean_read",
    ),
    // The release-manifest signature sidecar (ADR-8687): its reader is `regia_release_chunks`, a
    // pipeline crate the runtime registry must not link (ADR-10356). `tests/fuzz.rs` there runs the
    // registry's three properties for it: syntax mutations, value mutations and the round trip.
    (
        "release-signature",
        "crates/regia_release_chunks/tests/fuzz.rs",
        "release_signature_never_panics",
    ),
];

/// Byte targets whose data starts with a free field, not a magic or length word, so a hostile leading word is
/// valid input: they skip only that canonical rejection.
const HEADERLESS: &[&str] = &[
    "net.input_bundle",
    // A connected packet body starts with its free 16-bit sequence number (ADR-2084).
    "net.payload",
    // Quantized fields fill their widths: any leading word is a value (ADR-3080).
    "net.vehicle_input",
    "net.vehicle_driver_input",
    "net.vehicle_state",
    "net.vehicle_seat_request",
    // An input bundle starts with its free first tick (ADR-8468).
    "net.input_bundle_budget",
];

/// Targets that drive a datagram handler rather than a decoder: the relay server answers malformed requests with an
/// error reply instead of failing, so "accepted" carries no meaning for the canonical rejections. The registry run in
/// `every_target_round_trips_what_it_accepts` still checks them for panics.
const HANDLERS: &[&str] = &[
    "net.relay_server",
    // ADR-2084: each relay request kind through the same handler, and the server endpoint (it answers or drops every
    // datagram; only the framing can fail).
    "net.relay.lookup",
    "net.relay.punch_intro",
    "net.relay.bind",
    "net.relay.publish",
    "net.relay.list_games",
    // Relay protocol 2 (ADR-7982): the allocation request, and the allocation port's handler (a refresh or bytes it
    // forwards).
    "net.relay.allocate",
    "net.relay.refresh",
    "net.server_endpoint",
];

/// Files built to survive a cut: a truncation is valid input (`fuzz_tolerant_loader`).
const CUT_TOLERANT: &[&str] = &[
    "media.webm",
    "telemetry.regiatrace",
    // The clear header of a sealed record: the ciphertext after it has no length word (the datagram frames it).
    "net.sealed_header",
    // `cargo test` output is a stream: a run cut short (cancelled, crashed) is valid input (ADR-2346).
    "agent.run_tests_output",
    // A `cargo build --message-format=json` stream is the same kind of stream (ADR-5707).
    "agent.build_game_output",
    // A Rust source read line by line: a cut one is as good as any (ADR-2821).
    "agent.golden_test_source",
    // `regia cook|package --json` output is a line stream too: a cancelled run is valid input (ADR-7217).
    "agent.cook_package_output",
];

/// How a format document's format is fuzzed.
enum Doc {
    /// A registry target.
    Target(&'static str),
    /// A test outside the registry: the test file and its `fn`.
    Test(&'static str, &'static str),
    /// Nothing reads it from outside the build: why.
    NotParsed(&'static str),
}

/// Every page of `docs/formats/`. A new page fails `every_format_doc_is_covered` until it is listed here, so an
/// unfrozen format cannot ship unfuzzed either.
const FORMAT_DOCS: &[(&str, Doc)] = &[
    // The product viewer contracts (ADR-2535): each owned interface has a target, and one page is the overview.
    ("jewelry-build", Doc::Target("jewelry.build")),
    (
        "jewelry-contracts",
        Doc::NotParsed("an overview of the four contract pages; it describes no file of its own"),
    ),
    (
        "jewelry-material-library",
        Doc::Target("jewelry.material_library"),
    ),
    ("jewelry-part-kit", Doc::Target("jewelry.part_kit")),
    ("jewelry-product-package", Doc::Target("jewelry.product_package")),
    ("ability-ron", Doc::Target("ability")),
    // `.cloth.ron`, `.soft.ron` and their cooked payloads (spec 55 § 4): each has its own target.
    ("cloth-ron", Doc::Target("cloth")),
    ("glossary-locglossary-ron", Doc::Target("glossary")),
    ("permutations", Doc::Target("render.permutations")),
    ("animedit-ron", Doc::Target("animedit")),
    ("asset-tags-ron", Doc::Target("asset.tag_rules")),
    ("asset-collections-ron", Doc::Target("asset.collections")),
    ("import-presets-ron", Doc::Target("asset.import_presets")),
    ("atlas-ron", Doc::Target("atlas")),
    ("attrs-ron", Doc::Target("attrs")),
    ("audio-cue-v1", Doc::Target("cue")),
    ("audio-patch-v1", Doc::Target("audiopatch")),
    ("audio-patch-v2", Doc::Target("audiopatch")),
    ("autotile-ron", Doc::Target("autotile")),
    (
        "bake-ron",
        Doc::Test("crates/regia_pt/tests/fuzz.rs", "bake_desc_ron_never_panics"),
    ),
    (
        "baked-gi",
        Doc::Test("crates/regia_asset/tests/fuzz.rs", "baked_gi_volume_never_panics"),
    ),
    ("bans-ron", Doc::Target("net.bans")),
    ("biome-ron", Doc::Target("biome")),
    ("camera-ron", Doc::Target("camera")),
    ("chooser-ron", Doc::Target("chooser")),
    ("chunk-why", Doc::Target("chunk-why")),
    ("content-host", Doc::Target("content-host")),
    ("content-state", Doc::Target("content-state")),
    ("content-signature", Doc::Target("content-signature")),
    ("content-trust", Doc::Target("content-trust")),
    ("content-manifest", Doc::Target("content-manifest")),
    ("contract-ron", Doc::Target("delivery-contract")),
    ("conversation-ron", Doc::Target("conversation")),
    (
        "cooked-mesh",
        Doc::Test(
            "crates/regia_asset/tests/fuzz.rs",
            "load_skinned_morphed_mesh_bytes_never_panics",
        ),
    ),
    (
        "cooked-pack-guide",
        Doc::NotParsed("a guide to formats fuzzed through their own pages"),
    ),
    ("curve-table-ron", Doc::Target("curves")),
    ("data-table-ron", Doc::Target("data-table")),
    ("assetids-ron", Doc::Target("asset-ids")),
    (
        "dcc-feed-protocol-v1",
        Doc::Test(
            "crates/regia_agent/tests/dcc_feed_hostile.rs",
            "mutated_frames_never_panic",
        ),
    ),
    ("decal-ron", Doc::Target("decal")),
    ("destructible", Doc::Target("destructible")),
    ("destructionfx-ron", Doc::Target("destructionfx")),
    ("dialogue-ron", Doc::Target("dialogue")),
    ("drivers-ron", Doc::Target("drivers")),
    ("effect-ron", Doc::Target("effect")),
    ("expected-ron", Doc::Target("expected-values")),
    ("extensions-ron", Doc::Target("extension.cooked")),
    (
        "fracture",
        Doc::Test(
            "crates/regia_asset_pipeline/tests/fuzz.rs",
            "fracture_ron_never_panics",
        ),
    ),
    ("frec", Doc::Target("frec")),
    ("netdemo", Doc::Target("netdemo")),
    ("functest-ron", Doc::Target("functest-script")),
    ("functional-test", Doc::Target("functional-test")),
    ("glass-crack", Doc::Target("glasscrack")),
    (
        "gltf-conventions",
        Doc::NotParsed("naming rules on top of glTF; the parser is fuzzed through gltf-extensions"),
    ),
    (
        "gltf-extensions",
        Doc::Test(
            "crates/regia_asset_pipeline/tests/gltf_extensions.rs",
            "extension_import_never_panics_on_mutated_glbs",
        ),
    ),
    // `render.breadcrumbs { source: "last_bundle" }` reads it back (ADR-9070).
    ("gpu-breadcrumbs-v1", Doc::Target("gpu-breadcrumbs")),
    ("grass-ron", Doc::Target("grass")),
    ("hub-toml", Doc::Target("net.hub_toml")),
    ("hull-set", Doc::Target("hull-set")),
    ("kind-ron", Doc::Target("kind")),
    ("ies-profile-v1", Doc::Target("ies-profile")),
    (
        "import-rules-ron",
        Doc::Test(
            "crates/regia_asset_pipeline/tests/fuzz.rs",
            "import_rules_never_panic",
        ),
    ),
    ("layer-file", Doc::Target("layer-file")),
    ("layers-ron", Doc::Target("layers")),
    ("level-dir", Doc::Target("level-dir.entity")),
    ("lightmap", Doc::Target("lightmap")),
    ("loopgen-ron", Doc::Target("loopgen")),
    ("loudness-ron", Doc::Target("loudness-table")),
    ("matgraph-ron", Doc::Target("matgraph")),
    // Finished-match records a host or the CLI reads back from a folder (audit gap GR-10.4).
    ("match-result", Doc::Target("match_result_json")),
    ("meta", Doc::Target("meta")),
    ("montage-ron", Doc::Target("montage")),
    ("montage-variant-ron", Doc::Target("montage-variant")),
    ("motiondb-ron", Doc::Target("motiondb")),
    ("music-ron", Doc::Target("music")),
    ("ambience-ron", Doc::Target("ambience")),
    ("nav-cell", Doc::Target("nav.cell")),
    // The deterministic navmesh (`NVM2`, ADR-8174).
    ("nav-det-navmesh", Doc::Target("nav.det_navmesh")),
    ("navlanes-ron", Doc::Target("navlanes")),
    ("navlinks-ron", Doc::Target("navlinks")),
    ("nav-link-anchor", Doc::Target("nav-link-anchor")),
    ("traversal-settings", Doc::Target("traversal-settings")),
    ("traversal-ladder", Doc::Target("traversal-ladder")),
    ("traversal-jump-point", Doc::Target("traversal-jump-point")),
    ("net-manifest-ron", Doc::Target("net.manifest")),
    ("net-ron", Doc::Target("net.settings")),
    ("netprofile-ron", Doc::Target("net.profile")),
    (
        "pak",
        Doc::Test(
            "crates/regia_asset/tests/fuzz.rs",
            "pak_with_entries_reads_never_panic",
        ),
    ),
    (
        "plant-wind",
        Doc::NotParsed("a vertex attribute layout inside a glTF; the engine does not read it yet"),
    ),
    ("playlist-ron", Doc::Target("net.playlist_ron")),
    ("rules-ron", Doc::Target("ruleset_ron")),
    ("post-volume-v1", Doc::Target("post-volume")),
    ("post-volume-v2", Doc::Target("post-volume")),
    ("post-volume-v3", Doc::Target("post-volume")),
    ("prefab-variant-v1", Doc::Target("prefab-variant")),
    ("physical-camera-v1", Doc::Target("physical-camera")),
    ("lens-effects-v1", Doc::Target("lens-effects")),
    ("level-variant-v1", Doc::Target("level-variant")),
    ("physmat", Doc::Target("physmat")),
    ("profile-capture-v1", Doc::Target("profile-capture")),
    ("profile-capture-v2", Doc::Target("profile-capture")),
    ("primitive-v1", Doc::Target("primitive")),
    ("ragdoll-ron", Doc::Target("ragdoll")),
    ("regia-plugin-manifest", Doc::Target("extension.manifest")),
    ("regiademo", Doc::Target("demo")),
    ("regiatrace-v1", Doc::Target("telemetry.regiatrace")),
    ("render-job-ron", Doc::Target("render-job")),
    ("retarget-ron", Doc::Target("retarget")),
    (
        "rfdrec",
        Doc::Test(
            "crates/regia_agent/tests/dcc_feed_hostile.rs",
            "mutated_recordings_never_panic",
        ),
    ),
    (
        "save-format",
        Doc::Test(
            "crates/regia_framework/tests/fuzz.rs",
            "save_payload_with_valid_crc_never_panics",
        ),
    ),
    ("scene-files", Doc::Target("scene")),
    ("seq-ron", Doc::Target("sequence")),
    ("server-key", Doc::Target("net.server_key")),
    ("server-toml", Doc::Target("net.server_toml")),
    // The dedicated server's kept player records, a save section (ADR-3354).
    ("server-players", Doc::Target("server-players")),
    ("shots-ron", Doc::Target("shots")),
    ("siminput-ron", Doc::Target("net.siminput")),
    ("desync-ron", Doc::Target("net.desync")),
    ("lsreplay", Doc::Target("net.lsreplay")),
    ("skelprofile-ron", Doc::Target("skelprofile")),
    ("slots-ron", Doc::Target("slots")),
    ("smartobject-ron", Doc::Target("smartobject")),
    ("spring-ron", Doc::Target("spring")),
    ("statetree-ron", Doc::Target("statetree")),
    ("logic-ron", Doc::Target("logic")),
    // Logic authored on prefab and level entities (P-165, ADR-8566).
    ("logic-graph-ref", Doc::Target("logic-graph-ref")),
    ("sync-ron", Doc::Target("sync")),
    ("tags-ron", Doc::Target("tags")),
    ("terrain", Doc::Target("terrain-layers")),
    ("test-report", Doc::Target("test-report")),
    ("theme-ron", Doc::Target("theme")),
    ("tilemap-ron", Doc::Target("tilemap")),
    ("tilepatterns-ron", Doc::Target("tilepatterns")),
    ("ui-ron", Doc::Target("ui")),
    ("uisettings-ron", Doc::Target("uisettings")),
    ("utility-ron", Doc::Target("utility")),
    (
        "validation-ron",
        Doc::Test(
            "crates/regia_asset_pipeline/tests/fuzz.rs",
            "validation_rules_never_panic",
        ),
    ),
    ("vehicle", Doc::Target("vehicle")),
    ("vehicle-snapshot", Doc::Target("vehicle-snapshot")),
    ("net-vehicle-wire", Doc::Target("net.vehicle_state")),
    ("vfx-ron", Doc::Target("vfx")),
    ("voice-chat-ron", Doc::Target("voice-chat")),
    ("weather-ron", Doc::Target("weather")),
    // `regia_xr` is an optional dependency of `regia_agent`, so the registry cannot reach it.
    (
        "xr-actions-ron",
        Doc::Test("crates/regia_xr/tests/fuzz.rs", "xr_actions_never_panic"),
    ),
    (
        "xr-config-ron",
        Doc::Test("crates/regia_xr/tests/fuzz.rs", "xr_config_never_panics"),
    ),
    (
        "xrsim-ron",
        Doc::Test("crates/regia_xr/tests/fuzz.rs", "xrsim_never_panics"),
    ),
    // `.wstream` and `.wcell` (spec 78 § 5): the page covers both; `cell-blob` has its own target.
    ("world-stream", Doc::Target("world-index")),
    // A glTF's cooked collision proxies (ADR-2341).
    ("collision-set", Doc::Target("collision-set")),
    ("prefab-instances", Doc::Target("prefab-instances")),
    // Pages added by the format-registry sweep (ADR-2316).
    ("bt-ron", Doc::Target("bt")),
    ("convc", Doc::Target("convc")),
    (
        "engine-release-ron",
        Doc::Test(
            "crates/regia_cli/tests/fuzz.rs",
            "engine_release_ron_never_panics",
        ),
    ),
    ("input-overrides-ron", Doc::Target("input.overrides")),
    ("lobby-keys", Doc::Target("net.lobby_keys")),
    ("version-probe", Doc::Target("net.version_probe")),
    (
        "session-ron",
        Doc::Test("crates/regia_cli/tests/fuzz.rs", "session_ron_never_panics"),
    ),
    ("terrain-weights", Doc::Target("terrain.weights")),
    ("uianim-ron", Doc::Target("uianim")),
    // Custom data assets (`#[derive(Asset)]`, ADR-7232).
    ("data-ron", Doc::Target("data-asset")),
    // Baked particle-liquid runs (ADR-2441).
    ("fluidcache", Doc::Target("fluidcache")),
    ("viewer-env", Doc::Target("viewer-env")),
    // The trust-on-first-use store (ADR-2416).
    ("known-hosts-ron", Doc::Target("net.known_hosts")),
    // The grass density file (ADR-2411).
    ("grass-density", Doc::Target("terrain.grass_density")),
    // The terrain edit-layers sidecar (ADR-7825).
    ("terrain-editlayers", Doc::Target("terrain.edit_layers")),
    (
        "viewer-package",
        Doc::Test(
            "crates/regia_viewer_pack/src/lib.rs",
            "manifest_mutations_never_panic",
        ),
    ),
    // Generator declarations (ADR-2412) and the provenance policy (ADR-2428) live in the desktop-only pipeline.
    (
        "generator-toml",
        Doc::Test(
            "crates/regia_asset_pipeline/tests/fuzz.rs",
            "generator_toml_never_panics",
        ),
    ),
    (
        "provenance-ron",
        Doc::Test(
            "crates/regia_asset_pipeline/tests/fuzz.rs",
            "provenance_policy_never_panics",
        ),
    ),
    // Pages of integ-v037's merged lanes (ADR-2562).
    (
        "do27replay",
        Doc::Test("crates/regia_framework/tests/fuzz.rs", "replay_never_panics"),
    ),
    ("ecology-ron", Doc::Target("ecology")),
    ("voxel-far", Doc::Target("voxel-far")),
    (
        "agent-task",
        Doc::Test("xtask/src/agent_bench.rs", "agent_task_toml_never_panics"),
    ),
    (
        "perf-budgets-ron",
        Doc::Test("xtask/src/perf_budgets.rs", "perf_budgets_ron_never_panics"),
    ),
    ("trailer", Doc::Target("trailer")),
    ("aircraft-ron", Doc::Target("aircraft")),
    // Local reflection capture bakes (ADR-2860).
    ("fcap", Doc::Target("fcap")),
    ("vehicle-audio", Doc::Target("vehicle-audio")),
    // The project bus tree `config/audio.ron` (ADR-8354).
    ("audio-buses-ron", Doc::Target("audio-buses")),
    // Bake checkpoints (ADR-3095).
    ("bake-checkpoint", Doc::Target("bake-checkpoint")),
    ("shot-ron", Doc::Target("shot")),
    // The contact-sound bank (ADR-3270).
    ("impactbank", Doc::Target("impactbank")),
    // The cell index of `regia bake-lightmaps --cells` (ADR-3318): `regia_pt` is optional here.
    (
        "lightmap-cells",
        Doc::Test(
            "crates/regia_pt/tests/fuzz.rs",
            "lightmap_cells_index_never_panics",
        ),
    ),
    // Channel-pack recipes (ADR-3090).
    (
        "texpack-ron",
        Doc::Test(
            "crates/regia_asset_pipeline/tests/fuzz.rs",
            "texpack_ron_never_panics",
        ),
    ),
    // The button-prompt glyph table and the `[runtime]` table of `Regia.toml` (ADR-7070).
    ("input-glyphs-ron", Doc::Target("glyphs")),
    (
        "regia-toml-runtime",
        Doc::Test(
            "crates/regia_framework/tests/post05_ui_scripts.rs",
            "regia_toml_runtime_keys_set_keep_screen_on_and_pause_on_focus_loss",
        ),
    ),
    // Data assets (ADR-7234): the shape lives in the editor crate, which the registry must not depend on.
    (
        "data-asset-ron",
        Doc::Test(
            "crates/regia_editor/src/data_asset.rs",
            "mutated_data_asset_text_never_panics",
        ),
    ),
    // ADR-7750: the baked procedural output page.
    ("pgout", Doc::Target("pgout")),
    // ADR-7902: the editor's own files.
    (
        "editor-keymap-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "editor-layout-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "editor-panel-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "editor-prefs-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "editor-profile-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "editor-theme-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "editor-view-state-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "editor-window-state-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "inspector-pins-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "viewport-snap-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    (
        "saved-searches-ron",
        Doc::Test(
            "crates/regia_editor/src/baseline_fuzz.rs",
            "the_baseline_targets_run_clean",
        ),
    ),
    // The asset package manifest (ADR-7788).
    (
        "assetpkg-ron",
        Doc::Test(
            "crates/regia_asset_pipeline/src/asset_package.rs",
            "manifest_survives_mutation",
        ),
    ),
    // Project launch profiles (ADR-7843). The page landed with the loader test and without this row.
    (
        "launch-profiles-ron",
        Doc::Test(
            "crates/regia_asset_pipeline/tests/fuzz.rs",
            "launch_profiles_ron_never_panics",
        ),
    ),
    (
        "node-tracks",
        Doc::Test(
            "crates/regia_animation/tests/node_tracks.rs",
            "node_tracks_never_panic_under_seeded_mutation",
        ),
    ),
    ("formation-ron", Doc::Target("ai.formation")),
    (
        "key-track-ron",
        Doc::Test("crates/regia_cli/tests/fuzz.rs", "key_track_ron_never_panics"),
    ),
    (
        "source-lint-ron",
        Doc::Test(
            "crates/regia_asset_pipeline/tests/fuzz.rs",
            "source_lint_policy_never_panic",
        ),
    ),
    (
        "release-chunks",
        Doc::Test(
            "crates/regia_cli/tests/fuzz.rs",
            "release_chunks_json_never_panics",
        ),
    ),
    // Named hover hulls (`.hoverhull.ron`, ADR-8512).
    ("hoverhull", Doc::Target("hoverhull_ron")),
    // Two-wheel bikes (`.bike.ron`, ADR-8630).
    ("bike-ron", Doc::Target("bike_ron")),
    // A deterministic utility catalogue (`.detutility.ron` v1, gap AI-7).
    ("ai-det-utility", Doc::Target("ai.det_utility")),
    (
        "netcapture",
        Doc::Test(
            "crates/regia_net/src/capture.rs",
            "every_truncation_and_bit_flip_is_a_typed_error_or_a_clean_read",
        ),
    ),
    // Release manifest signature sidecars (`.json.sig`, ADR-8846). The reader belongs to the
    // pipeline crate `regia_release_chunks`, which `regia_agent` must not link (ADR-10356), so its
    // fuzz tests live there (`tests/fuzz.rs`) and the registry names them.
    (
        "release-signature",
        Doc::Test(
            "crates/regia_release_chunks/tests/fuzz.rs",
            "release_signature_never_panics",
        ),
    ),
    // Gait settings (`.gait.ron` v1, gap G7).
    ("gait-ron", Doc::Target("anim.gait")),
    // The `[game]` table of `Regia.toml`: the pause key and its screen (ADR-10104).
    (
        "regia-toml-game",
        Doc::Test(
            "crates/regia_framework/src/game_pause/tests.rs",
            "mutated_game_table_never_panics",
        ),
    ),
];

/// Mutations per target through the time-boxed harness (one thread per call).
const HARNESS_ITERATIONS: usize = 120;
/// Well-formed value mutations per text target through the time-boxed harness.
const VALUE_ITERATIONS: usize = 200;
/// Mutations per target for the round-trip property (in-process, cheap).
const PROPERTY_ITERATIONS: u32 = 1000;

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[derive(serde::Deserialize)]
struct FormatHashes {
    formats: BTreeMap<String, ron::Value>,
}

fn frozen_formats() -> Vec<String> {
    let path = repo_root().join("docs/formats/format-hashes.ron");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let table: FormatHashes = ron::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    table.formats.into_keys().collect()
}

#[test]
fn every_frozen_format_has_a_fuzz_target() {
    let formats = frozen_formats();
    assert!(
        formats.len() >= 80,
        "format-hashes.ron lists {} formats",
        formats.len()
    );
    let mut missing = Vec::new();
    for format in &formats {
        if fuzz_targets::targets()
            .iter()
            .any(|t| t.format == Some(format.as_str()))
        {
            continue;
        }
        match COVERED_ELSEWHERE.iter().find(|(f, _, _)| f == format) {
            Some((_, file, function)) => {
                let text = std::fs::read_to_string(repo_root().join(file)).unwrap_or_default();
                assert!(
                    text.contains(&format!("fn {function}()")),
                    "{format}: {file} has no `fn {function}()`"
                );
            }
            None => missing.push(format.clone()),
        }
    }
    assert!(
        missing.is_empty(),
        "frozen formats without a fuzz target (add one to regia_agent::fuzz_targets): {missing:?}"
    );
    // The book's example answer and the spec's acceptance line quote these.
    let in_registry = formats
        .iter()
        .filter(|format| {
            fuzz_targets::targets()
                .iter()
                .any(|t| t.format == Some(format.as_str()))
        })
        .count();
    assert_eq!(in_registry + COVERED_ELSEWHERE.len(), formats.len());
    eprintln!(
        "{} targets, {in_registry} of {} frozen formats",
        fuzz_targets::targets().len(),
        formats.len()
    );
    // Every registry `format` names a real frozen format (no typos hiding a gap).
    for t in fuzz_targets::targets() {
        if let Some(format) = t.format {
            assert!(
                formats.iter().any(|f| f == format),
                "{}: unknown format {format}",
                t.name
            );
        }
    }
}

#[test]
fn every_format_doc_is_covered() {
    let dir = repo_root().join("docs/formats");
    let mut pages: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".md").map(str::to_string))
        .collect();
    pages.sort();
    let listed: Vec<&str> = FORMAT_DOCS.iter().map(|(page, _)| *page).collect();
    let missing: Vec<&String> = pages.iter().filter(|p| !listed.contains(&p.as_str())).collect();
    assert!(
        missing.is_empty(),
        "format docs with no entry in FORMAT_DOCS (add a target or a test): {missing:?}"
    );
    for (page, coverage) in FORMAT_DOCS {
        assert!(
            pages.iter().any(|p| p == page),
            "{page}: no such page in docs/formats"
        );
        match coverage {
            Doc::Target(name) => {
                assert!(fuzz_targets::target(name).is_some(), "{page}: no target {name}");
            }
            Doc::Test(file, function) => {
                let text = std::fs::read_to_string(repo_root().join(file)).unwrap_or_default();
                assert!(
                    text.contains(&format!("fn {function}()")),
                    "{page}: {file} has no `fn {function}()`"
                );
            }
            Doc::NotParsed(why) => assert!(!why.is_empty(), "{page}: say why"),
        }
    }
}

#[test]
fn every_target_fixture_loads_and_round_trips() {
    let mut problems = Vec::new();
    for t in fuzz_targets::targets() {
        let outcome = match t.input {
            FuzzInput::Text { fixture, load } => load(&fixture()).and_then(|canonical| {
                if t.round_trip {
                    check_round_trip_text(load, &canonical)
                } else {
                    Ok(())
                }
            }),
            FuzzInput::Bytes { fixture, load } => load(&fixture()).and_then(|canonical| {
                if t.round_trip {
                    check_round_trip_bytes(load, &canonical)
                } else {
                    Ok(())
                }
            }),
        };
        if let Err(e) = outcome {
            problems.push(format!("{}: {e}", t.name));
        }
    }
    assert!(
        problems.is_empty(),
        "fixtures that do not load or round trip:\n{}",
        problems.join("\n")
    );
}

/// Runs `check` once per target and fails with every target that panicked, not only the first: one bad target must
/// not hide the rest.
fn for_every_target(check: impl Fn(usize, &fuzz_targets::FuzzTarget)) {
    let mut failed = Vec::new();
    for (i, t) in fuzz_targets::targets().iter().enumerate() {
        eprintln!("fuzzing {}", t.name);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(i, t)));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "(non-string panic payload)".to_string());
            failed.push(format!("{}: {message}", t.name));
        }
    }
    assert!(failed.is_empty(), "targets that failed:\n{}", failed.join("\n"));
}

#[test]
fn every_target_survives_mutation() {
    for_every_target(|i, t| {
        if HANDLERS.contains(&t.name) {
            return;
        }
        let seed = 0x5EED_0000 + i as u64;
        match t.input {
            FuzzInput::Text { fixture, load } => {
                fuzz_text_loader(&fixture(), seed, HARNESS_ITERATIONS, load);
            }
            FuzzInput::Bytes { fixture, load } if CUT_TOLERANT.contains(&t.name) => {
                fuzz_tolerant_loader(&fixture(), seed, HARNESS_ITERATIONS, load);
            }
            FuzzInput::Bytes { fixture, load } if HEADERLESS.contains(&t.name) => {
                fuzz_headerless_loader(&fixture(), seed, HARNESS_ITERATIONS, load);
            }
            FuzzInput::Bytes { fixture, load } => {
                fuzz_loader(&fixture(), seed, HARNESS_ITERATIONS, load);
            }
        }
    });
}

#[test]
fn every_text_target_survives_value_mutation() {
    for_every_target(|i, t| {
        if let FuzzInput::Text { fixture, load } = t.input {
            fuzz_text_values_loader(&fixture(), 0xA1_0000 + i as u64, VALUE_ITERATIONS, load);
        }
    });
}

#[test]
fn every_target_round_trips_what_it_accepts() {
    let mut problems = Vec::new();
    let mut accepted = 0u32;
    for (i, t) in fuzz_targets::targets().iter().enumerate() {
        let name = t.name;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let target = fuzz_targets::target(name).expect("registered");
            let _ = tx.send(fuzz_targets::run(
                target,
                None,
                0xF0_2200 + i as u64,
                PROPERTY_ITERATIONS,
            ));
        });
        // A guard against a hang only; the counts decide the result, not the clock. Sized to the
        // slowest target, not to a typical one: `matgraph-wind` validates every host shader per
        // iteration, measured at 0.35 s of CPU each, so 1000 iterations are about 350 s of real work.
        let report = rx
            .recv_timeout(std::time::Duration::from_secs(1800))
            .unwrap_or_else(|_| panic!("{name}: the run did not finish (possible hang)"))
            .expect("built-in fixture");
        accepted += report.accepted;
        eprintln!(
            "{:<24} accepted {:>5} rejected {:>5} findings {}",
            t.name,
            report.accepted,
            report.rejected,
            report.panicked + report.round_trip_failures + report.empty_errors
        );
        if !report.is_clean() {
            problems.push(format!("{}: {report:?}", t.name));
        }
    }
    assert!(
        accepted > 0,
        "no mutation loaded: the property was never exercised"
    );
    assert!(
        problems.is_empty(),
        "targets with findings:\n{}",
        problems.join("\n")
    );
}

/// The ADR-1917 finding: without serde_json's `float_roundtrip` feature, `1e-45` parsed as
/// `1.0000000000000001e-45`, so a float in an agent request arrived one ulp off and re-encoded differently.
#[test]
fn json_numbers_parse_correctly_rounded() {
    // The standard library's float parser is correctly rounded: it is the reference.
    for text in [
        "1e-45",
        "0.1",
        "2.2250738585072011e-308",
        "1.7976931348623157e308",
        "3.4028235e38",
        "-300.0",
    ] {
        let value: f64 = text.parse().expect("a float");
        let parsed: f64 = serde_json::from_str(text).expect("a JSON number");
        assert_eq!(parsed.to_bits(), value.to_bits(), "{text}");
        let again: f64 =
            serde_json::from_str(&serde_json::to_string(&parsed).expect("encode")).expect("decode");
        assert_eq!(again.to_bits(), parsed.to_bits(), "{text} does not round trip");
    }
}

/// Fixtures copied into the crate (`cargo package` copies only the crate folder, ADR-1485) and the checked-in examples
/// they copy: `(fixture file name, source relative to the repo root)`. They must stay byte-identical.
const COPIED_FIXTURES: &[(&str, &str)] = &[
    ("meta.example.ron", "docs/formats/meta.example.ron"),
    ("material-v3.example.ron", "docs/formats/material-v3.example.ron"),
    ("glasscrack.example.ron", "docs/formats/glasscrack.example.ron"),
    (
        "data-table.example.table.ron",
        "docs/formats/data-table.example.table.ron",
    ),
    ("level.example.ron", "docs/formats/level.example.ron"),
    (
        "ui.example.ui.ron",
        "crates/regia_ui/tests/fixtures/ui.example.ui.ron",
    ),
    (
        "ui.v6.example.ui.ron",
        "crates/regia_ui/tests/fixtures/ui.v6.example.ui.ron",
    ),
    (
        "ui.v7.example.ui.ron",
        "crates/regia_ui/tests/fixtures/ui.v7.example.ui.ron",
    ),
    (
        "ui.components.example.ui.ron",
        "crates/regia_ui/tests/fixtures/ui.components.example.ui.ron",
    ),
    (
        "ui.v9.example.ui.ron",
        "crates/regia_ui/tests/fixtures/ui.v9.example.ui.ron",
    ),
    (
        "ui.v11-designer.example.ui.ron",
        "crates/regia_ui/tests/fixtures/ui.v11-designer.example.ui.ron",
    ),
    (
        "ui.v11-parity.example.ui.ron",
        "crates/regia_ui/tests/fixtures/ui.v11-parity.example.ui.ron",
    ),
    (
        "ui.example.uisettings.ron",
        "crates/regia_ui/tests/fixtures/ui.example.uisettings.ron",
    ),
    (
        "theme.example.theme.ron",
        "crates/regia_ui/tests/fixtures/theme.example.theme.ron",
    ),
    (
        "muzzle_flash.vfx.ron",
        "crates/regia_vfx/tests/fixtures/muzzle_flash.vfx.ron",
    ),
    ("decal.example.ron", "docs/formats/decal.example.ron"),
    (
        "locomotion.animgraph.ron",
        "docs/formats/locomotion.animgraph.ron",
    ),
    ("chain_ik.animgraph.ron", "docs/formats/chain_ik.animgraph.ron"),
    (
        "locomotion_warp.animgraph.ron",
        "docs/formats/locomotion_warp.animgraph.ron",
    ),
    (
        "arm.drivers.ron",
        "crates/regia_animation/tests/fixtures/arm.drivers.ron",
    ),
    (
        "impact_flesh.cue.ron",
        "crates/regia_audio/tests/fixtures/impact_flesh.cue.ron",
    ),
    (
        "hover_engine.audiopatch.ron",
        "crates/regia_audio/tests/fixtures/hover_engine.audiopatch.ron",
    ),
    ("loopgen.example.ron", "docs/formats/loopgen.example.ron"),
    (
        "ground.terrain.layers.ron",
        "crates/regia_terrain/tests/fixtures/ground.terrain.layers.ron",
    ),
    (
        "rocks.scatter.ron",
        "crates/regia_terrain/tests/fixtures/rocks.scatter.ron",
    ),
    ("matgraph.example.ron", "docs/formats/matgraph.example.ron"),
    ("matinst.example.ron", "docs/formats/matinst.example.ron"),
    ("tilemap.example.ron", "docs/formats/tilemap.example.ron"),
    (
        "tilepatterns.example.ron",
        "docs/formats/tilepatterns.example.ron",
    ),
    ("atlas.example.ron", "docs/formats/atlas.example.ron"),
    ("shots.example.ron", "docs/formats/shots.example.ron"),
    ("seq.example.ron", "docs/formats/seq.example.ron"),
    ("tags.example.ron", "docs/formats/tags.example.ron"),
    ("statetree.example.ron", "docs/formats/statetree.example.ron"),
    ("logic.example.ron", "docs/formats/logic.example.ron"),
    ("render-job.example.ron", "docs/formats/render-job.example.ron"),
    (
        "lightmap-v1.flmap",
        "crates/regia_asset/tests/fixtures/lightmap-v1.flmap",
    ),
    ("autotile.example.ron", "docs/formats/autotile.example.ron"),
    ("utility.example.ron", "docs/formats/utility.example.ron"),
    ("shot.example.ron", "docs/formats/shot.example.ron"),
    (
        "terrain-source-tiles.example.json",
        "docs/formats/terrain-source-tiles.example.json",
    ),
];

#[test]
fn copied_fixtures_match_their_sources() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fuzz_targets/fixtures");
    for (name, source) in COPIED_FIXTURES {
        let copy = std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let original = std::fs::read(repo_root().join(source)).unwrap_or_else(|e| panic!("{source}: {e}"));
        assert!(copy == original, "{name} differs from {source}: copy it again");
    }
}

/// The soak (ADR-2084): every registry target over many seeds, `REGIA_FUZZ_SOAK_SEEDS` seeds (default 3000) of
/// `REGIA_FUZZ_SOAK_ITERATIONS` mutations each (default 4), on `REGIA_FUZZ_SOAK_THREADS` threads (default 4), and
/// optionally only the targets whose name starts with `REGIA_FUZZ_SOAK_PREFIX`; `REGIA_FUZZ_SOAK_FIRST_SEED` (default 1)
/// moves the seed range so a second pass covers new inputs; `REGIA_FUZZ_SOAK_WALK` (default 0) first walks the fixture
/// that many mutations away, keeping each mutation that still loads (see [`walk`]), so the run mutates a valid input
/// unlike the fixture. Every finding is listed with the
/// seed that reproduces it (`fuzz.run` with that target, seed and iteration count). Ignored in the gate (it runs for
/// minutes); `cargo test -p regia_agent --test fuzz_targets -- --ignored soak --nocapture`.
#[test]
#[ignore = "long soak; run by hand"]
fn soak_every_target_over_many_seeds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let var = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
    };
    let seeds = var("REGIA_FUZZ_SOAK_SEEDS", 3000);
    let first = var("REGIA_FUZZ_SOAK_FIRST_SEED", 1);
    let iterations = u32::try_from(var("REGIA_FUZZ_SOAK_ITERATIONS", 4)).unwrap_or(4);
    let walk_steps = usize::try_from(var("REGIA_FUZZ_SOAK_WALK", 0)).unwrap_or(0);
    let threads = usize::try_from(var("REGIA_FUZZ_SOAK_THREADS", 4))
        .unwrap_or(4)
        .max(1);
    let prefix = std::env::var("REGIA_FUZZ_SOAK_PREFIX").unwrap_or_default();
    let chosen: Vec<&'static fuzz_targets::FuzzTarget> = fuzz_targets::targets()
        .iter()
        .filter(|t| t.name.starts_with(&prefix))
        .collect();
    let next = AtomicUsize::new(0);
    let rows = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(t) = chosen.get(i) else { break };
                    let mut total = fuzz_targets::FuzzReport::default();
                    let mut findings = Vec::new();
                    for seed in first..first.saturating_add(seeds) {
                        let base = match walk(t, seed, walk_steps) {
                            Ok(base) => base,
                            Err(panic) => {
                                findings.push(format!(
                                    "{} seed {seed} walk {walk_steps}: panic: {panic}",
                                    t.name
                                ));
                                continue;
                            }
                        };
                        let report =
                            fuzz_targets::run(t, base.as_deref(), seed, iterations).expect("fixture");
                        if !report.is_clean()
                            && let Some(base) = &base
                        {
                            let path = std::env::temp_dir()
                                .join(format!("regia-soak-{}-{seed}-{walk_steps}.fixture", t.name));
                            if std::fs::write(&path, base).is_ok() {
                                eprintln!(
                                    "soak {}: the walked fixture of seed {seed} is {}",
                                    t.name,
                                    path.display()
                                );
                            }
                        }
                        total.accepted += report.accepted;
                        total.rejected += report.rejected;
                        let clean = report.is_clean();
                        if let Some(f) = report.first_failure.filter(|_| !clean) {
                            findings.push(format!(
                                "{} seed {seed} iterations {iterations}: {} at {}: {}",
                                t.name,
                                f.kind.as_str(),
                                f.iteration,
                                f.message
                            ));
                        }
                    }
                    eprintln!(
                        "soak {:<40} accepted {:>7} rejected {:>7} findings {}",
                        t.name,
                        total.accepted,
                        total.rejected,
                        findings.len()
                    );
                    rows.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((t.name, total.accepted, total.rejected, findings));
                }
            });
        }
    });
    let rows = rows
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inputs: u64 = rows
        .iter()
        .map(|(_, a, r, _)| u64::from(*a) + u64::from(*r))
        .sum();
    let findings: Vec<&String> = rows.iter().flat_map(|(_, _, _, f)| f).collect();
    eprintln!(
        "soak: {} targets x {seeds} seeds from {first} x {iterations} iterations ({inputs} inputs accepted or rejected), {} findings",
        rows.len(),
        findings.len()
    );
    assert_eq!(rows.len(), chosen.len());
    assert!(
        findings.is_empty(),
        "findings:\n{}",
        findings.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
    );
}

/// The soak's walk: `steps` mutations from the target's fixture (well-formed value mutations for text, byte mutations
/// otherwise; seeded from `seed`), keeping each one that still loads. `Ok(None)` for no step; `Err` with the message
/// when a step panicked (a finding of its own).
fn walk(t: &fuzz_targets::FuzzTarget, seed: u64, steps: usize) -> Result<Option<Vec<u8>>, String> {
    use regia_test_utils::fuzz::{FuzzRng, mutate_bytes, mutate_text_values};
    if steps == 0 {
        return Ok(None);
    }
    let mut rng = FuzzRng::new(seed ^ 0x5741_4C4B_0000_0000);
    let caught = |f: &mut dyn FnMut() -> bool| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|payload| {
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "(non-string panic payload)".to_string())
        })
    };
    match t.input {
        FuzzInput::Text { fixture, load } => {
            let mut base = fixture();
            for _ in 0..steps {
                let next = mutate_text_values(&base, &mut rng);
                if caught(&mut || load(&next).is_ok())? {
                    base = next;
                }
            }
            Ok(Some(base.into_bytes()))
        }
        FuzzInput::Bytes { fixture, load } => {
            let mut base = fixture();
            for _ in 0..steps {
                let next = mutate_bytes(&base, &mut rng);
                if caught(&mut || load(&next).is_ok())? {
                    base = next;
                }
            }
            Ok(Some(base))
        }
    }
}

/// A file one version newer than this build whose body no longer has the current shape names its
/// version: the loader peeks `format_version` before the full parse, so the error says "upgrade the
/// engine", never a RON type error (ADR-8553, `book/changing-a-content-format.md`). The v0.6 formats
/// the freeze review checked; key-track has the same check in `crates/regia_cli/tests/fuzz.rs`.
#[test]
fn newer_foreign_bodies_name_their_version() {
    let mut problems = Vec::new();
    for name in [
        "net.playlist_ron",
        "delivery-contract",
        "asset-ids",
        "ruleset_ron",
    ] {
        let Some(t) = fuzz_targets::target(name) else {
            problems.push(format!("{name}: not registered"));
            continue;
        };
        let FuzzInput::Text { load, .. } = t.input else {
            problems.push(format!("{name}: not a text target"));
            continue;
        };
        let newer = t.format_version + 1;
        for body in [
            r#"entries: "nope""#,
            r#"entries: [(path: 3, id: (), extra: [1, 2])]"#,
            r#"class_name: 7, lod: (), looping: "maybe""#,
            r#"rows: {"a": 1}"#,
        ] {
            let text = format!("(\n    format_version: {newer},\n    {body},\n)\n");
            match load(&text) {
                Ok(_) => problems.push(format!("{name}: v{newer} with a foreign body loaded:\n{text}")),
                Err(e) if !e.contains(&newer.to_string()) || !e.contains("version") => {
                    problems.push(format!(
                        "{name}: v{newer} foreign body: `{e}` does not name the version"
                    ));
                }
                Err(_) => {}
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The stateful net targets of spec 120 § 15.5 (ADR-8468): one connection fed a sequence of packets, a sealed
/// server endpoint after a real handshake (replays, rebinds, forged and oversized datagrams), the shared-port
/// reader, the voice relay and an input bundle at the datagram budget.
const STATEFUL_NET_TARGETS: &[&str] = &[
    "net.connection_sequence",
    "net.connection_sequence.hostile",
    "net.sealed_endpoint_sequence",
    "net.sealed_endpoint_sequence.hostile",
    "net.shared_port_sequence",
    "net.voice_relay_route",
    "net.input_bundle_budget",
    "net.snapshot_sequence",
    // Two honest connections over the impairment model (ADR-8521).
    "net.link_pair_impaired",
];

/// Audit row H6 (ADR-8468): every stateful net target is registered, its fixture holds its model, and the
/// deterministic registry budget (the seeds and iteration count of `every_target_round_trips_what_it_accepts`)
/// finds nothing. Before the reassembly caps (ADR-7981) the `.hostile` fixtures (audit experiment E6) and the
/// mutations of the honest ones were findings: a reassembly gap stood past every legal stall deadline and the link
/// never failed, or an over-long fragment was accepted.
#[test]
fn stateful_net_targets_hold_their_models_within_the_registry_budget() {
    let mut problems = Vec::new();
    for (i, name) in STATEFUL_NET_TARGETS.iter().enumerate() {
        let Some(target) = fuzz_targets::target(name) else {
            problems.push(format!("{name}: not registered"));
            continue;
        };
        if let FuzzInput::Bytes { fixture, load } = target.input
            && let Err(e) = std::panic::catch_unwind(|| load(&fixture()))
                .map_err(|p| p.downcast_ref::<String>().cloned().unwrap_or_default())
                .and_then(|r| r)
        {
            problems.push(format!("{name}: the fixture fails its model: {e}"));
        }
        let report =
            fuzz_targets::run(target, None, 0x5EA1_0000 + i as u64, PROPERTY_ITERATIONS).expect("fixture");
        eprintln!(
            "{name:<40} accepted {:>5} rejected {:>5} findings {}",
            report.accepted,
            report.rejected,
            report.panicked + report.round_trip_failures + report.empty_errors + report.bugs
        );
        if !report.is_clean() {
            problems.push(format!(
                "{name}: {} finding(s), first: {:?}",
                report.panicked + report.round_trip_failures + report.empty_errors + report.bugs,
                report.first_failure.map(|f| (f.iteration, f.message))
            ));
        }
        if report.accepted == 0 {
            problems.push(format!(
                "{name}: no mutation was accepted, so no sequence was explored"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "stateful net targets:\n{}",
        problems.join("\n")
    );
}

#[test]
fn match_flow_input_json_and_rules_match_params_survive_10k() {
    for name in ["match_flow_input_json", "rules_match_params"] {
        let target = fuzz_targets::target(name).unwrap_or_else(|| panic!("missing fuzz target {name}"));
        let report = fuzz_targets::run(target, None, 0x4D52_5EED, 10_000).expect("fixture is text");
        assert!(
            report.is_clean(),
            "{name} panicked or broke a round trip: {report:?}"
        );
    }
}

/// ADR-8694: `.rules.ron` is untrusted content (mods, downloads). 10 000 seeded mutations of the v1
/// fixture through the parse, the check and the resolution never panic and hold the round trip.
#[test]
fn ruleset_ron_fuzz_target_never_panics() {
    let target = fuzz_targets::target("ruleset_ron").expect("the ruleset_ron target is registered");
    assert_eq!(target.format, Some("rules-ron"));
    let report = fuzz_targets::run(target, None, 0x5255_4C45, 10_000).expect("fixture is text");
    assert!(
        report.is_clean(),
        "ruleset_ron panicked or broke a round trip: {report:?}"
    );
}

/// P-17: fuzz the retained v1/v2 layouts and the v3 custom-image field through the
/// registered loader. Parsing/migration must not try to open the referenced image.
#[test]
fn decal_all_versions_fuzz_round_trip_with_custom_texture() {
    let target = fuzz_targets::target("decal").expect("decal target");
    // ADR-8829: `v3-surface.ron` (the footstep surface folded into v3) is fuzzed with the others.
    for (file, case) in [("v1", 1), ("v2", 2), ("v3", 3), ("v3-surface", 4)] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/format-corpus/decal/{file}.ron"));
        let bytes = std::fs::read(path).expect("frozen decal fixture");
        let prefab = regia_vfx::DecalPrefab::from_ron(std::str::from_utf8(&bytes).expect("RON"))
            .expect("fixture parses without image IO");
        assert_eq!(prefab.texture.is_some(), case == 3);
        assert_eq!(prefab.surface.is_some(), case == 4);
        let report = fuzz_targets::run(target, Some(&bytes), 8158 + case, PROPERTY_ITERATIONS)
            .expect("fuzz retained version");
        assert!(report.is_clean(), "{file}: {report:?}");
        assert!(report.accepted > 0, "{file} must exercise round trips");
        assert_eq!(report.accepted + report.rejected, PROPERTY_ITERATIONS);
    }
}
